//! 苦乐的桌面外壳。
//!
//! 它不是另一套程序，有两种用法：
//! - **本机**：在本进程里起和 `sa serve` 一样的本机服务，再开一个窗口去看它。
//!   界面、数据目录、配置都和命令行版是同一份，所以两边不能同时开着。
//! - **服务器**：服务常驻在别的机器上，这边用 SSH 隧道连过去（和 `sa connect` 一样），
//!   窗口开的是隧道这头的地址。关掉窗口只断开隧道，服务器上的任务照常跑。
//!
//! 主窗口里的页面拿不到任何桌面能力，它能做的事和在浏览器里一样多。
//! 只有内置的启动页（选本机还是服务器、说明为什么没起来）能调下面几个命令。

// 发布版在 Windows 上不带黑色的终端窗口。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use clap::Parser;
use sa_cli::{DataDir, RunArgs, SshTarget, Tunnel, TunnelStop};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Mutex};
use std::time::Duration;
use tauri::menu::{MenuBuilder, SubmenuBuilder};
use tauri::webview::{DownloadEvent, NewWindowResponse};
use tauri::{AppHandle, Manager, RunEvent, Url, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tauri_plugin_opener::OpenerExt;

/// 关窗口之后最多等服务收尾这么久（让 Bot 停笔、把没写完的回复标成「中断」）。
const SHUTDOWN_WAIT: Duration = Duration::from_secs(10);

#[derive(Parser, Clone)]
#[command(name = "superagent-desktop", version, about = "苦乐桌面版")]
struct Args {
    /// 数据目录（配置、数据库、会话记录都在这里），和 sa 用的是同一个
    #[arg(long, env = "SA_DATA_DIR", value_name = "目录")]
    data_dir: Option<PathBuf>,
    /// Bot 能读取的工作目录。不给就用上次选的；第一次启动会让你选
    #[arg(long, value_name = "目录")]
    workdir: Option<PathBuf>,
    /// 用不联网的假模型，不需要密钥（用来试流程）
    #[arg(long)]
    mock: bool,
    /// 监听端口，覆盖 config.toml 里的设置
    #[arg(long)]
    port: Option<u16>,
    /// 不在本机起服务，用 SSH 连这台服务器上的苦乐（写成 用户@主机）。不给就用上次选的
    #[arg(long, value_name = "用户@主机", conflicts_with_all = ["workdir", "mock", "port"])]
    server: Option<String>,
    /// 服务器的 SSH 端口
    #[arg(long, requires = "server")]
    ssh_port: Option<u16>,
}

/// 这次怎么启动。
#[derive(Clone)]
enum Launch {
    /// 在本机起服务，Bot 读写这个目录。
    Local(PathBuf),
    /// 连服务器。
    Server(SshTarget),
    /// 还没选过（或者上次选的不能用了）：先让人选。
    Choose,
    /// 命令行给的服务器地址不对。
    Invalid(String),
}

/// 启动页（`ui/index.html`）的窗口名。只有它能调下面的命令。
const START: &str = "start";

/// 正在跑的服务：怎么让它停，以及它停好了没有。
struct Server {
    stop: tokio::sync::oneshot::Sender<()>,
    stopped: mpsc::Receiver<()>,
}

struct Shell {
    args: Args,
    data_dir: DataDir,
    launch: Launch,
    server: Mutex<Option<Server>>,
    tunnel: Mutex<Option<Tunnel>>,
    /// 还在连服务器时，退出要靠它杀掉 ssh。
    connecting: TunnelStop,
}

impl Shell {
    /// 让服务收尾并等它停下，或者断开到服务器的隧道。没在跑、已经停过都没关系。
    fn stop_server(&self) {
        self.connecting.close();
        if let Some(tunnel) = self.tunnel.lock().unwrap().take() {
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

    /// 命令行给的 > 上次选的 > 现在让人选。
    fn decide(&self) -> Launch {
        if let Some(host) = &self.args.server {
            return match SshTarget::parse(host, self.args.ssh_port) {
                Ok(target) => Launch::Server(target),
                Err(error) => Launch::Invalid(format!("{error:#}")),
            };
        }
        if let Some(dir) = &self.args.workdir {
            return Launch::Local(dir.clone());
        }
        let remembered = read_remembered(&self.remembered());
        match remembered.server.filter(|_| remembered.use_server) {
            Some(target) => Launch::Server(target),
            // 上次选的目录后来被删了、挪了，就当没选过。
            None => match remembered.workdir.filter(|dir| dir.is_dir()) {
                Some(dir) => Launch::Local(dir),
                None => Launch::Choose,
            },
        }
    }

    /// 启动参数里定死了用哪个（`--workdir`、`--server`）：菜单和启动页里就不让换。
    fn pinned(&self) -> bool {
        self.args.workdir.is_some() || self.args.server.is_some()
    }
}

fn main() {
    let args = Args::parse();
    sa_cli::init_logging(tracing::Level::INFO);
    let data_dir = match DataDir::resolve(args.data_dir.clone()) {
        Ok(data_dir) => data_dir,
        Err(error) => {
            eprintln!("错误：{error:#}");
            std::process::exit(1);
        }
    };
    let mut shell = Shell {
        args,
        data_dir,
        launch: Launch::Choose,
        server: Mutex::new(None),
        tunnel: Mutex::new(None),
        connecting: TunnelStop::default(),
    };
    shell.launch = shell.decide();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(shell)
        .invoke_handler(tauri::generate_handler![choose_local, choose_server, retry])
        .setup(|app| {
            set_menu(app.handle())?;
            // 连服务器、等服务起来都要等，不能占着主线程。
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

/// 按 [`Launch`] 起服务或连服务器，开窗口。哪一步不成都开启动页说明原因，而不是悄悄退出。
fn start(handle: &AppHandle) {
    let shell = handle.state::<Shell>();
    let opened = match shell.launch.clone() {
        Launch::Local(workdir) => start_local(handle, &shell, workdir),
        Launch::Server(target) => start_server(handle, &shell, &target),
        Launch::Choose => open_start(handle, &shell, serde_json::json!({ "view": "choose" })),
        Launch::Invalid(reason) => open_failed(handle, &shell, &reason, None),
    };
    if let Err(error) = opened {
        tracing::error!("窗口打不开：{error}");
        handle.exit(1);
    }
}

fn start_local(handle: &AppHandle, shell: &Shell, workdir: PathBuf) -> tauri::Result<()> {
    let (ready, started) = mpsc::channel::<Result<String, String>>();
    let (stop, stop_asked) = tokio::sync::oneshot::channel::<()>();
    let (done, stopped) = mpsc::channel::<()>();
    let run = RunArgs {
        workdir: Some(workdir),
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
        Ok(Ok(url)) => open_main(handle, &url, "苦乐"),
        Ok(Err(reason)) => open_failed(handle, shell, &reason, None),
        Err(_) => open_failed(handle, shell, "服务还没开始就退出了", None),
    }
}

/// 先开启动页说「正在连接」，连上了换成主窗口，没连上就在启动页里说明原因。
fn start_server(handle: &AppHandle, shell: &Shell, target: &SshTarget) -> tauri::Result<()> {
    let server = target.to_string();
    open_start(
        handle,
        shell,
        serde_json::json!({ "view": "connecting", "server": server }),
    )?;
    match Tunnel::open(target, None, &shell.connecting) {
        Ok(tunnel) => {
            let url = tunnel.url().to_string();
            *shell.tunnel.lock().unwrap() = Some(tunnel);
            open_main(handle, &url, &format!("苦乐 · {}", target.host))?;
            if let Some(start) = handle.get_webview_window(START) {
                start.close()?;
            }
            Ok(())
        }
        Err(error) => open_failed(handle, shell, &format!("{error:#}"), Some(&server)),
    }
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
    Remembered {
        workdir: value
            .get("workdir")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from),
        server: value
            .get("server")
            .and_then(serde_json::Value::as_str)
            .and_then(|host| SshTarget::parse(host, port).ok()),
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

/// 让服务收好尾（或者断开隧道），再整个重新启动，照新记下的来。
fn relaunch(handle: &AppHandle) {
    handle.state::<Shell>().stop_server();
    handle.restart();
}

/// 命令只给内置的启动页用；主窗口里开的是服务的网页，不许它调。
fn from_start_page(window: &WebviewWindow) -> Result<(), String> {
    if window.label() == START {
        Ok(())
    } else {
        Err("只有启动页能用".into())
    }
}

/// 启动页上点「用本机」：选一个工作目录，然后用它重新启动。没选就什么都不做。
#[tauri::command]
async fn choose_local(window: WebviewWindow) -> Result<(), String> {
    from_start_page(&window)?;
    let handle = window.app_handle().clone();
    let picked = {
        let handle = handle.clone();
        tauri::async_runtime::spawn_blocking(move || pick_folder(&handle).blocking_pick_folder())
            .await
            .map_err(|error| error.to_string())?
    };
    let Some(dir) = picked.and_then(|picked| picked.into_path().ok()) else {
        return Ok(());
    };
    remember(&handle.state::<Shell>(), |remembered| {
        remembered.workdir = Some(dir);
        remembered.use_server = false;
    });
    relaunch(&handle);
    Ok(())
}

/// 启动页上填好服务器地址点「连接」：记下来，然后重新启动去连它。地址不对就把原因交回页面。
#[tauri::command]
fn choose_server(window: WebviewWindow, host: String, port: Option<u16>) -> Result<(), String> {
    from_start_page(&window)?;
    let target = SshTarget::parse(&host, port).map_err(|error| format!("{error:#}"))?;
    let handle = window.app_handle().clone();
    remember(&handle.state::<Shell>(), |remembered| {
        remembered.server = Some(target);
        remembered.use_server = true;
    });
    relaunch(&handle);
    Ok(())
}

/// 启动页上点「重试」。
#[tauri::command]
fn retry(window: WebviewWindow) -> Result<(), String> {
    from_start_page(&window)?;
    relaunch(window.app_handle());
    Ok(())
}

/// 主窗口：直接打开服务的页面（本机的，或者隧道这头的）。
fn open_main(handle: &AppHandle, url: &str, title: &str) -> tauri::Result<()> {
    let url: Url = url.parse().map_err(tauri::Error::InvalidUrl)?;
    let home = url.origin();
    let (links, popups, downloads) = (handle.clone(), handle.clone(), handle.clone());
    WebviewWindowBuilder::new(handle, "main", WebviewUrl::External(url))
        .title(title)
        .inner_size(1280.0, 820.0)
        .min_inner_size(720.0, 480.0)
        // 窗口里只看苦乐自己的页面；别的网址（PR 链接之类）交给系统的浏览器。
        .on_navigation(move |to| {
            if to.origin() == home || matches!(to.scheme(), "about" | "blob" | "data") {
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
                        .title("苦乐")
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

/// 服务没起来、服务器没连上：在启动页里说明原因。`server` 是连的哪台服务器，本机就是 `None`。
fn open_failed(
    handle: &AppHandle,
    shell: &Shell,
    reason: &str,
    server: Option<&str>,
) -> tauri::Result<()> {
    let details = serde_json::json!({
        "view": "failed",
        "reason": reason,
        "server": server,
        "dataDir": shell.data_dir.root().to_string_lossy(),
    });
    open_start(handle, shell, details)
}

/// 启动页（内置的页面，不联网）。已经开着就换成 `details` 说的那一屏。
fn open_start(
    handle: &AppHandle,
    shell: &Shell,
    mut details: serde_json::Value,
) -> tauri::Result<()> {
    details["pinned"] = shell.pinned().into();
    if let Some(window) = handle.get_webview_window(START) {
        // 页面可能还没载完：先记下，载完时 main.js 会照它显示。
        window.eval(format!(
            "window.__SA_START__ = {details}; window.saShow?.(window.__SA_START__);"
        ))?;
        return window.set_focus();
    }
    WebviewWindowBuilder::new(handle, START, WebviewUrl::App("index.html".into()))
        .title("苦乐")
        .inner_size(760.0, 520.0)
        .initialization_script(format!("window.__SA_START__ = {details};"))
        .build()
        .map(drop)
}

fn set_menu(handle: &AppHandle) -> tauri::Result<()> {
    let shell = handle.state::<Shell>();
    let mut file = SubmenuBuilder::new(handle, "文件");
    // 用 --workdir、--server 启动的，重启后还是那样，菜单里改了也不算数：干脆不给这几项。
    if !shell.pinned() {
        if matches!(shell.launch, Launch::Local(_)) {
            file = file.text("workdir", "更换工作目录…");
        }
        file = file.text("switch", "本机 / 服务器…");
    }
    let file = file
        .text("data-dir", "打开数据目录")
        .separator()
        .text("quit", "退出")
        .build()?;
    handle.set_menu(MenuBuilder::new(handle).item(&file).build()?)?;
    handle.on_menu_event(|handle, event| match event.id().as_ref() {
        "workdir" => {
            let handle = handle.clone();
            pick_folder(&handle.clone()).pick_folder(move |picked| {
                let Some(dir) = picked.and_then(|picked| picked.into_path().ok()) else {
                    return;
                };
                remember(&handle.state::<Shell>(), |remembered| {
                    remembered.workdir = Some(dir);
                    remembered.use_server = false;
                });
                // 工作目录是启动时定下的：先让服务收好尾，再整个重新启动。
                relaunch(&handle);
            });
        }
        // Windows 上在菜单回调里直接建窗口会卡死（WebView2），换个线程建。
        "switch" => {
            let handle = handle.clone();
            std::thread::spawn(move || {
                let shell = handle.state::<Shell>();
                let choose = serde_json::json!({ "view": "choose" });
                if let Err(error) = open_start(&handle, &shell, choose) {
                    tracing::warn!("打不开启动页：{error}");
                }
            });
        }
        "data-dir" => {
            let dir = handle
                .state::<Shell>()
                .data_dir
                .root()
                .to_string_lossy()
                .into_owned();
            if let Err(error) = handle.opener().open_path(dir, None::<&str>) {
                tracing::warn!("打不开数据目录：{error}");
            }
        }
        "quit" => handle.exit(0),
        _ => {}
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            r#"{ "mode": "server", "workdir": "/tmp/项目", "server": "me@vps", "sshPort": 2222 }"#,
        )
        .unwrap();
        assert_eq!(
            read_remembered(&file),
            Remembered {
                workdir: Some(PathBuf::from("/tmp/项目")),
                server: Some(SshTarget::parse("me@vps", Some(2222)).unwrap()),
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
