//! `sa share`：把这台电脑上的一个目录共享给 Myriad 服务（比如云端服务器）。
//!
//! 服务那边的 Bot 要看、要搜、要改这个目录里的文件时，这边来取活、在本机执行、把结果交回去。
//! 文件不会整个上传，传的只有每次要看的内容和要写的改动。文件工具只开放这一个目录，目录外面的碰不到。
//! 加了 `--commands` 的话，Bot 还能在这个目录里执行命令（每条都要在房间里批准），点停止会连同它启动的进程一起结束。
//! 连服务器走 SSH 隧道（和 `sa connect` 一样），断了自动重连；那边要用这个目录的步骤会等这边连回来。

use crate::paths::DataDir;
use crate::remote::{self, RemoteService, SshTarget, Tunnel, TunnelStop, RETRY_FIRST, RETRY_MAX};
use anyhow::{bail, Context, Result};
use reqwest::StatusCode;
use sa_orchestrator::shares::{share_name, READ_ONLY_TOOLS, STOP};
use sa_protocol::{ShareCall, SharePoll, ShareReply};
use sa_tools::{local_shell, Fence, ToolCtx, ToolRegistry};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// 一次取活最多等这么久（服务那边 25 秒没活就回空）。
const POLL_TIMEOUT: Duration = Duration::from_secs(40);
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);
/// 交结果失败时最多重试几次。
const REPLY_TRIES: u32 = 5;

#[derive(clap::Args)]
pub struct ShareArgs {
    /// 要共享的目录
    dir: PathBuf,
    /// 共享的名字，房间里按它来选（默认用目录名）
    #[arg(long)]
    name: Option<String>,
    /// 只能看和搜，不能改
    #[arg(long)]
    read_only: bool,
    /// 也让 Bot 在这个目录里执行命令（编译、测试这类），每条都要在房间里批准。
    /// 命令和你在终端里跑的一样，不只碰得到这个目录
    #[arg(long, conflicts_with = "read_only")]
    commands: bool,
    /// 共享给哪台服务器：SSH 地址（`用户@主机` 或 ~/.ssh/config 里的 Host 名）。不写就共享给这台电脑上的服务
    /// 给了它就不看 --url / SA_SHARE_URL
    #[arg(long)]
    server: Option<String>,
    /// 服务器的 SSH 端口
    #[arg(short = 'p', long, requires = "server")]
    ssh_port: Option<u16>,
    /// 直接给服务的地址（sa url 打印的那一行），已经自己开好隧道时用。
    /// 链接里有令牌，放在环境变量 SA_SHARE_URL 里就不会出现在进程列表里
    #[arg(long, env = "SA_SHARE_URL", hide_env_values = true)]
    url: Option<String>,
}

pub fn run(data_dir: &DataDir, args: ShareArgs) -> Result<()> {
    crate::app::init_logging(tracing::Level::WARN);
    // 数据目录不存在的话围栏认不出它：先建好，以后放进去的令牌、数据库才挡得住。
    data_dir
        .create()
        .with_context(|| format!("建不了数据目录 {}", data_dir.root().display()))?;
    let fence = Fence::new(&args.dir, &[data_dir.root().to_path_buf()])
        .with_context(|| format!("目录 {} 打不开", args.dir.display()))?;
    let name = match &args.name {
        Some(name) => name.clone(),
        None => fence
            .root()
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "共享目录".into()),
    };
    let name = share_name(&name).map_err(anyhow::Error::msg)?;

    let stop = TunnelStop::default();
    let (service, tunnel, place) = match (&args.server, &args.url) {
        (Some(server), _) => {
            let target = SshTarget::parse(server, args.ssh_port)?;
            println!("正在连接 {target}……");
            let tunnel = Tunnel::open(&target, None, &stop)?;
            let service = RemoteService::parse(tunnel.url()).context("隧道地址不对")?;
            (service, Some(tunnel), target.to_string())
        }
        (None, Some(url)) => {
            let service = RemoteService::parse(url)
                .context("地址不对，要的是 sa url 打印的那一行：http://地址/auth?token=令牌")?;
            // 令牌和文件内容走明文 HTTP：只连本机这头（自己开好的隧道），别的机器用 --server。
            if !service.addr.ip().is_loopback() {
                bail!("--url 只能是本机地址（127.0.0.1）；连别的机器请用 --server 走 SSH");
            }
            let place = service.addr.to_string();
            (service, None, place)
        }
        (None, None) => {
            let service = remote::local_service(data_dir)?;
            (service, None, "这台电脑上的服务".into())
        }
    };

    let sharer = Arc::new(Sharer::new(
        fence,
        name,
        args.read_only,
        args.commands,
        service,
    )?);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        tokio::select! {
            result = sharer.serve(&place) => result,
            () = crate::serve::stop_signal() => Ok(()),
        }
    });
    if let Some(tunnel) = tunnel {
        tunnel.close();
    }
    result
}

