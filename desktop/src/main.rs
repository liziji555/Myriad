//! SuperAgent 的桌面外壳。
//!
//! 它不是另一套程序：启动时在本进程里起和 `sa serve` 一样的本机服务，再开一个窗口去看它。
//! 界面、数据目录、配置都和命令行版是同一份，所以两边不能同时开着。
//!
//! 窗口里的页面拿不到任何桌面能力（没有给它开 Tauri 的权限），它能做的事和在浏览器里一样多。

// 发布版在 Windows 上不带黑色的终端窗口。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use clap::Parser;
use sa_cli::{DataDir, RunArgs};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Mutex};
use std::time::Duration;
use tauri::menu::{MenuBuilder, SubmenuBuilder};
use tauri::webview::{DownloadEvent, NewWindowResponse};
use tauri::{AppHandle, Manager, RunEvent, Url, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tauri_plugin_opener::OpenerExt;

/// 关窗口之后最多等服务收尾这么久（让 Bot 停笔、把没写完的回复标成「中断」）。
const SHUTDOWN_WAIT: Duration = Duration::from_secs(10);

#[derive(Parser, Clone)]
#[command(name = "superagent-desktop", version, about = "SuperAgent 桌面版")]
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
}

/// 正在跑的服务：怎么让它停，以及它停好了没有。
struct Server {
    stop: tokio::sync::oneshot::Sender<()>,
    stopped: mpsc::Receiver<()>,
}

struct Shell {
    args: Args,
    data_dir: DataDir,
    server: Mutex<Option<Server>>,
}

impl Shell {
    /// 让服务收尾并等它停下。没在跑、已经停过都没关系。
    fn stop_server(&self) {
        let Some(server) = self.server.lock().unwrap().take() else {
            return;
        };
        let _ = server.stop.send(());
        if server.stopped.recv_timeout(SHUTDOWN_WAIT).is_err() {
            tracing::warn!("服务没能在 {SHUTDOWN_WAIT:?} 内停好，直接退出");
        }
    }

    /// 记着上次选的工作目录的文件。
    fn remembered(&self) -> PathBuf {
        self.data_dir.root().join("desktop.json")
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
    let shell = Shell {
        args,
        data_dir,
        server: Mutex::new(None),
    };

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(shell)
        .setup(|app| {
            set_menu(app.handle())?;
            // 选目录的对话框要等人，不能占着主线程。
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

/// 定下工作目录，起服务，开窗口。哪一步不成都开一个说明原因的窗口，而不是悄悄退出。
fn start(handle: &AppHandle) {
    let shell = handle.state::<Shell>();
    let Some(workdir) = workdir(handle, &shell) else {
        // 第一次启动没选目录就关了：什么都不做。
        handle.exit(0);
        return;
    };

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

    let opened = match started.recv() {
        Ok(Ok(url)) => open_main(handle, &url),
        Ok(Err(reason)) => open_failed(handle, &shell, &reason),
        Err(_) => open_failed(handle, &shell, "服务还没开始就退出了"),
    };
    if let Err(error) = opened {
        tracing::error!("窗口打不开：{error}");
        handle.exit(1);
    }
}

/// 这次用哪个工作目录：命令行给的 > 上次选的 > 现在让人选。`None` 表示人没选。
fn workdir(handle: &AppHandle, shell: &Shell) -> Option<PathBuf> {
    if let Some(dir) = &shell.args.workdir {
        return Some(dir.clone());
    }
    // 上次选的目录后来被删了、挪了，就当没选过。
    if let Some(dir) = read_remembered(&shell.remembered()).filter(|dir| dir.is_dir()) {
        return Some(dir);
    }
    handle
        .dialog()
        .message(
            "Bot 只能读写一个你指定的文件夹（通常是你的项目文件夹）。\n\n\
             接下来请选这个文件夹。以后可以在菜单「文件 → 更换工作目录」里改。",
        )
        .title("SuperAgent")
        .blocking_show();
    let dir = pick_folder(handle)
        .blocking_pick_folder()?
        .into_path()
        .ok()?;
    remember(shell, &dir);
    Some(dir)
}

fn pick_folder(handle: &AppHandle) -> tauri_plugin_dialog::FileDialogBuilder<tauri::Wry> {
    handle
        .dialog()
        .file()
        .set_title("选择 Bot 能读写的工作目录")
}

fn read_remembered(path: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(PathBuf::from(value.get("workdir")?.as_str()?))
}

fn remember(shell: &Shell, dir: &Path) {
    let value = serde_json::json!({ "workdir": dir.to_string_lossy() });
    // 第一次启动时数据目录可能还没有：照 sa 的规矩建成只有自己能进的。
    let saved = shell
        .data_dir
        .create()
        .and_then(|()| std::fs::write(shell.remembered(), format!("{value:#}\n")));
    if let Err(error) = saved {
        tracing::warn!("没能记下选的工作目录，下次启动还会再问：{error}");
    }
}

/// 主窗口：直接打开本机服务的页面。
fn open_main(handle: &AppHandle, url: &str) -> tauri::Result<()> {
    let url: Url = url.parse().map_err(tauri::Error::InvalidUrl)?;
    let home = url.origin();
    let (links, popups, downloads) = (handle.clone(), handle.clone(), handle.clone());
    WebviewWindowBuilder::new(handle, "main", WebviewUrl::External(url))
        .title("SuperAgent")
        .inner_size(1280.0, 820.0)
        .min_inner_size(720.0, 480.0)
        // 窗口里只看 SuperAgent 自己的页面；别的网址（PR 链接之类）交给系统的浏览器。
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
                    let Ok(folder) = downloads.path().download_dir() else {
                        return false;
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
                        .title("SuperAgent")
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

/// 服务没起来：开一个只说明原因的窗口（内置的页面，不联网）。
fn open_failed(handle: &AppHandle, shell: &Shell, reason: &str) -> tauri::Result<()> {
    let details = serde_json::json!({
        "reason": reason,
        "dataDir": shell.data_dir.root().to_string_lossy(),
    });
    WebviewWindowBuilder::new(handle, "main", WebviewUrl::App("index.html".into()))
        .title("SuperAgent")
        .inner_size(760.0, 480.0)
        .initialization_script(format!("window.__SA_START__ = {details};"))
        .build()
        .map(drop)
}

fn set_menu(handle: &AppHandle) -> tauri::Result<()> {
    let mut file = SubmenuBuilder::new(handle, "文件");
    // 用 --workdir 启动的，重启后还是那个目录，菜单里改了也不算数：干脆不给这一项。
    if handle.state::<Shell>().args.workdir.is_none() {
        file = file.text("workdir", "更换工作目录…");
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
                let shell = handle.state::<Shell>();
                remember(&shell, &dir);
                // 工作目录是启动时定下的：先让服务收好尾，再整个重新启动。
                shell.stop_server();
                handle.restart();
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
    fn the_remembered_workdir_is_read_back_and_bad_files_count_as_none() {
        let file = std::env::temp_dir().join(format!("sa-desktop-{}.json", std::process::id()));
        std::fs::write(&file, r#"{ "workdir": "/tmp/项目" }"#).unwrap();
        assert_eq!(read_remembered(&file), Some(PathBuf::from("/tmp/项目")));
        std::fs::write(&file, "不是 JSON").unwrap();
        assert_eq!(read_remembered(&file), None);
        std::fs::remove_file(&file).unwrap();
        assert_eq!(read_remembered(&file), None);
    }
}
