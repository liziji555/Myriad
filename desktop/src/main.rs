//! Myriad 的桌面外壳。
//!
//! 它不是另一套程序：在本进程里起和 `sa serve` 一样的本机服务，再开一个窗口去看它。
//! 界面、数据目录、配置都和命令行版是同一份，所以两边不能同时开着。
//!
//! 打开就是本机的主界面：没选过工作目录就用「文稿」下的 `Myriad` 文件夹。
//! 要连服务器，在主界面里点左栏底下的连接按钮，开「连接」面板填地址。连的时候主界面照常用，
//! 连上了窗口才换到隧道这头的地址（和 `sa connect` 一样），没连上就留在本机、在面板里说原因。
//! 连着服务器时本机的服务不停，随时能换回来；关掉窗口只断开隧道，服务器上的任务照常跑。
//!
//! 主窗口没有系统的标题栏和菜单栏：窗口按钮由页面自己画，顶栏就是标题栏。
//! 主窗口里的页面只能调「连接」面板和窗口按钮要用的那几个命令（[`from_main`]），别的桌面能力一概拿不到。
//! 内置的启动页只在本机服务起不来时出现：那时没有主界面可看。

// 发布版在 Windows 上不带黑色的终端窗口。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use clap::Parser;
use sa_cli::{DataDir, LoginDenied, RunArgs, SshTarget, Tunnel, TunnelStop};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Mutex};
use std::time::Duration;
use tauri::menu::{Menu, MenuBuilder, SubmenuBuilder};
use tauri::webview::{DownloadEvent, NewWindowResponse};
use tauri::{AppHandle, Manager, RunEvent, Url, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tauri_plugin_opener::OpenerExt;

/// 关窗口之后最多等服务收尾这么久（让 Bot 停笔、把没写完的回复标成「中断」）。
const SHUTDOWN_WAIT: Duration = Duration::from_secs(10);

#[derive(Parser, Clone)]
#[command(name = "superagent-desktop", version, about = "Myriad 桌面版")]
struct Args {
    /// 数据目录（配置、数据库、会话记录都在这里），和 sa 用的是同一个
    #[arg(long, env = "SA_DATA_DIR", value_name = "目录")]
    data_dir: Option<PathBuf>,
    /// Bot 能读取的工作目录。不给就用上次选的；没选过就用「文稿」下的「Myriad」文件夹
    #[arg(long, value_name = "目录")]
    workdir: Option<PathBuf>,
    /// 用不联网的假模型，不需要密钥（用来试流程）
    #[arg(long)]
    mock: bool,
    /// 监听端口，覆盖 config.toml 里的设置
    #[arg(long)]
    port: Option<u16>,
    /// 打开后用 SSH 连这台服务器上的 Myriad（写成 用户@主机）。不给就用上次连的
    #[arg(long, value_name = "用户@主机")]
    server: Option<String>,
    /// 服务器的 SSH 端口
    #[arg(long, requires = "server")]
    ssh_port: Option<u16>,
}

/// 启动页（`ui/index.html`）的窗口名。
const START: &str = "start";
/// 主窗口的名字。
const MAIN: &str = "main";

/// 默认工作目录：在「文稿」（没有就在主目录）下面。
const DEFAULT_WORKDIR: &str = "Myriad";

/// 正在跑的服务：怎么让它停，以及它停好了没有。
struct Server {
    stop: tokio::sync::oneshot::Sender<()>,
    stopped: mpsc::Receiver<()>,
}

/// 一次还没连完的连接。
/// 上次没连上（或者「测试连接」没通过）的那一台。
#[derive(Clone)]
struct Failure {
    target: Option<SshTarget>,
    server: String,
    reason: String,
    /// 服务器不认密钥：面板里给密码框，密码就是交给 `target` 的。
    password: bool,
}

impl Failure {
    fn of(target: &SshTarget, error: &anyhow::Error) -> Self {
        Self {
            target: Some(target.clone()),
            server: target.to_string(),
            reason: format!("{error:#}"),
            password: error.downcast_ref::<LoginDenied>().is_some(),
        }
    }

    fn json(&self) -> Value {
        json!({ "server": self.server, "reason": self.reason, "password": self.password })
    }
}

struct Attempt {
    id: u64,
    target: SshTarget,
    stop: TunnelStop,
}

/// 到服务器的连接：连着哪台、正在连哪台、上次为什么没连上。
#[derive(Default)]
struct Link {
    tunnel: Option<Tunnel>,
    /// 主窗口现在看的是这台服务器。没有就是本机。
    target: Option<SshTarget>,
    attempt: Option<Attempt>,
    /// 正在「测试连接」的那一次。
    probe: Option<(u64, TunnelStop)>,
    attempts: u64,
    failed: Option<Failure>,
}

struct Shell {
    args: Args,
    data_dir: DataDir,
    /// 本机服务读写的工作目录。
    serving: Mutex<Option<PathBuf>>,
    server: Mutex<Option<Server>>,
    /// 本机服务的地址，带令牌。
    local_url: Mutex<Option<Url>>,
    link: Mutex<Link>,
    /// 主窗口里能开的页面是哪几个来源：本机服务的，和隧道这头的。只有它们能调命令。
    homes: Mutex<Vec<String>>,
}

impl Shell {
    /// 断开到服务器的隧道，让本机服务收尾并等它停下。没在跑、已经停过都没关系。
    fn stop_server(&self) {
        let (attempt, tunnel) = {
            let mut link = self.link.lock().unwrap();
            if let Some((_, probe)) = link.probe.take() {
                probe.close();
            }
            (link.attempt.take(), link.tunnel.take())
        };
        if let Some(attempt) = attempt {
            attempt.stop.close();
        }
        if let Some(tunnel) = tunnel {
            tunnel.close();
        }
        let Some(server) = self.server.lock().unwrap().take() else {
            return;
        };
        let _ = server.stop.send(());
        if server.stopped.recv_timeout(SHUTDOWN_WAIT).is_err() {
            tracing::warn!("服务没能在 {SHUTDOWN_WAIT:?} 内停好，直接退出");
        }
    }

    /// 记着上次选的工作目录、服务器的文件。
    fn remembered(&self) -> PathBuf {
        self.data_dir.root().join("desktop.json")
    }

    /// 启动参数里定死了的（`--workdir`、`--server`）：面板和菜单里就不让换那一样。
    fn pinned(&self) -> Option<&'static str> {
        if self.args.server.is_some() {
            Some("server")
        } else if self.args.workdir.is_some() {
            Some("workdir")
        } else {
            None
        }
    }

    /// 本机用哪个工作目录：命令行给的 > 上次选的（后来被删了、挪了就当没选过）。
    fn chosen_workdir(&self) -> Option<PathBuf> {
        self.args.workdir.clone().or_else(|| {
            read_remembered(&self.remembered())
                .workdir
                .filter(|dir| dir.is_dir())
        })
    }

    /// 打开后要不要接着连服务器：命令行给的 > 上次连着的。`--workdir` 是说就用本机。
    fn wanted_server(&self) -> Option<Result<SshTarget, String>> {
        if let Some(host) = &self.args.server {
            return Some(
                SshTarget::parse(host, self.args.ssh_port).map_err(|error| format!("{error:#}")),
            );
        }
        if self.args.workdir.is_some() {
            return None;
        }
        let remembered = read_remembered(&self.remembered());
        remembered.server.filter(|_| remembered.use_server).map(Ok)
    }

    /// 页面要连的是哪台。用 `--server` 启动的只连那一台。
    fn target(
        &self,
        host: &str,
        port: Option<u16>,
        key: Option<String>,
    ) -> Result<SshTarget, String> {
        match self.pinned() {
            Some("workdir") => Err("启动时用 --workdir 定死了用本机，换不了".into()),
            Some(_) => self
                .wanted_server()
                .unwrap_or_else(|| Err("没有给服务器地址".into())),
            None => {
                let key = key
                    .map(|key| key.trim().to_string())
                    .filter(|key| !key.is_empty());
                SshTarget::parse(host, port)
                    .map(|target| target.with_identity(key.map(PathBuf::from)))
                    .map_err(|error| format!("{error:#}"))
            }
        }
    }
}

