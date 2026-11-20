//! `sa serve` 和 `sa chat` 共用的启动过程：锁住数据目录、读配置、打开数据库、接好模型和工具。

use crate::config::{Config, DEFAULT_CONFIG_TOML};
use crate::models::{self, ConfigModels};
use crate::paths::DataDir;
use anyhow::{Context, Result};
use sa_engine::Engine;
use sa_orchestrator::{ModelRouter, Orchestrator, Settings};
use sa_store::Store;
use sa_tools::Fence;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 一轮里最多调用几次模型。
const MAX_STEPS: usize = 24;

#[derive(clap::Args, Clone, Debug, Default)]
pub struct RunArgs {
    /// Bot 能读取的工作目录，默认是当前目录
    #[arg(long, value_name = "目录")]
    pub workdir: Option<PathBuf>,
    /// 用不联网的假模型，不需要密钥（用来试流程）
    #[arg(long)]
    pub mock: bool,
}

pub struct App {
    pub config: Config,
    pub engine: Arc<Engine>,
    pub orchestrator: Arc<Orchestrator>,
    /// 给人看的工作目录。
    pub workdir: String,
    pub mock: bool,
    /// 持有期间别的 `sa serve` / `sa chat` 不能用这个数据目录。
    _lock: std::fs::File,
}

impl App {
    pub fn open(data_dir: &DataDir, args: &RunArgs) -> Result<Self> {
        Self::open_with(data_dir, args, |config| {
            if args.mock {
                Arc::new(models::mock())
            } else {
                Arc::new(ConfigModels::new(config.clone()))
            }
        })
    }

    /// 和 [`App::open`] 一样，但由调用方决定用哪些模型。
    pub fn open_with(
        data_dir: &DataDir,
        args: &RunArgs,
        models: impl FnOnce(&Config) -> Arc<dyn ModelRouter>,
    ) -> Result<Self> {
        create_private_dir(data_dir.root())
            .with_context(|| format!("无法创建数据目录 {}", data_dir.root().display()))?;
        let lock = lock_data_dir(&data_dir.lock())?;
        if write_if_missing(&data_dir.config(), DEFAULT_CONFIG_TOML)? {
            tracing::info!("已生成默认配置 {}", data_dir.config().display());
        }
        let config = Config::load(&data_dir.config())?;

        let workdir = match &args.workdir {
            Some(dir) => dir.clone(),
            None => std::env::current_dir().context("拿不到当前目录，请用 --workdir 指定")?,
        };
        // 数据目录里有访问令牌和数据库：即使它在工作目录里面，Bot 也不能读。
        let fence = Fence::new(&workdir, &[data_dir.root().to_path_buf()])
            .with_context(|| format!("工作目录 {} 打不开", workdir.display()))?;
        let workdir = fence.root_display();

        let prompt_path = data_dir.chat_prompt();
        let prompt = match std::fs::read_to_string(&prompt_path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => {
                return Err(e).with_context(|| format!("读取 {} 失败", prompt_path.display()))
            }
        };

        let store = Store::open(&data_dir.database())
            .with_context(|| format!("无法打开数据库 {}", data_dir.database().display()))?;
        let engine = Arc::new(Engine::new(store)?);
        engine.ensure_defaults()?;
        let interrupted = engine.recover_interrupted()?;
        if interrupted > 0 {
            tracing::info!("上次退出时有 {interrupted} 条回复没写完，已标记为中断");
        }

        let models = models(&config);
        let settings = Settings {
            sessions_dir: data_dir.sessions(),
            fence,
            prompt,
            max_steps: MAX_STEPS,
            allow_private_network: false,
        };
        let orchestrator = Arc::new(Orchestrator::new(engine.clone(), models, settings));
        Ok(Self {
            config,
            engine,
            orchestrator,
            workdir,
            mock: args.mock,
            _lock: lock,
        })
    }
}

/// 用真实模型时要让人知道的事：配置里的花费上限现在还拦不住调用。
pub const BUDGET_NOTICE: &str =
    "注意：config.toml 里的每日花费上限这个版本还没有生效，模型调用不会被它拦住，花费请在模型服务的控制台里看。";

pub fn init_logging(default: tracing::Level) {
    let level = std::env::var("SA_LOG")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default);
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(std::io::stderr)
        .init();
}

/// 独占数据目录，直到返回的文件被关闭（进程退出时系统会自动释放）。
/// 引擎假定自己是唯一的写入者，两个进程共用一个目录会互相看不到对方的事件。
fn lock_data_dir(path: &Path) -> Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("无法创建 {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(
            "这个数据目录正被另一个 sa serve 或 sa chat 使用：请先停掉它，或用 --data-dir 换一个目录"
        ),
        Err(std::fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("无法锁定 {}", path.display()))
        }
    }
}

/// 建目录（连同缺的上级）。新建出来的目录只有当前用户能进：里面有令牌、数据库和会话记录。
/// 已经存在的目录不动它的权限。
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(path)
}

/// 文件不存在时写入 `contents`；返回是否新建了文件。已有的文件不动。
pub fn write_if_missing(path: &Path, contents: &str) -> Result<bool> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn open(dir: &Path, workdir: &Path) -> Result<App> {
        let data_dir = DataDir::resolve(Some(dir.to_path_buf())).unwrap();
        App::open(
            &data_dir,
            &RunArgs {
                workdir: Some(workdir.to_path_buf()),
                mock: true,
            },
        )
    }

    #[test]
    fn data_dir_lock_is_exclusive_until_released() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serve.lock");
        let held = lock_data_dir(&path).unwrap();
        assert!(lock_data_dir(&path).is_err());
        drop(held);
        lock_data_dir(&path).unwrap();
    }

    #[test]
    fn write_if_missing_keeps_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(write_if_missing(&path, "a").unwrap());
        assert!(!write_if_missing(&path, "b").unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a");
    }

    #[test]
    fn open_creates_the_data_dir_and_refuses_a_second_opener() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let app = open(&data, dir.path()).unwrap();
        assert!(data.join("config.toml").exists());
        assert!(data.join("state.db").exists());
        assert_eq!(app.engine.list_rooms().unwrap().len(), 1);

        let err = open(&data, dir.path()).err().unwrap().to_string();
        assert!(err.contains("正被另一个"), "{err}");
        drop(app);
        open(&data, dir.path()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn new_data_dir_is_private_and_an_existing_one_is_left_alone() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let dir = tempfile::tempdir().unwrap();
        let fresh = dir.path().join("fresh");
        drop(open(&fresh, dir.path()).unwrap());
        assert_eq!(mode(&fresh), 0o700);

        let existing = dir.path().join("existing");
        std::fs::create_dir(&existing).unwrap();
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o755)).unwrap();
        drop(open(&existing, dir.path()).unwrap());
        assert_eq!(mode(&existing), 0o755);
    }

    #[test]
    fn missing_workdir_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = open(&dir.path().join("data"), &dir.path().join("没有这个目录"))
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("工作目录"), "{err:#}");
    }
}
