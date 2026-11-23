//! `sa serve` 和 `sa chat` 共用的启动过程：锁住数据目录、读配置、打开数据库、接好模型和工具。

use crate::config::{Config, DEFAULT_CONFIG_TOML};
use crate::models::{self, ConfigModels};
use crate::paths::DataDir;
use anyhow::{Context, Result};
use sa_engine::{Audit, Engine, AUDIT_KEEP_DAYS};
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
                // 假模型也看 mid 档位的窗口大小：调小它就能演示自动压缩。
                Arc::new(models::mock(config.tiers.mid.context_window))
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
        Self::open_tuned(data_dir, args, models, |_| {})
    }

    /// 和 [`App::open_with`] 一样，另外可以在启动前调整设置。测试用它把等待时间调短。
    pub fn open_tuned(
        data_dir: &DataDir,
        args: &RunArgs,
        models: impl FnOnce(&Config) -> Arc<dyn ModelRouter>,
        tune: impl FnOnce(&mut Settings),
    ) -> Result<Self> {
        let (engine, lock) = open_state(data_dir)?;
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

        // Bot 执行的命令拿不到这些密钥：只有 sa 自己用它们。
        sa_tools::hide_env_from_commands(
            config
                .providers
                .values()
                .map(|provider| provider.api_key_env.clone())
                .chain([
                    config.github.token_env.clone(),
                    config.github.webhook_secret_env.clone(),
                    config.telegram.token_env.clone(),
                ]),
        );
        let models = models(&config);
        let mut settings = Settings {
            sessions_dir: data_dir.sessions(),
            fence,
            prompt,
            max_steps: MAX_STEPS,
            allow_private_network: false,
            approval_timeout: sa_orchestrator::DEFAULT_APPROVAL_TIMEOUT,
            work_dir: data_dir.work(),
            github: Some(sa_orchestrator::GitHub {
                api_base: config.github.api_base.clone(),
                token: std::env::var(&config.github.token_env)
                    .ok()
                    .filter(|token| !token.trim().is_empty()),
                repo: config.github.repo.clone(),
            }),
            budget: config.budget(),
            watchdog: sa_orchestrator::Watchdog {
                db_path: Some(data_dir.database()),
                ..Default::default()
            },
            routines: Default::default(),
            max_parallel_bots: config.rooms.max_parallel_bots.max(1),
        };
        tune(&mut settings);
        let orchestrator = Arc::new(Orchestrator::new(engine.clone(), models, settings));
        orchestrator.use_skills(data_dir.skills());
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

impl App {
    /// 配置里启用着的 MCP 服务。它们在工作目录里启动。
    pub fn mcp_specs(&self) -> Vec<sa_tools::mcp::McpSpec> {
        let env = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
        self.config
            .mcp_specs(std::path::Path::new(&self.workdir), env)
    }

    /// 把配置里的 MCP 服务连上再往下走：接下来的第一步就可能用到它们的工具。没连上的说一声，不拦着。
    pub async fn connect_mcp(&self) {
        for spec in self.mcp_specs() {
            let name = spec.name.clone();
            let server = Arc::new(sa_tools::mcp::McpServer::spawned(spec));
            if let Err(error) = self.orchestrator.connect_mcp(server).await {
                eprintln!("MCP 服务「{name}」没连上：{error}");
            }
        }
    }
}

/// 锁住数据目录并打开状态库。返回的文件持有期间，别的 `sa` 进程不能用这个数据目录。
pub fn open_state(data_dir: &DataDir) -> Result<(Arc<Engine>, std::fs::File)> {
    data_dir
        .create()
        .with_context(|| format!("无法创建数据目录 {}", data_dir.root().display()))?;
    let lock = lock_data_dir(&data_dir.lock())?;
    let store = Store::open(&data_dir.database())
        .with_context(|| format!("无法打开数据库 {}", data_dir.database().display()))?;
    let engine = Arc::new(Engine::new(store)?);
    engine.ensure_defaults()?;
    let interrupted = engine.recover_interrupted()?;
    if interrupted > 0 {
        tracing::info!("上次退出时有 {interrupted} 条回复没写完，已标记为中断");
    }
    let runs = engine.recover_runs()?;
    if runs > 0 {
        tracing::info!("上次退出时有 {runs} 个任务没做完，会接着做");
    }
    if interrupted > 0 || runs > 0 {
        let summary = format!(
            "上次没有正常退出：{interrupted} 条回复写到一半，标成了中断；{runs} 个任务没做完，会接着做"
        );
        engine.audit(Audit::system("recovery.done", summary));
    }
    // 审计记录留半年，太旧的在启动时清掉。
    match engine.prune_audit() {
        Ok(0) => {}
        Ok(pruned) => tracing::info!("清掉了 {pruned} 条 {AUDIT_KEEP_DAYS} 天以前的审计记录"),
        Err(error) => tracing::warn!("清理旧的审计记录时出错：{error}"),
    }
    Ok((engine, lock))
}

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
            "这个数据目录正被另一个 sa 进程（sa serve 或 sa chat）使用：请先停掉它，或用 --data-dir 换一个目录。sa serve 运行时，Bot 和房间请在界面的「成员」面板里管理"
        ),
        Err(std::fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("无法锁定 {}", path.display()))
        }
    }
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