fn main() {
    sa_cli::answer_askpass();
    let args = Args::parse();
    sa_cli::init_logging(tracing::Level::INFO);
    let data_dir = match DataDir::resolve(args.data_dir.clone()) {
        Ok(data_dir) => data_dir,
        Err(error) => {
            eprintln!("错误：{error:#}");
            std::process::exit(1);
        }
    };
    let shell = Shell {
        args,
        data_dir,
        serving: Mutex::new(None),
        server: Mutex::new(None),
        local_url: Mutex::new(None),
        link: Mutex::new(Link::default()),
        homes: Mutex::new(Vec::new()),
    };

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(shell)
        .invoke_handler(tauri::generate_handler![
            desktop_state,
            connect_server,
            test_server,
            install_key,
            cancel_connect,
            use_local,
            pick_key,
            pick_workdir,
            open_data_dir,
            window_chrome,
            retry
        ])
        .setup(|app| {
            app.handle().on_menu_event(on_menu);
            // 等服务起来要等，不能占着主线程。
            let handle = app.handle().clone();
            std::thread::spawn(move || start(&handle));
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("桌面外壳启动失败");
    app.run(|handle, event| {
        if let RunEvent::Exit = event {
            handle.state::<Shell>().stop_server();
        }
    });
}

/// 起本机服务、开主窗口；上次连着服务器的，接着在后台连它。
/// 本机服务起不来就开启动页说明原因，而不是悄悄退出。
fn start(handle: &AppHandle) {
    let shell = handle.state::<Shell>();
    let workdir = match shell.chosen_workdir() {
        Some(workdir) => Ok(workdir),
        None => default_workdir(handle).inspect(|workdir| {
            remember(&shell, |remembered| {
                remembered.workdir = Some(workdir.clone())
            });
        }),
    };
    let opened = match workdir {
        Ok(workdir) => start_local(handle, &shell, workdir),
        Err(reason) => open_failed(handle, &shell, &reason),
    };
    if let Err(error) = opened {
        tracing::error!("窗口打不开：{error}");
        handle.exit(1);
        return;
    }
    if handle.get_webview_window(MAIN).is_none() {
        return;
    }
    match shell.wanted_server() {
        Some(Ok(target)) => drop(connect(handle, target)),
        Some(Err(reason)) => {
            let server = shell.args.server.clone().unwrap_or_default();
            shell.link.lock().unwrap().failed = Some(Failure {
                target: None,
                server,
                reason,
                password: false,
            });
            notify(handle);
        }
        None => {}
    }
}

fn start_local(handle: &AppHandle, shell: &Shell, workdir: PathBuf) -> tauri::Result<()> {
    let (ready, started) = mpsc::channel::<Result<String, String>>();
    let (stop, stop_asked) = tokio::sync::oneshot::channel::<()>();
    let (done, stopped) = mpsc::channel::<()>();
    let run = RunArgs {
        workdir: Some(workdir.clone()),
        mock: shell.args.mock,
    };
    let (data_dir, port) = (shell.data_dir.clone(), shell.args.port);
    std::thread::spawn(move || {
        let listening = ready.clone();
        let result = sa_cli::serve_until(
            &data_dir,
            &run,
            port,
            move |url| drop(listening.send(Ok(url.to_string()))),
            async move { drop(stop_asked.await) },
        );
        if let Err(error) = result {
            tracing::error!("服务停下了：{error:#}");
            let _ = ready.send(Err(format!("{error:#}")));
        }
        let _ = done.send(());
    });
    *shell.server.lock().unwrap() = Some(Server { stop, stopped });

    match started.recv() {
        Ok(Ok(url)) => {
            let url: Url = url.parse().map_err(tauri::Error::InvalidUrl)?;
            *shell.serving.lock().unwrap() = Some(workdir);
            *shell.local_url.lock().unwrap() = Some(url.clone());
            open_main(handle, shell, url)
        }
        Ok(Err(reason)) => open_failed(handle, shell, &reason),
        Err(_) => open_failed(handle, shell, "服务还没开始就退出了"),
    }
}

/// 连 `target`：连的时候主窗口照常用，连上了才换到隧道这头的页面并记下它。
/// 没连上就留在原来的页面，原因记在 [`Link::failed`] 里，也交回给调用的人。
/// 途中又连了别的、或者取消了，这一次就作废。
fn connect(handle: &AppHandle, target: SshTarget) -> Result<(), Value> {
    let shell = handle.state::<Shell>();
    let stop = TunnelStop::default();
    let (id, replaced) = {
        let mut link = shell.link.lock().unwrap();
        link.attempts += 1;
        link.failed = None;
        let attempt = Attempt {
            id: link.attempts,
            target: target.clone(),
            stop: stop.clone(),
        };
        (attempt.id, link.attempt.replace(attempt))
    };
    if let Some(replaced) = replaced {
        replaced.stop.close();
    }
    notify(handle);

    let opened = Tunnel::open(&target, None, &stop);
    let mut link = shell.link.lock().unwrap();
    if link.attempt.as_ref().map(|attempt| attempt.id) != Some(id) {
        return Err(
            json!({ "server": target.to_string(), "reason": "已取消连接", "cancelled": true }),
        );
    }
    link.attempt = None;
    let failure = match opened.and_then(|tunnel| Ok((tunnel.url().parse::<Url>()?, tunnel))) {
        Ok((url, tunnel)) => {
            let old = link.tunnel.replace(tunnel);
            link.target = Some(target.clone());
            drop(link);
            // 用 --server 启动的只是这一次，不改记着的。
            if shell.pinned().is_none() {
                remember(&shell, |remembered| {
                    remembered.server = Some(target.clone());
                    remembered.use_server = true;
                });
            }
            show(handle, &shell, url, &format!("Myriad · {}", target.host));
            // 页面换过去之后再断开原来那台。
            drop(old);
            return Ok(());
        }
        Err(error) => Failure::of(&target, &error),
    };
    link.failed = Some(failure.clone());
    drop(link);
    notify(handle);
    Err(failure.json())
}

/// 主窗口换到 `url`。要读 Cookie，别在主线程上调（Windows 上会卡死）。
fn show(handle: &AppHandle, shell: &Shell, url: Url, title: &str) {
    let Some(window) = handle.get_webview_window(MAIN) else {
        return;
    };
    {
        let mut homes = shell.homes.lock().unwrap();
        let local = shell.local_url.lock().unwrap().as_ref().map(origin);
        *homes = local.into_iter().chain([origin(&url)]).collect();
    }
    // Cookie 不分端口：本机服务和隧道这头都是 127.0.0.1，不清掉的话这一头的登录令牌会被带给另一头。
    // 两个地址自己都带着令牌，换过去会重新登录。
    for cookie in window.cookies().unwrap_or_default() {
        if let Err(error) = window.delete_cookie(cookie) {
            tracing::warn!("没清掉上一个页面的登录：{error}");
        }
    }
    // 换过去的页面未必会自己画窗口按钮（服务器上的 sa 可能是老版本）：先把系统的标题栏和菜单还回来，
    // 新页面画了按钮再来收走（[`window_chrome`]）。
    let framed = menu(handle)
        .and_then(|menu| window.set_menu(menu).map(drop))
        .and_then(|()| window.set_decorations(true));
    if let Err(error) = framed {
        tracing::warn!("没能把标题栏还回来：{error}");
    }
    if let Err(error) = window.navigate(url).and_then(|()| window.set_title(title)) {
        tracing::warn!("主窗口换不了页面：{error}");
    }
}

fn origin(url: &Url) -> String {
    url.origin().ascii_serialization()
}

/// 告诉主窗口里的页面：连接的状态变了，再问一次 [`desktop_state`]。
fn notify(handle: &AppHandle) {
    if let Some(window) = handle.get_webview_window(MAIN) {
        let _ = window.eval("window.dispatchEvent(new Event('sa-desktop'))");
    }
}

/// 「文稿/Myriad」，没有就建一个。
fn default_workdir(handle: &AppHandle) -> Result<PathBuf, String> {
    let path = handle.path();
    let parent = path
        .document_dir()
        .or_else(|_| path.home_dir())
        .map_err(|error| format!("找不到「文稿」文件夹和主目录：{error}"))?;
    let dir = parent.join(DEFAULT_WORKDIR);
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("没能建好默认工作目录 {}：{error}", dir.display()))?;
    Ok(dir)
}

fn pick_folder(handle: &AppHandle) -> tauri_plugin_dialog::FileDialogBuilder<tauri::Wry> {
    handle
        .dialog()
        .file()
        .set_title("选择 Bot 能读写的工作目录")
}

/// `desktop.json` 里记着的东西。老版本只记了 `workdir`，那就是用本机。
#[derive(Default, Debug, PartialEq)]
struct Remembered {
    workdir: Option<PathBuf>,
    server: Option<SshTarget>,
    use_server: bool,
}

fn read_remembered(path: &Path) -> Remembered {
    let Some(value) = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
    else {
        return Remembered::default();
    };
    let port = value
        .get("sshPort")
        .and_then(serde_json::Value::as_u64)
        .and_then(|port| u16::try_from(port).ok());
    let key = value
        .get("sshKey")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from);
    Remembered {
        workdir: value
            .get("workdir")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from),
        server: value
            .get("server")
            .and_then(serde_json::Value::as_str)
            .and_then(|host| SshTarget::parse(host, port).ok())
            .map(|target| target.with_identity(key)),
        use_server: value.get("mode").and_then(serde_json::Value::as_str) == Some("server"),
    }
}

