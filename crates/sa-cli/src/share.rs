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
use sa_orchestrator::share_desk::{
    self, ApprovalReply, Desks, Evict, Forward, PendingApprovals, Progress, Steer,
};
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
/// 交 harness 做到一半的进展最多试几次：丢了一两步不要紧，结果最后还会交。
const PROGRESS_TRIES: u32 = 2;

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

    // 链接里有服务的令牌：批准执行的命令拿不到它。
    sa_tools::hide_env_from_commands(["SA_SHARE_URL".to_string()]);
    // harness 的独立目录放在这边的数据目录里：文件工具碰不到，和服务那边一样。
    let desks = Desks::new(fence.root(), &data_dir.work().join("share-desks"))
        .with_context(|| format!("建不了放独立目录的地方 {}", data_dir.work().display()))?;
    let sharer = Arc::new(Sharer::new(
        fence,
        desks,
        name,
        args.read_only,
        args.commands,
        service,
    )?);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let out = tokio::select! {
            result = sharer.serve(&place) => result,
            () = crate::serve::stop_signal() => Ok(()),
        };
        sharer.shutdown();
        out
    });
    if let Some(tunnel) = tunnel {
        tunnel.close();
    }
    result
}

struct ActiveDesk {
    approvals: PendingApprovals,
    steer_tx: tokio::sync::mpsc::UnboundedSender<Steer>,
}