struct Sharer {
    fence: Fence,
    name: String,
    read_only: bool,
    commands: bool,
    session: String,
    tools: ToolRegistry,
    /// 正在做的活，人点了停止时用它叫停。
    running: Mutex<HashMap<u64, CancellationToken>>,
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Sharer {
    fn new(
        fence: Fence,
        name: String,
        read_only: bool,
        commands: bool,
        service: RemoteService,
    ) -> Result<Self> {
        let commands = commands && !read_only;
        let standard = ToolRegistry::standard();
        let mut tools = ToolRegistry::new();
        for tool in standard.iter() {
            let tool_name = tool.name();
            let allowed = READ_ONLY_TOOLS.contains(&tool_name)
                || (!read_only && matches!(tool_name, "write" | "edit"))
                || (commands && tool_name == "shell");
            if allowed {
                tools.register_shared(tool.clone());
            }
        }
        let _ = rustls::crypto::ring::default_provider().install_default();
        // 连的是本机端口（隧道的这头）：不走系统代理。
        let http = reqwest::Client::builder().no_proxy().build()?;
        Ok(Self {
            fence,
            name,
            read_only,
            commands,
            session: uuid::Uuid::new_v4().simple().to_string(),
            tools,
            running: Mutex::default(),
            http,
            base: format!("http://{}/api/shares", service.addr),
            token: service.token,
        })
    }

    /// 一直取活、做活、交回，直到出了没法继续的错。
    async fn serve(self: &Arc<Self>, place: &str) -> Result<()> {
        let mode = if self.read_only {
            "只读"
        } else if self.commands {
            "可以改、可以执行命令"
        } else {
            "可以改"
        };
        let mut connected = false;
        let mut retry = RETRY_FIRST;
        loop {
            match self.poll().await {
                Ok(call) => {
                    if !connected {
                        println!(
                            "已把 {} 共享给 {place}，名字「{}」，{mode}。在房间的「成员」页里选它当工作目录。按 Ctrl+C 停止共享。",
                            self.fence.root_display(),
                            self.name
                        );
                        connected = true;
                    }
                    retry = RETRY_FIRST;
                    if let Some(call) = call {
                        self.start(call);
                    }
                }
                Err(Poll::Taken(reason)) => bail!("{reason}"),
                Err(Poll::Failed(error)) => {
                    if connected {
                        eprintln!("和服务断开了（{error}），{} 秒后重连……", retry.as_secs());
                        connected = false;
                    } else {
                        eprintln!("连不上服务（{error}），{} 秒后重试……", retry.as_secs());
                    }
                    tokio::time::sleep(retry).await;
                    retry = (retry * 2).min(RETRY_MAX);
                }
            }
        }
    }

    async fn poll(&self) -> Result<Option<ShareCall>, Poll> {
        let hello = SharePoll {
            name: self.name.clone(),
            session: self.session.clone(),
            root: self.fence.root_display(),
            read_only: self.read_only,
            shell: self.commands.then(|| local_shell().to_string()),
            running: self.running.lock().unwrap().keys().copied().collect(),
        };
        let response = self
            .http
            .post(format!("{}/poll", self.base))
            .bearer_auth(&self.token)
            .timeout(POLL_TIMEOUT)
            .json(&hello)
            .send()
            .await
            .map_err(|e| Poll::Failed(e.to_string()))?;
        match response.status() {
            StatusCode::NO_CONTENT => Ok(None),
            StatusCode::OK => response
                .json()
                .await
                .map(Some)
                .map_err(|e| Poll::Failed(e.to_string())),
            StatusCode::CONFLICT => Err(Poll::Taken(error_text(response).await)),
            StatusCode::UNAUTHORIZED => Err(Poll::Taken("令牌不对，服务不认".into())),
            StatusCode::NOT_FOUND => Err(Poll::Taken(
                "服务那边的 Myriad 太旧，还不支持共享目录，请先升级".into(),
            )),
            status => Err(Poll::Failed(format!("服务回了 {status}"))),
        }
    }

    /// 开始做一件活（不等它做完，接着去取下一件），或者叫停正在做的那件。
    fn start(self: &Arc<Self>, call: ShareCall) {
        if call.tool == STOP {
            if let Some(cancel) = self.running.lock().unwrap().get(&call.id) {
                cancel.cancel();
            }
            return;
        }
        let cancel = CancellationToken::new();
        self.running.lock().unwrap().insert(call.id, cancel.clone());
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let id = call.id;
            this.handle(call, cancel).await;
            this.running.lock().unwrap().remove(&id);
        });
    }

    /// 做一件活，把结果交回去。交不回去就重试几次；还不行就算了，服务那边会当它丢了。
    async fn handle(&self, call: ShareCall, cancel: CancellationToken) {
        let (ok, text) = match self.run_tool(&call, cancel).await {
            Ok(text) => (true, text),
            Err(text) => (false, text),
        };
        let reply = ShareReply {
            name: self.name.clone(),
            session: self.session.clone(),
            id: call.id,
            ok,
            text,
        };
        let mut retry = RETRY_FIRST;
        for _ in 0..REPLY_TRIES {
            let sent = self
                .http
                .post(format!("{}/reply", self.base))
                .bearer_auth(&self.token)
                .timeout(REPLY_TIMEOUT)
                .json(&reply)
                .send()
                .await;
            match sent {
                Ok(response) if response.status().is_success() => return,
                Ok(response) if response.status() == StatusCode::CONFLICT => {
                    // 交晚了（被别的顶替了）：结果没人要了。还能不能接着共享看下一次取活。
                    eprintln!("结果没交上：{}", error_text(response).await);
                    return;
                }
                Ok(response) => eprintln!("交结果时服务回了 {}，重试……", response.status()),
                Err(error) => eprintln!("交结果失败（{error}），重试……"),
            }
            tokio::time::sleep(retry).await;
            retry = (retry * 2).min(RETRY_MAX);
        }
    }

    async fn run_tool(
        &self,
        call: &ShareCall,
        cancel: CancellationToken,
    ) -> Result<String, String> {
        let Some(tool) = self.tools.get(&call.tool) else {
            return Err(if self.read_only {
                format!("这个共享目录是只读的，不能用 {}。", call.tool)
            } else {
                format!("共享目录这边不能用 {}。", call.tool)
            });
        };
        let tool = Arc::clone(tool);
        let input = call.input.clone();
        let title = tool.title(&input);
        // 本机的工具在目录外面会先问人再写；这边没人可问，目录外面的一律不写。
        // 命令在服务那边已经一条条问过人了，在这个目录里执行。
        if tool.name() != "shell" && !tool.is_read_only(&input) {
            let path = input["path"].as_str().unwrap_or_default();
            match self.fence.resolve_for_write(path) {
                Ok((_, true)) => {}
                Ok((_, false)) => {
                    return Err(format!("路径 {path} 在共享目录之外，不能改。"));
                }
                Err(error) => return Err(error.0),
            }
        }
        println!("{title}");
        let mut ctx = ToolCtx::new(self.fence.clone());
        ctx.cancel = cancel;
        tool.call(input, &ctx).await.map_err(|error| error.0)
    }
}

enum Poll {
    /// 网络、隧道出了问题：过一会儿再试。
    Failed(String),
    /// 没法继续：被顶替了、令牌不对。
    Taken(String),
}

async fn error_text(response: reqwest::Response) -> String {
    #[derive(serde::Deserialize)]
    struct Body {
        message: String,
    }
    match response.json::<Body>().await {
        Ok(body) => body.message,
        Err(_) => "服务拒绝了".into(),
    }
}