/// 改掉记着的东西：`change` 改完再整个写回去，没改到的照旧。
fn remember(shell: &Shell, change: impl FnOnce(&mut Remembered)) {
    let mut remembered = read_remembered(&shell.remembered());
    change(&mut remembered);
    let mut value = serde_json::json!({
        "mode": if remembered.use_server { "server" } else { "local" },
    });
    if let Some(dir) = &remembered.workdir {
        value["workdir"] = dir.to_string_lossy().into();
    }
    if let Some(target) = &remembered.server {
        value["server"] = target.host.clone().into();
        if let Some(port) = target.port {
            value["sshPort"] = port.into();
        }
        if let Some(key) = &target.identity {
            value["sshKey"] = key.to_string_lossy().into();
        }
    }
    // 第一次启动时数据目录可能还没有：照 sa 的规矩建成只有自己能进的。
    let saved = shell
        .data_dir
        .create()
        .and_then(|()| std::fs::write(shell.remembered(), format!("{value:#}\n")));
    if let Err(error) = saved {
        tracing::warn!("没能记下选的工作目录、服务器，下次启动还会再问：{error}");
    }
}

/// 工作目录是启动时定下的：让服务收好尾、断开隧道，再整个重新启动，照新记下的来。
fn relaunch(handle: &AppHandle) {
    handle.state::<Shell>().stop_server();
    handle.restart();
}

