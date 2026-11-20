use crate::config::Config;
use crate::paths::DataDir;
use std::io::ErrorKind;
use std::net::{SocketAddr, TcpListener};
use std::process::ExitCode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
}

impl Check {
    fn new(name: &'static str, status: Status, detail: impl Into<String>) -> Self {
        Self {
            name,
            status,
            detail: detail.into(),
        }
    }
}

/// `sa doctor`：只检查、不修改（除了在数据目录里试写一个临时文件）。
pub fn run(data_dir: &DataDir) -> ExitCode {
    let checks = collect(data_dir, &|name| std::env::var(name).ok());
    println!("SuperAgent 体检（版本 {}）", env!("CARGO_PKG_VERSION"));
    for check in &checks {
        let mark = match check.status {
            Status::Ok => "✓",
            Status::Warn => "!",
            Status::Fail => "✗",
        };
        println!("  {mark} {}：{}", check.name, check.detail);
    }
    let count = |status| checks.iter().filter(|c| c.status == status).count();
    let (warnings, failures) = (count(Status::Warn), count(Status::Fail));
    if failures > 0 {
        println!("结果：{failures} 项有问题，{warnings} 条提醒");
        ExitCode::FAILURE
    } else {
        println!("结果：通过，{warnings} 条提醒");
        ExitCode::SUCCESS
    }
}

pub fn collect(data_dir: &DataDir, env: &dyn Fn(&str) -> Option<String>) -> Vec<Check> {
    let mut checks = vec![check_data_dir(data_dir)];
    let config = match Config::load_or_default(&data_dir.config()) {
        Ok(config) => {
            let detail = if data_dir.config().exists() {
                format!("{} 有效", data_dir.config().display())
            } else {
                "还没有 config.toml，使用默认值（首次运行 sa serve 时生成）".to_string()
            };
            checks.push(Check::new("配置文件", Status::Ok, detail));
            Some(config)
        }
        Err(err) => {
            checks.push(Check::new("配置文件", Status::Fail, format!("{err:#}")));
            None
        }
    };
    if let Some(config) = &config {
        checks.extend(check_api_keys(config, env));
    }
    checks.push(check_git());
    if let Some(config) = &config {
        let var = &config.github.token_env;
        checks.push(match env(var) {
            Some(value) if !value.trim().is_empty() => {
                Check::new("GitHub 令牌", Status::Ok, format!("环境变量 {var} 已设置"))
            }
            _ => Check::new(
                "GitHub 令牌",
                Status::Warn,
                format!("环境变量 {var} 未设置，Bot 开不了 PR（别的不受影响）"),
            ),
        });
    }
    checks.push(check_database(data_dir));
    if let Some(config) = &config {
        checks.push(check_port(SocketAddr::new(
            config.server.host,
            config.server.port,
        )));
    }
    checks.push(if sa_server::frontend_is_embedded() {
        Check::new("前端页面", Status::Ok, "已内嵌")
    } else {
        Check::new(
            "前端页面",
            Status::Fail,
            "这个可执行文件里没有前端页面，需要先构建 web/ 再重新编译",
        )
    });
    checks
}

fn check_data_dir(data_dir: &DataDir) -> Check {
    let root = data_dir.root();
    if !root.exists() {
        return Check::new(
            "数据目录",
            Status::Warn,
            format!("{} 还不存在（首次运行 sa serve 时创建）", root.display()),
        );
    }
    let probe = root.join(format!(".doctor-{}", std::process::id()));
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Check::new(
                "数据目录",
                Status::Ok,
                format!("{}（可写）", root.display()),
            )
        }
        Err(err) => Check::new(
            "数据目录",
            Status::Fail,
            format!("{} 不可写：{err}", root.display()),
        ),
    }
}

/// 任务要用 git：给每个任务开独立目录、算改了什么、把改动带回来。
fn check_git() -> Check {
    let found = std::process::Command::new("git").arg("--version").output();
    match found {
        Ok(output) if output.status.success() => Check::new(
            "git",
            Status::Ok,
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        ),
        _ => Check::new(
            "git",
            Status::Warn,
            "没有找到 git，任务（独立目录、验收、推送）用不了；对话不受影响",
        ),
    }
}

fn check_api_keys(config: &Config, env: &dyn Fn(&str) -> Option<String>) -> Vec<Check> {
    config
        .providers
        .iter()
        // 没有档位在用的模型服务不需要密钥。
        .filter(|(name, _)| config.tiers.iter().any(|(_, tier)| &tier.provider == *name))
        .map(|(name, provider)| {
            let var = &provider.api_key_env;
            match env(var) {
                Some(value) if !value.trim().is_empty() => Check::new(
                    "模型密钥",
                    Status::Ok,
                    format!("{name}：环境变量 {var} 已设置"),
                ),
                _ => Check::new(
                    "模型密钥",
                    Status::Warn,
                    format!("{name}：环境变量 {var} 未设置，Bot 将无法调用模型"),
                ),
            }
        })
        .collect()
}

