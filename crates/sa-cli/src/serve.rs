use crate::app::{init_logging, write_if_missing, App, RunArgs};
use crate::paths::DataDir;
use anyhow::{Context, Result};
use sa_server::AppState;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::Path;
use tokio::net::TcpListener;

#[derive(clap::Args)]
pub struct ServeArgs {
    /// 监听端口，覆盖 config.toml 里的设置（0 表示由系统分配）
    #[arg(long)]
    port: Option<u16>,
    /// 启动后自动打开浏览器
    #[arg(long)]
    open: bool,
    #[command(flatten)]
    run: RunArgs,
}

pub fn run(data_dir: &DataDir, args: ServeArgs) -> Result<()> {
    init_logging(tracing::Level::INFO);
    let app = App::open(data_dir, &args.run)?;
    let token = load_or_create_token(&data_dir.token())?;

    let server = &app.config.server;
    let addr = SocketAddr::new(server.host, args.port.unwrap_or(server.port));
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            let listener = TcpListener::bind(addr).await.map_err(|e| match e.kind() {
                ErrorKind::AddrInUse => anyhow::anyhow!(
                    "端口 {addr} 已被占用：可能已有一个 sa serve 在运行，或用 --port 换一个端口"
                ),
                _ => anyhow::Error::new(e).context(format!("无法监听 {addr}")),
            })?;
            let url = format!("http://{}/auth?token={token}", listener.local_addr()?);
            println!("SuperAgent 已启动，数据目录：{}", data_dir.root().display());
            println!("Bot 能读取的工作目录：{}", app.workdir);
            if app.mock {
                println!("当前用的是不联网的假模型（--mock），回复是固定规则生成的。");
            } else {
                println!("{}", app.config.budget_notice());
            }
            println!("在浏览器里打开：{url}");
            println!("按 Ctrl+C 停止。");
            if args.open {
                if let Err(e) = open_browser(&url) {
                    tracing::warn!("无法自动打开浏览器：{e}");
                }
            }
            // 上次退出时没做完的任务接着排队。
            app.orchestrator.resume_tasks();
            app.orchestrator.start_watchdog();
            app.orchestrator.start_routines();
            let mut state = AppState::new(app.engine.clone(), app.orchestrator.clone(), token);
            // 外面的连接都是可选的：没设对应的环境变量就不开。
            let github = &app.config.github;
            if let Some(secret) = env_value(&github.webhook_secret_env) {
                let options = sa_adapters::github_events::Options {
                    issue_labels: github.issue_labels.clone(),
                };
                state = state.with_github_hook(&secret, options);
                println!("接收 GitHub 事件：/hooks/github");
            }
            let telegram = &app.config.telegram;
            if let Some(token) = env_value(&telegram.token_env) {
                let client = sa_orchestrator::Telegram::new(&telegram.api_base, &token);
                app.orchestrator.start_telegram(client, Default::default());
                println!("正在连接 Telegram。");
            }
            let orchestrator = app.orchestrator.clone();
            sa_server::serve(listener, state, async move {
                let _ = tokio::signal::ctrl_c().await;
                // 先让 Bot 停笔、把没写完的回复标成「中断」，再关连接：浏览器还来得及收到这个状态。
                orchestrator.shutdown().await;
            })
            .await?;
            Ok(())
        })
}

/// 环境变量的值。没设或者是空的算没有。
fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 访问令牌保存在数据目录里，重启后链接不变。
fn load_or_create_token(path: &Path) -> Result<String> {
    let fresh = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    if write_if_missing(path, &fresh)? {
        return Ok(fresh);
    }
    let token = std::fs::read_to_string(path)
        .with_context(|| format!("读取 {} 失败", path.display()))?
        .trim()
        .to_string();
    anyhow::ensure!(
        token.len() >= 32 && token.chars().all(|c| c.is_ascii_alphanumeric()),
        "{} 里的访问令牌无效，删除这个文件后重新启动即可生成新的",
        path.display()
    );
    Ok(token)
}

fn open_browser(url: &str) -> std::io::Result<()> {
    let mut command = if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else {
        std::process::Command::new("xdg-open")
    };
    command.arg(url).spawn().map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_created_once_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        let first = load_or_create_token(&path).unwrap();
        assert_eq!(first.len(), 64);
        assert_eq!(load_or_create_token(&path).unwrap(), first);
    }

    #[test]
    fn damaged_token_file_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        std::fs::write(&path, "short").unwrap();
        assert!(load_or_create_token(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn new_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        load_or_create_token(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