/// 主窗口里开的是服务的网页：只有它（本机服务的，或者隧道这头的）能调「连接」面板的命令。
fn from_main(window: &WebviewWindow) -> Result<(), String> {
    let shell = window.state::<Shell>();
    let home = window
        .url()
        .is_ok_and(|url| shell.homes.lock().unwrap().contains(&origin(&url)));
    if window.label() == MAIN && home {
        Ok(())
    } else {
        Err("只有主窗口能用".into())
    }
}

/// 连谁、往哪台服务器交密码，只有本机服务的页面能定：服务器发来的页面不能替你换服务器、收密码。
fn from_local(window: &WebviewWindow) -> Result<(), String> {
    let shell = window.state::<Shell>();
    let local = shell.local_url.lock().unwrap().as_ref().map(origin);
    let here = window.url().ok().map(|url| origin(&url));
    if window.label() == MAIN && local.is_some() && local == here {
        Ok(())
    } else {
        Err("先换回本机，再连别的服务器".into())
    }
}

/// 现在用的是本机还是哪台服务器、正在连谁、上次为什么没连上，以及面板里先填上的那些。
#[tauri::command]
fn desktop_state(window: WebviewWindow) -> Result<Value, String> {
    from_main(&window)?;
    let shell = window.state::<Shell>();
    let workdir = shell
        .serving
        .lock()
        .unwrap()
        .as_ref()
        .map(|dir| dir.to_string_lossy().into_owned());
    let link = shell.link.lock().unwrap();
    if from_local(&window).is_err() {
        // 服务器的页面只用知道自己是谁，好显示「换回本机」。
        return Ok(json!({
            "current": match &link.target {
                Some(target) => json!({ "mode": "server", "server": target.to_string() }),
                None => json!({ "mode": "local" }),
            },
            "workdir": null,
            "pinned": shell.pinned(),
            "saved": null,
            "connecting": null,
            "failed": null,
        }));
    }
    let saved = match shell.wanted_server() {
        Some(Ok(target)) if shell.pinned().is_some() => Some(target),
        _ => read_remembered(&shell.remembered()).server,
    };
    Ok(json!({
        "current": match &link.target {
            Some(target) => json!({ "mode": "server", "server": target.to_string() }),
            None => json!({ "mode": "local" }),
        },
        "workdir": workdir,
        "pinned": shell.pinned(),
        "saved": saved.map(|target| json!({
            "host": target.host,
            "port": target.port,
            "key": target.identity.map(|key| key.to_string_lossy().into_owned()),
        })),
        "connecting": link.attempt.as_ref().map(|attempt| attempt.target.to_string()),
        "failed": link.failed.as_ref().map(Failure::json),
    }))
}