fn check_database(data_dir: &DataDir) -> Check {
    let path = data_dir.database();
    if !path.exists() {
        return Check::new(
            "数据库",
            Status::Warn,
            "还没有 state.db（首次运行 sa serve 时创建）",
        );
    }
    let latest = sa_store::LATEST_SCHEMA_VERSION;
    match sa_store::inspect(&path) {
        Err(err) => Check::new("数据库", Status::Fail, format!("无法读取：{err}")),
        Ok(report) if !report.integrity_problems.is_empty() => Check::new(
            "数据库",
            Status::Fail,
            format!("完整性检查未通过：{}", report.integrity_problems.join("；")),
        ),
        Ok(report) if report.schema_version > latest => Check::new(
            "数据库",
            Status::Fail,
            format!(
                "结构版本 {} 比本程序支持的 {latest} 新，请升级程序",
                report.schema_version
            ),
        ),
        Ok(report) if report.schema_version < latest => Check::new(
            "数据库",
            Status::Warn,
            format!(
                "结构版本 {}，下次运行 sa serve 时会升级到 {latest}",
                report.schema_version
            ),
        ),
        Ok(report) => Check::new(
            "数据库",
            Status::Ok,
            format!("结构版本 {}，完整性检查通过", report.schema_version),
        ),
    }
}

fn check_port(addr: SocketAddr) -> Check {
    match TcpListener::bind(addr) {
        Ok(_) => Check::new("端口", Status::Ok, format!("{addr} 可用")),
        Err(err) if err.kind() == ErrorKind::AddrInUse => Check::new(
            "端口",
            Status::Warn,
            format!("{addr} 已被占用（如果 sa serve 正在运行，这是正常的）"),
        ),
        Err(err) => Check::new("端口", Status::Fail, format!("无法监听 {addr}：{err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_CONFIG_TOML;

    fn find<'a>(checks: &'a [Check], name: &str) -> &'a Check {
        checks.iter().find(|c| c.name == name).unwrap()
    }

    fn data_dir(dir: &tempfile::TempDir) -> DataDir {
        DataDir::resolve(Some(dir.path().join("data"))).unwrap()
    }

    /// 配置里的端口换成系统刚分配过的空闲端口，避免和本机正在跑的服务冲突。
    fn config_with_free_port() -> String {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        DEFAULT_CONFIG_TOML.replace("port = 4517", &format!("port = {port}"))
    }

    #[test]
    fn fresh_machine_only_warns() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = data_dir(&dir);
        let checks = collect(&data_dir, &|_| None);

        assert_eq!(find(&checks, "数据目录").status, Status::Warn);
        assert_eq!(find(&checks, "配置文件").status, Status::Ok);
        assert_eq!(find(&checks, "模型密钥").status, Status::Warn);
        assert_eq!(find(&checks, "数据库").status, Status::Warn);
        assert!(!data_dir.root().exists(), "doctor 不应创建数据目录");
    }

    #[test]
    fn initialized_data_dir_passes() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = data_dir(&dir);
        std::fs::create_dir_all(data_dir.root()).unwrap();
        std::fs::write(data_dir.config(), config_with_free_port()).unwrap();
        drop(sa_store::Store::open(&data_dir.database()).unwrap());

        let checks = collect(&data_dir, &|name| {
            (name == "DEEPSEEK_API_KEY").then(|| "sk-test".to_string())
        });
        for name in ["数据目录", "配置文件", "模型密钥", "数据库", "端口"] {
            assert_eq!(find(&checks, name).status, Status::Ok, "{name}");
        }
        let leftovers: Vec<_> = std::fs::read_dir(data_dir.root())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with(".doctor-"))
            .collect();
        assert!(leftovers.is_empty(), "试写文件没有清理：{leftovers:?}");
    }

    #[test]
    fn only_providers_in_use_need_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = data_dir(&dir);
        std::fs::create_dir_all(data_dir.root()).unwrap();
        let config = format!(
            "{}\n[providers.spare]\nkind = \"openai_compat\"\nbase_url = \"https://example.com/v1\"\napi_key_env = \"SPARE_KEY\"\n",
            config_with_free_port()
        );
        std::fs::write(data_dir.config(), config).unwrap();
        let checks = collect(&data_dir, &|name| {
            (name == "DEEPSEEK_API_KEY").then(|| "sk-test".to_string())
        });
        let keys: Vec<_> = checks.iter().filter(|c| c.name == "模型密钥").collect();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].status, Status::Ok);
        assert!(keys[0].detail.contains("deepseek"));
    }

    #[test]
    fn broken_config_fails_and_skips_dependent_checks() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = data_dir(&dir);
        std::fs::create_dir_all(data_dir.root()).unwrap();
        std::fs::write(data_dir.config(), "[server]\nport = \"abc\"").unwrap();

        let checks = collect(&data_dir, &|_| None);
        assert_eq!(find(&checks, "配置文件").status, Status::Fail);
        assert!(checks
            .iter()
            .all(|c| c.name != "端口" && c.name != "模型密钥"));
    }

    #[test]
    fn corrupt_database_fails() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = data_dir(&dir);
        std::fs::create_dir_all(data_dir.root()).unwrap();
        std::fs::write(data_dir.database(), "this is not sqlite").unwrap();

        let checks = collect(&data_dir, &|_| None);
        assert_eq!(find(&checks, "数据库").status, Status::Fail);
    }

    #[test]
    fn busy_port_is_a_warning() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let check = check_port(listener.local_addr().unwrap());
        assert_eq!(check.status, Status::Warn);
    }
}