struct Sharer {
    fence: Fence,
    desks: Desks,
    name: String,
    read_only: bool,
    commands: bool,
    session: String,
    tools: ToolRegistry,
    /// 正在做的活，人点了停止时用它叫停。
    running: Mutex<HashMap<u64, CancellationToken>>,
    /// 正在运行中的远端 harness 工位控制通道（审批回传 + 实时插话）。
    active_desks: Mutex<HashMap<u64, ActiveDesk>>,
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Sharer {
    fn new(
        fence: Fence,
        desks: Desks,
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
            desks,
            name,
            read_only,
            commands,
            session: uuid::Uuid::new_v4().simple().to_string(),
            tools,
            running: Mutex::default(),
            active_desks: Mutex::default(),
            http,
            base: format!("http://{}/api/shares", service.addr),
            token: service.token,
        })
    }

    fn shutdown(&self) {
        let tokens: Vec<CancellationToken> =
            self.running.lock().unwrap().values().cloned().collect();
        for token in tokens {
            token.cancel();
        }
        self.active_desks.lock().unwrap().clear();
        self.desks.evict_all();
    }

    /// 一直取活、做活、交回，直到出了没法继续的错。
    async fn serve(self: &Arc<Self>, place: &str) -> Result<()> {
        let mode = if self.read_only {
            "只读"
        } else if self.commands {
            "可以改、可以执行命令和跑 harness"
        } else {
            "可以改"
        };
        // 文件操作按交来的顺序一件件做（两次改同一个文件不能倒过来）；命令另外跑。
        let (files, mut queued) = tokio::sync::mpsc::unbounded_channel::<(
            ShareCall,
            CancellationToken,
            tokio::sync::oneshot::Sender<()>,
        )>();
        let worker = Arc::clone(self);
        tokio::spawn(async move {
            while let Some((call, cancel, done)) = queued.recv().await {
                let id = call.id;
                worker.handle(call, cancel, None).await;
                worker.running.lock().unwrap().remove(&id);
                let _ = done.send(());
            }
        });
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
                    // 命令可能跑很久，边跑边接着取活（好收到「停下」、审批回传与实时插话）；有命令在跑时文件操作也不等，免得耽误叫停。
                    // 没命令在跑时文件操作做完再取下一件：旧的服务不看 `running`，边做边取会被它当成丢了。
                    match call {
                        Some(call)
                            if matches!(
                                call.tool.as_str(),
                                STOP | share_desk::APPROVAL
                                    | share_desk::STEER
                                    | share_desk::EVICT
                                    | "shell"
                                    | share_desk::RUN
                            ) =>
                        {
                            self.start(call)
                        }
                        Some(call) => {
                            let (busy, cancel) = {
                                let mut running = self.running.lock().unwrap();
                                let busy = !running.is_empty();
                                let cancel = CancellationToken::new();
                                running.insert(call.id, cancel.clone());
                                (busy, cancel)
                            };
                            let (done, finished) = tokio::sync::oneshot::channel();
                            if files.send((call, cancel, done)).is_ok() && !busy {
                                let _ = finished.await;
                            }
                        }
                        None => {}
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
            interactive: self.commands && !self.read_only,
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

    /// 开始执行一条命令 / harness（不等它跑完，接着去取下一件），或立即处理控制消息（停下、审批回传、实时插话）。
    fn start(self: &Arc<Self>, call: ShareCall) {
        if call.tool == STOP {
            if let Some(cancel) = self.running.lock().unwrap().get(&call.id) {
                cancel.cancel();
            }
            return;
        }
        if call.tool == share_desk::APPROVAL {
            if let Ok(reply) = serde_json::from_value::<ApprovalReply>(call.input) {
                let tx = self
                    .active_desks
                    .lock()
                    .unwrap()
                    .get(&call.id)
                    .and_then(|desk| desk.approvals.lock().unwrap().remove(&reply.req_id));
                if let Some(tx) = tx {
                    let _ = tx.send(reply.allowed);
                }
            }
            return;
        }
        if call.tool == share_desk::STEER {
            if let Ok(steer) = serde_json::from_value::<Steer>(call.input) {
                let sent = self
                    .active_desks
                    .lock()
                    .unwrap()
                    .get(&call.id)
                    .is_some_and(|desk| desk.steer_tx.send(steer.clone()).is_ok());
                if !sent {
                    // 目标工位已结束或未启用插话：回发 rejected 防止服务端继续等待确认。
                    let this = Arc::clone(self);
                    let id = call.id;
                    tokio::spawn(async move {
                        let text = serde_json::to_string(&Progress::SteerAck {
                            steer_id: steer.steer_id,
                            accepted: false,
                        })
                        .expect("只有基本类型");
                        let _ = this
                            .send(&this.reply(id, true, text, true), REPLY_TRIES)
                            .await;
                    });
                }
            }
            return;
        }
        let cancel = CancellationToken::new();
        self.running.lock().unwrap().insert(call.id, cancel.clone());
        // 在 spawn 前同步注册远端控制表，确保紧随其后的 `_approval` / `_steer` 不会竞态扑空。
        let desk_runtime = if call.tool == share_desk::RUN {
            let approvals: PendingApprovals = Arc::default();
            let (steer_tx, steer_rx) = tokio::sync::mpsc::unbounded_channel();
            self.active_desks.lock().unwrap().insert(
                call.id,
                ActiveDesk {
                    approvals: Arc::clone(&approvals),
                    steer_tx,
                },
            );
            Some((approvals, steer_rx))
        } else {
            None
        };
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let id = call.id;
            this.handle(call, cancel, desk_runtime).await;
            this.active_desks.lock().unwrap().remove(&id);
            this.running.lock().unwrap().remove(&id);
        });
    }

    /// 做一件活，把结果交回去。交不回去就重试几次；还不行就算了，服务那边会当它丢了。
    async fn handle(
        &self,
        call: ShareCall,
        cancel: CancellationToken,
        desk_runtime: Option<(
            PendingApprovals,
            tokio::sync::mpsc::UnboundedReceiver<Steer>,
        )>,
    ) {
        let approvals_ref = desk_runtime.as_ref().map(|(a, _)| Arc::clone(a));
        // harness 做到一半的进展按顺序先交，最后再交结果。
        let (progress, mut steps) = tokio::sync::mpsc::unbounded_channel::<Progress>();
        let forward = async {
            while let Some(step) = steps.recv().await {
                let critical = matches!(
                    step,
                    Progress::ApprovalRequest { .. } | Progress::SteerAck { .. }
                );
                let pending_req_id = match &step {
                    Progress::ApprovalRequest { req_id, .. } => Some(req_id.clone()),
                    _ => None,
                };
                let text = serde_json::to_string(&step).expect("可序列化");
                let tries = if critical {
                    REPLY_TRIES
                } else {
                    PROGRESS_TRIES
                };
                let delivered = self
                    .send(&self.reply(call.id, true, text, true), tries)
                    .await;
                if !delivered {
                    // 审批请求未能送达服务端：立刻本地拒绝，避免远端 harness 死等超时。
                    if let (Some(req_id), Some(approvals)) = (pending_req_id, &approvals_ref) {
                        if let Some(tx) = approvals.lock().unwrap().remove(&req_id) {
                            let _ = tx.send(false);
                        }
                    }
                }
            }
        };
        let (result, ()) = tokio::join!(
            self.run_tool(&call, cancel, progress, desk_runtime),
            forward
        );
        let (ok, text) = match result {
            Ok(text) => (true, text),
            Err(text) => (false, text),
        };
        let _ = self
            .send(&self.reply(call.id, ok, text, false), REPLY_TRIES)
            .await;
    }

    fn reply(&self, id: u64, ok: bool, text: String, progress: bool) -> ShareReply {
        ShareReply {
            name: self.name.clone(),
            session: self.session.clone(),
            id,
            ok,
            text,
            progress,
        }
    }

    async fn send(&self, reply: &ShareReply, tries: u32) -> bool {
        let mut retry = RETRY_FIRST;
        for _ in 0..tries {
            let sent = self
                .http
                .post(format!("{}/reply", self.base))
                .bearer_auth(&self.token)
                .timeout(REPLY_TIMEOUT)
                .json(reply)
                .send()
                .await;
            match sent {
                Ok(response) if response.status().is_success() => return true,
                Ok(response) if response.status() == StatusCode::CONFLICT => {
                    // 交晚了（被别的顶替了）：结果没人要了。还能不能接着共享看下一次取活。
                    eprintln!("结果没交上：{}", error_text(response).await);
                    return false;
                }
                Ok(response) => eprintln!("交结果时服务回了 {}，重试……", response.status()),
                Err(error) => eprintln!("交结果失败（{error}），重试……"),
            }
            tokio::time::sleep(retry).await;
            retry = (retry * 2).min(RETRY_MAX);
        }
        false
    }

    async fn run_tool(
        &self,
        call: &ShareCall,
        cancel: CancellationToken,
        progress: tokio::sync::mpsc::UnboundedSender<Progress>,
        desk_runtime: Option<(
            PendingApprovals,
            tokio::sync::mpsc::UnboundedReceiver<Steer>,
        )>,
    ) -> Result<String, String> {
        // 排着队时就被叫停了：不做了。
        if cancel.is_cancelled() {
            return Err("已停止，没有执行。".into());
        }
        if call.tool.starts_with("_desk") || call.tool == share_desk::RUN {
            return self.desk_work(call, cancel, progress, desk_runtime).await;
        }
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
                Ok((real, true)) if sa_tools::in_git_dir(&real) && !self.commands => {
                    return Err(format!(
                        "路径 {path} 是 git 内部文件（里面的配置能让 git 执行命令），共享目录未开启 --commands 时不能改。"
                    ));
                }
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

impl Sharer {
    /// harness 的独立目录：开、在里面跑 harness、带回/丢掉或回收常驻会话池。
    async fn desk_work(
        &self,
        call: &ShareCall,
        cancel: CancellationToken,
        progress: tokio::sync::mpsc::UnboundedSender<Progress>,
        desk_runtime: Option<(
            PendingApprovals,
            tokio::sync::mpsc::UnboundedReceiver<Steer>,
        )>,
    ) -> Result<String, String> {
        if self.read_only {
            return Err("这个共享目录是只读的，harness 不能在这里干活。".into());
        }
        if share_desk::needs_commands(&call.tool) && !self.commands {
            return Err("这台电脑共享时没加 --commands，harness 不能在这里跑。".into());
        }
        let input = call.input.clone();
        let unreadable = |e: serde_json::Error| {
            format!("看不懂服务交来的活（{e}），sa share 和服务的版本可能对不上")
        };
        match call.tool.as_str() {
            share_desk::OPEN => {
                let open = serde_json::from_value(input).map_err(unreadable)?;
                let workdir = self.desks.open(open).await?;
                println!("harness 的独立目录：{}", workdir.path);
                Ok(serde_json::to_string(&workdir).expect("只有字符串"))
            }
            share_desk::RUN => {
                let job: share_desk::Run = serde_json::from_value(input).map_err(unreadable)?;
                println!("{} 开始在 {} 里干活", job.harness.label(), job.workdir.path);
                let (observer, steer_rx) = match (job.interactive, desk_runtime) {
                    (true, Some((approvals, steer_rx))) => (
                        Forward::interactive(
                            progress.clone(),
                            approvals,
                            cancel.clone(),
                            job.approval_timeout_ms,
                        ),
                        Some(steer_rx),
                    ),
                    _ => (Forward::new(progress.clone()), None),
                };
                let ran = self
                    .desks
                    .run_interactive(job, steer_rx, &progress, &observer, &cancel)
                    .await;
                Ok(serde_json::to_string(&ran).expect("只有字符串"))
            }
            share_desk::CLOSE => {
                let close: share_desk::Close = serde_json::from_value(input).map_err(unreadable)?;
                let what = if close.accept { "带回" } else { "丢掉" };
                self.desks.close(close).await?;
                println!("harness 的改动已{what}");
                Ok(String::new())
            }
            share_desk::EVICT => {
                let evict: Evict = serde_json::from_value(input).map_err(unreadable)?;
                self.desks.evict_scope(&evict.scope);
                Ok(String::new())
            }
            other => Err(format!("共享目录这边不能用 {other}。")),
        }
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