/// 「连接」面板里点「保存并连接」。连上了窗口会换到服务器的页面；没连上把原因交回页面。
#[tauri::command]
async fn connect_server(
    window: WebviewWindow,
    host: String,
    port: Option<u16>,
    key: Option<String>,
) -> Result<(), Value> {
    from_local(&window)?;
    let handle = window.app_handle().clone();
    let target = handle.state::<Shell>().target(&host, port, key)?;
    tauri::async_runtime::spawn_blocking(move || connect(&handle, target))
        .await
        .map_err(|error| error.to_string())?
}

/// 「测试连接」：真的开一次隧道，那边的 Myriad 答应了才算通，然后就断开。不换页面、不记成要连的服务器。
#[tauri::command]
async fn test_server(
    window: WebviewWindow,
    host: String,
    port: Option<u16>,
    key: Option<String>,
) -> Result<(), Value> {
    from_local(&window)?;
    let handle = window.app_handle().clone();
    let target = handle.state::<Shell>().target(&host, port, key)?;
    tauri::async_runtime::spawn_blocking(move || {
        let shell = handle.state::<Shell>();
        let stop = TunnelStop::default();
        let (id, replaced) = {
            let mut link = shell.link.lock().unwrap();
            link.attempts += 1;
            let id = link.attempts;
            (id, link.probe.replace((id, stop.clone())))
        };
        if let Some((_, replaced)) = replaced {
            replaced.close();
        }
        let opened = Tunnel::open(&target, None, &stop);
        let mut link = shell.link.lock().unwrap();
        if link.probe.as_ref().map(|(probe, _)| *probe) != Some(id) {
            return Err(
                json!({ "server": target.to_string(), "reason": "已取消测试", "cancelled": true }),
            );
        }
        link.probe = None;
        let failure = match opened {
            Ok(tunnel) => {
                tunnel.close();
                None
            }
            Err(error) => Some(Failure::of(&target, &error)),
        };
        link.failed = failure.clone();
        drop(link);
        notify(&handle);
        failure.map_or(Ok(()), |failure| Err(failure.json()))
    })
    .await
    .map_err(|error| error.to_string())?
}

/// 面板里填了服务器密码：登录一次装好这台电脑的公钥，接着就连。密码用完就丢。
/// 密码只交给刚才不认密钥的那一台（[`Link::failed`]），不看面板里现在填的是谁。
#[tauri::command]
async fn install_key(window: WebviewWindow, server: String, password: String) -> Result<(), Value> {
    from_local(&window)?;
    let handle = window.app_handle().clone();
    let denied = handle.state::<Shell>().link.lock().unwrap().failed.clone();
    let Some(Failure {
        target: Some(target),
        server: denied,
        password: true,
        ..
    }) = denied
    else {
        return Err("先连一次：服务器不认密钥时才用得上密码".into());
    };
    // 面板上写的那台和外壳记着的不是同一台（中间又试了别的）：不交密码，让面板重新显示。
    if server != denied {
        notify(&handle);
        return Err("刚才又试了别的服务器：看清现在写的是哪一台，再填密码".into());
    }
    tauri::async_runtime::spawn_blocking(move || {
        // 没装上（多半是密码不对）：密码框留着，可以再填。
        target.install_key(Some(&password)).map_err(
            |error| json!({ "server": server, "reason": format!("{error:#}"), "password": true }),
        )?;
        drop(password);
        connect(&handle, target)
    })
    .await
    .map_err(|error| error.to_string())?
}

