use crate::config::{Config, DEFAULT_CONFIG_TOML};
use crate::paths::DataDir;
use anyhow::{Context, Result};
use sa_engine::Engine;
use sa_server::AppState;
use sa_store::Store;
use std::io::{ErrorKind, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use tokio::net::TcpListener;

#[derive(clap::Args)]
pub struct ServeArgs {
    /// 监听端口，覆盖 config.toml 里的设置（0 表示由系统分配）
    #[arg(long)]
    port: Option<u16>,
    /// 启动后自动打开浏览器
    #[arg(long)]
    open: bool,
}

pub fn run(data_dir: &DataDir, args: ServeArgs) -> Result<()> {
    init_logging();
    std::fs::create_dir_all(data_dir.root())
        .with_context(|| format!("无法创建数据目录 {}", data_dir.root().display()))?;
    if write_if_missing(&data_dir.config(), DEFAULT_CONFIG_TOML)? {
        tracing::info!("已生成默认配置 {}", data_dir.config().display());
    }
    let config = Config::load(&data_dir.config())?;
    let token = load_or_create_token(&data_dir.token())?;

    let store = Store::open(&data_dir.database())
        .with_context(|| format!("无法打开数据库 {}", data_dir.database().display()))?;
    let engine = Arc::new(Engine::new(store)?);
    engine.ensure_defaults()?;

    let addr = SocketAddr::new(config.server.host, args.port.unwrap_or(config.server.port));
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
            println!("在浏览器里打开：{url}");
            println!("按 Ctrl+C 停止。");
            if args.open {
                if let Err(e) = open_browser(&url) {
                    tracing::warn!("无法自动打开浏览器：{e}");
                }
            }
            sa_server::serve(listener, AppState::new(engine, token), async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await?;
            Ok(())
        })
}

fn init_logging() {
    let level = std::env::var("SA_LOG")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(tracing::Level::INFO);
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(std::io::stderr)
        .init();
}

/// 文件不存在时写入 `contents`；返回是否新建了文件。已有的文件不动。
fn write_if_missing(path: &Path, contents: &str) -> Result<bool> {
    match open_new_private(path) {
        Ok(mut file) => {
            file.write_all(contents.as_bytes())
                .with_context(|| format!("写入 {} 失败", path.display()))?;
            Ok(true)
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e).with_context(|| format!("无法创建 {}", path.display())),
    }
}

/// 新建一个只有当前用户能读写的文件；已存在则报 `AlreadyExists`。
fn open_new_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
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

    #[test]
    fn write_if_missing_keeps_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(write_if_missing(&path, "a").unwrap());
        assert!(!write_if_missing(&path, "b").unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a");
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