/// 不连了、不测了：还在连、还在测的那一次作废。
#[tauri::command]
fn cancel_connect(window: WebviewWindow) -> Result<(), String> {
    from_local(&window)?;
    let shell = window.state::<Shell>();
    let stops: Vec<TunnelStop> = {
        let mut link = shell.link.lock().unwrap();
        let attempt = link.attempt.take().map(|attempt| attempt.stop);
        attempt
            .into_iter()
            .chain(link.probe.take().map(|(_, stop)| stop))
            .collect()
    };
    if !stops.is_empty() {
        // 杀 ssh 要等它退出，别占着调命令的线程。
        std::thread::spawn(move || stops.iter().for_each(TunnelStop::close));
    }
    notify(window.app_handle());
    Ok(())
}

/// 换回本机：断开隧道，窗口回到本机服务的页面。下次打开也不再自动连服务器。
#[tauri::command]
fn use_local(window: WebviewWindow) -> Result<(), String> {
    from_main(&window)?;
    back_to_local(window.app_handle())
}

/// 菜单里也有这一项：服务器那头是旧版、页面里没有「连接」面板时，靠它回得来。
fn back_to_local(handle: &AppHandle) -> Result<(), String> {
    let shell = handle.state::<Shell>();
    if shell.pinned() == Some("server") {
        return Err("启动时用 --server 定死了连服务器，换不了".into());
    }
    let (attempt, tunnel) = {
        let mut link = shell.link.lock().unwrap();
        link.target = None;
        link.failed = None;
        (link.attempt.take(), link.tunnel.take())
    };
    remember(&shell, |remembered| remembered.use_server = false);
    let local = shell.local_url.lock().unwrap().clone();
    let handle = handle.clone();
    std::thread::spawn(move || {
        match (local, tunnel.is_some()) {
            (Some(url), true) => show(&handle, &handle.state::<Shell>(), url, "Myriad"),
            _ => notify(&handle),
        }
        if let Some(attempt) = attempt {
            attempt.stop.close();
        }
        drop(tunnel);
    });
    Ok(())
}

/// 选一把私钥，把路径交回页面。没选就是 `None`。
#[tauri::command]
async fn pick_key(window: WebviewWindow) -> Result<Option<String>, String> {
    from_local(&window)?;
    let handle = window.app_handle().clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        let mut dialog = handle.dialog().file().set_title("选择 SSH 私钥");
        if let Ok(home) = handle.path().home_dir() {
            dialog = dialog.set_directory(home.join(".ssh"));
        }
        dialog.blocking_pick_file()
    })
    .await
    .map_err(|error| error.to_string())?;
    Ok(picked
        .and_then(|picked| picked.into_path().ok())
        .map(|path| path.to_string_lossy().into_owned()))
}

/// 换本机的工作目录：选好了记下来，重新启动。没选、或者选的就是现在这个，什么都不做。
#[tauri::command]
async fn pick_workdir(window: WebviewWindow) -> Result<(), String> {
    if window.label() != START {
        from_local(&window)?;
    }
    let handle = window.app_handle().clone();
    if handle.state::<Shell>().args.workdir.is_some() {
        return Err("启动时用 --workdir 定死了，换不了".into());
    }
    let picker = handle.clone();
    let picked =
        tauri::async_runtime::spawn_blocking(move || pick_folder(&picker).blocking_pick_folder())
            .await
            .map_err(|error| error.to_string())?;
    let Some(dir) = picked.and_then(|picked| picked.into_path().ok()) else {
        return Ok(());
    };
    change_workdir(&handle, dir);
    Ok(())
}

fn change_workdir(handle: &AppHandle, dir: PathBuf) {
    let shell = handle.state::<Shell>();
    if shell.serving.lock().unwrap().as_ref() == Some(&dir) {
        return;
    }
    remember(&shell, |remembered| remembered.workdir = Some(dir));
    relaunch(handle);
}

/// 在系统的文件管理器里打开数据目录。只有本机页面能调，避免远程服务器页面打开本机目录。
#[tauri::command]
fn open_data_dir(window: WebviewWindow) -> Result<(), String> {
    from_local(&window)?;
    show_data_dir(window.app_handle()).map_err(|error| error.to_string())
}

fn show_data_dir(handle: &AppHandle) -> Result<(), tauri_plugin_opener::Error> {
    let dir = handle
        .state::<Shell>()
        .data_dir
        .root()
        .to_string_lossy()
        .into_owned();
    handle.opener().open_path(dir, None::<&str>)
}

/// 主窗口没有系统的标题栏：页面自己画最小化、最大化、关闭，拖顶栏挪窗口，都从这里走。
/// `own` 是页面说「按钮我画好了」，这时才把系统的标题栏和菜单收走。
#[tauri::command]
fn window_chrome(window: WebviewWindow, action: &str) -> Result<(), String> {
    from_main(&window)?;
    match action {
        "own" => window
            .remove_menu()
            .map(drop)
            .and_then(|()| window.set_decorations(false)),
        "drag" => window.start_dragging(),
        "minimize" => window.minimize(),
        "maximize" if window.is_maximized().unwrap_or(false) => window.unmaximize(),
        "maximize" => window.maximize(),
        "close" => window.close(),
        _ => return Err(format!("不认识的窗口操作：{action}")),
    }
    .map_err(|error| error.to_string())
}

/// 启动页上点「重试」。
#[tauri::command]
fn retry(window: WebviewWindow) -> Result<(), String> {
    if window.label() != START {
        return Err("只有启动页能用".into());
    }
    relaunch(window.app_handle());
    Ok(())
}

/// 主窗口想要的大小和四周留的边。主窗口没有系统的标题栏，留的边只是让它别贴着屏幕。
const MAIN_SIZE: (f64, f64) = (1280.0, 820.0);
const MAIN_CHROME: (f64, f64) = (32.0, 48.0);

/// 主窗口打开时的大小：屏幕放得下就用 [`MAIN_SIZE`]，放不下就缩到屏幕里。
fn main_size(handle: &AppHandle) -> (f64, f64) {
    let Ok(Some(monitor)) = handle.primary_monitor() else {
        return MAIN_SIZE;
    };
    let area = monitor
        .work_area()
        .size
        .to_logical::<f64>(monitor.scale_factor());
    fit(MAIN_SIZE, (area.width, area.height))
}

fn fit(want: (f64, f64), area: (f64, f64)) -> (f64, f64) {
    let side = |want: f64, area: f64, chrome: f64| want.min(area - chrome).max(320.0);
    (
        side(want.0, area.0, MAIN_CHROME.0),
        side(want.1, area.1, MAIN_CHROME.1),
    )
}

/// 主窗口：打开本机服务的页面。连上服务器后换到隧道这头的页面（[`show`]）。
fn open_main(handle: &AppHandle, shell: &Shell, url: Url) -> tauri::Result<()> {
    *shell.homes.lock().unwrap() = vec![origin(&url)];
    let (links, popups, downloads) = (handle.clone(), handle.clone(), handle.clone());
    let (width, height) = main_size(handle);
    WebviewWindowBuilder::new(handle, MAIN, WebviewUrl::External(url))
        .title("Myriad")
        .inner_size(width, height)
        .min_inner_size(width.min(720.0), height.min(480.0))
        .center()
        // 标题栏由页面自己画，和界面是一块玻璃。
        .decorations(false)
        // 页面靠它知道自己在桌面版里，再用 desktop_state 问现在连的是哪个。
        .initialization_script("window.__SA_DESKTOP__ = true;")
        // 窗口里只看 Myriad 自己的页面；别的网址（PR 链接之类）交给系统的浏览器。
        .on_navigation(move |to| {
            let home = links
                .state::<Shell>()
                .homes
                .lock()
                .unwrap()
                .contains(&origin(to));
            if home || matches!(to.scheme(), "about" | "blob" | "data") {
                return true;
            }
            open_outside(&links, to);
            false
        })
        .on_new_window(move |to, _| {
            open_outside(&popups, &to);
            NewWindowResponse::Deny
        })
        // 导出 Bot 模板是一次下载：存进「下载」文件夹，存好了说一声存在哪。
        .on_download(move |_, event| {
            match event {
                // 页面给的位置各个系统不一样（Linux 上是程序启动时所在的目录），一律改放「下载」文件夹。
                DownloadEvent::Requested { destination, .. } => {
                    // 系统没有「下载」文件夹的（有些 Linux 桌面没配）退到用户的主目录；
                    // 两个都没有就不动页面给的位置，总比存不了强。
                    let path = downloads.path();
                    let Ok(folder) = path.download_dir().or_else(|_| path.home_dir()) else {
                        return true;
                    };
                    let name = destination
                        .file_name()
                        .unwrap_or("superagent-download".as_ref());
                    *destination = free_name(&folder, Path::new(name));
                }
                DownloadEvent::Finished { path, success, .. } => {
                    let (text, kind) = match path.filter(|_| success) {
                        Some(path) => (
                            format!("已保存到：{}", path.display()),
                            MessageDialogKind::Info,
                        ),
                        None => ("没有保存成功。".to_string(), MessageDialogKind::Error),
                    };
                    downloads
                        .dialog()
                        .message(text)
                        .title("Myriad")
                        .kind(kind)
                        .show(|_| {});
                }
                _ => {}
            }
            true
        })
        .build()
        .map(drop)
}

/// `folder` 里一个还没被占用的文件名：`名字.json`、`名字 (1).json`……
fn free_name(folder: &Path, name: &Path) -> PathBuf {
    let stem = name.file_stem().unwrap_or_default().to_string_lossy();
    let extension = name
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default();
    (0..)
        .map(|n| match n {
            0 => folder.join(name),
            n => folder.join(format!("{stem} ({n}){extension}")),
        })
        .find(|path| !path.exists())
        .expect("总有一个没被占用的名字")
}

/// 用系统的浏览器打开。只开网页地址，别的一概不理。
fn open_outside(handle: &AppHandle, url: &Url) {
    if !matches!(url.scheme(), "http" | "https") {
        return;
    }
    if let Err(error) = handle.opener().open_url(url.as_str(), None::<&str>) {
        tracing::warn!("打不开 {url}：{error}");
    }
}

/// 本机服务没起来：开启动页（内置的页面，不联网）说明原因。
fn open_failed(handle: &AppHandle, shell: &Shell, reason: &str) -> tauri::Result<()> {
    let details = json!({
        "reason": reason,
        "dataDir": shell.data_dir.root().to_string_lossy(),
        "pinned": shell.args.workdir.is_some(),
    });
    WebviewWindowBuilder::new(handle, START, WebviewUrl::App("index.html".into()))
        .title("Myriad")
        .inner_size(800.0, 560.0)
        .menu(menu(handle)?)
        .initialization_script(format!("window.__SA_START__ = {details};"))
        .build()
        .map(drop)
}

/// 系统的菜单栏。只有带系统标题栏的窗口才有：启动页，和页面不会自己画窗口按钮时的主窗口。
fn menu(handle: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let shell = handle.state::<Shell>();
    let mut file = SubmenuBuilder::new(handle, "文件");
    // 用 --workdir 启动的，重启后还是那个目录，菜单里改了也不算数：干脆不给这几项。
    if shell.args.workdir.is_none() {
        file = file
            .text("workdir", "更换工作目录…")
            .text("switch", "连接本机 / 服务器…")
            .text("local", "换回本机");
    }
    let file = file
        .text("data-dir", "打开数据目录")
        .separator()
        .text("quit", "退出")
        .build()?;
    MenuBuilder::new(handle).item(&file).build()
}

fn on_menu(handle: &AppHandle, event: tauri::menu::MenuEvent) {
    match event.id().as_ref() {
        "workdir" => {
            let handle = handle.clone();
            pick_folder(&handle.clone()).pick_folder(move |picked| {
                let Some(dir) = picked.and_then(|picked| picked.into_path().ok()) else {
                    return;
                };
                change_workdir(&handle, dir);
            });
        }
        // 让主窗口里的页面打开「连接」面板。
        "switch" => {
            if let Some(window) = handle.get_webview_window(MAIN) {
                let _ = window.eval("window.dispatchEvent(new Event('sa-desktop-open'))");
            }
        }
        "local" => {
            if let Err(error) = back_to_local(handle) {
                tracing::warn!("换不回本机：{error}");
            }
        }
        "data-dir" => {
            if let Err(error) = show_data_dir(handle) {
                tracing::warn!("打不开数据目录：{error}");
            }
        }
        "quit" => handle.exit(0),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_main_window_fits_inside_a_small_screen() {
        assert_eq!(fit(MAIN_SIZE, (1920.0, 1040.0)), MAIN_SIZE);
        // 1920×1080 放大到 125%，去掉任务栏：整个窗口都要在屏幕里。
        let (width, height) = fit(MAIN_SIZE, (1536.0, 816.0));
        assert_eq!(width, 1280.0);
        assert!(height + MAIN_CHROME.1 <= 816.0);
        assert_eq!(fit(MAIN_SIZE, (200.0, 100.0)), (320.0, 320.0));
    }

    #[test]
    fn downloads_never_overwrite_an_existing_file() {
        let folder = std::env::temp_dir().join(format!("sa-desktop-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let name = Path::new("团队.json");
        assert_eq!(free_name(&folder, name), folder.join("团队.json"));
        std::fs::write(folder.join("团队.json"), "{}").unwrap();
        assert_eq!(free_name(&folder, name), folder.join("团队 (1).json"));
        std::fs::write(folder.join("团队 (1).json"), "{}").unwrap();
        assert_eq!(free_name(&folder, name), folder.join("团队 (2).json"));
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn the_remembered_choice_is_read_back_and_bad_files_count_as_none() {
        let file = std::env::temp_dir().join(format!("sa-desktop-{}.json", std::process::id()));
        // 老版本只记了工作目录。
        std::fs::write(&file, r#"{ "workdir": "/tmp/项目" }"#).unwrap();
        assert_eq!(
            read_remembered(&file),
            Remembered {
                workdir: Some(PathBuf::from("/tmp/项目")),
                ..Remembered::default()
            }
        );
        std::fs::write(
            &file,
            r#"{ "mode": "server", "workdir": "/tmp/项目", "server": "me@vps", "sshPort": 2222, "sshKey": "/home/me/.ssh/vps" }"#,
        )
        .unwrap();
        assert_eq!(
            read_remembered(&file),
            Remembered {
                workdir: Some(PathBuf::from("/tmp/项目")),
                server: Some(
                    SshTarget::parse("me@vps", Some(2222))
                        .unwrap()
                        .with_identity(Some(PathBuf::from("/home/me/.ssh/vps"))),
                ),
                use_server: true,
            }
        );
        // 记着的服务器地址不对：当没记过。
        std::fs::write(
            &file,
            r#"{ "mode": "server", "server": "-oProxyCommand=x" }"#,
        )
        .unwrap();
        assert_eq!(read_remembered(&file).server, None);
        std::fs::write(&file, "不是 JSON").unwrap();
        assert_eq!(read_remembered(&file), Remembered::default());
        std::fs::remove_file(&file).unwrap();
        assert_eq!(read_remembered(&file), Remembered::default());
    }
}
