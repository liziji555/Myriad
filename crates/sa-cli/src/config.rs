use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::Path;

/// 首次运行时写入数据目录的 `config.toml`，也是各项的默认值。
pub const DEFAULT_CONFIG_TOML: &str = include_str!("default-config.toml");

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: Server,
    pub providers: BTreeMap<String, Provider>,
    pub tiers: Tiers,
    pub budget: Budget,
    /// 开 PR 用。老的配置文件里没有这一节，用默认值。
    #[serde(default)]
    pub github: GitHub,
    /// 连 Telegram 用。老的配置文件里没有这一节，用默认值。
    #[serde(default)]
    pub telegram: Telegram,
    /// 外部 MCP 服务：名字 → 怎么启动。它们的工具会加给每个 Bot。
    #[serde(default)]
    pub mcp: BTreeMap<String, McpServer>,
    /// 各个模型每一百万 token 多少钱，算花费用。`default` 是没有单独写的模型的价格。
    /// 老的配置文件里没有这一节，用内置的偏高的价格。
    #[serde(default)]
    pub prices: BTreeMap<String, Price>,
    /// 房间里的对话。老的配置文件里没有这一节，用默认值。
    #[serde(default)]
    pub rooms: Rooms,
}

/// 房间里的对话怎么跑。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rooms {
    /// 一个房间里最多几个 Bot 同时说话，多的排队。
    #[serde(default = "default_max_parallel_bots")]
    pub max_parallel_bots: usize,
}

impl Default for Rooms {
    fn default() -> Self {
        Self {
            max_parallel_bots: default_max_parallel_bots(),
        }
    }
}

fn default_max_parallel_bots() -> usize {
    sa_orchestrator::DEFAULT_MAX_PARALLEL_BOTS
}

/// 一个模型的价格：每一百万 token 多少钱，单位是 `[budget]` 的货币。
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Price {
    pub input: f64,
    /// 命中缓存的输入。不写就和 `input` 一样。
    pub cached_input: Option<f64>,
    pub output: f64,
}

/// 配置里没有写价格时用的：故意偏高，宁可早一点停下。
pub const FALLBACK_PRICE: Price = Price {
    input: 10.0,
    cached_input: None,
    output: 30.0,
};

/// 一个任务默认最多花多少。
pub const DEFAULT_TASK_LIMIT: f64 = 10.0;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GitHub {
    /// 接口地址。GitHub Enterprise 是 `https://主机/api/v3`。
    pub api_base: String,
    /// 访问令牌放在哪个环境变量里。
    pub token_env: String,
    /// `owner/name`。不写就从工作目录的 origin 地址里认。
    pub repo: Option<String>,
    /// Webhook 的密钥放在哪个环境变量里。
    pub webhook_secret_env: String,
    /// 只收带这些标签之一的新 issue。空着就都收。
    pub issue_labels: Vec<String>,
}

/// 一个外部 MCP 服务：启动一个本机的程序，通过标准输入输出和它说话。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServer {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// 给它的环境变量。
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// 把自己环境里的这几个变量原样传给它（密钥放在环境变量里，不写进配置文件）。
    #[serde(default)]
    pub env_from: Vec<String>,
    /// 这些工具不用问人，直接执行。别的每次都要批准。
    #[serde(default)]
    pub auto_approve: Vec<String>,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

fn enabled_by_default() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Telegram {
    /// 机器人的令牌放在哪个环境变量里。
    pub token_env: String,
    /// 接口地址。
    pub api_base: String,
}

impl Default for Telegram {
    fn default() -> Self {
        Self {
            token_env: "TELEGRAM_BOT_TOKEN".to_string(),
            api_base: "https://api.telegram.org".to_string(),
        }
    }
}

impl Default for GitHub {
    fn default() -> Self {
        Self {
            api_base: sa_orchestrator::DEFAULT_API_BASE.to_string(),
            token_env: "GITHUB_TOKEN".to_string(),
            repo: None,
            webhook_secret_env: "GITHUB_WEBHOOK_SECRET".to_string(),
            issue_labels: vec![],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    pub host: IpAddr,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub kind: ProviderKind,
    pub base_url: String,
    pub api_key_env: String,
    /// 把会话 ID 放在这个请求头里发给服务端。不写就不发。
    #[serde(default)]
    pub session_header: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    OpenaiCompat,
    Anthropic,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tiers {
    pub cheap: TierModel,
    pub mid: TierModel,
    pub strong: TierModel,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TierModel {
    pub provider: String,
    pub model: String,
    /// 是否开启思考模式。不写就用模型服务自己的默认值。
    #[serde(default)]
    pub thinking: Option<bool>,
    /// 这个模型的上下文窗口（token）。对话快占满时自动压缩。不写就用模型服务报的大小。
    #[serde(default)]
    pub context_window: Option<usize>,
}

/// 窗口再小，系统提示词和工具说明就放不下了。
pub const MIN_CONTEXT_WINDOW: usize = 16_000;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    /// 所有房间加起来每天最多花多少。0 表示不限。
    pub daily_limit: f64,
    /// 一个任务最多花多少。0 表示不限。老的配置文件里没有这一项。
    #[serde(default = "default_task_limit")]
    pub task_limit: f64,
    pub currency: String,
}

fn default_task_limit() -> f64 {
    DEFAULT_TASK_LIMIT
}

impl Config {
    pub fn parse(text: &str) -> Result<Self> {
        let config: Config = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读取 {} 失败", path.display()))?;
        Self::parse(&text).with_context(|| format!("{} 有误", path.display()))
    }

    /// 配置文件存在就读它，不存在就用默认值。
    pub fn load_or_default(path: &Path) -> Result<Self> {
        if path.exists() {
            Self::load(path)
        } else {
            Self::parse(DEFAULT_CONFIG_TOML)
        }
    }

    /// 启用着的 MCP 服务怎么启动。`cwd` 是它们的工作目录，`env` 用来查 `env_from` 里写的变量。
    pub fn mcp_specs(
        &self,
        cwd: &Path,
        env: impl Fn(&str) -> Option<String>,
    ) -> Vec<sa_tools::mcp::McpSpec> {
        let enabled = self.mcp.iter().filter(|(_, server)| server.enabled);
        enabled
            .map(|(name, server)| {
                let mut vars: Vec<(String, String)> = server
                    .env_from
                    .iter()
                    .filter_map(|key| Some((key.clone(), env(key)?)))
                    .collect();
                vars.extend(
                    server
                        .env
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone())),
                );
                sa_tools::mcp::McpSpec {
                    name: name.clone(),
                    command: server.command.clone(),
                    args: server.args.clone(),
                    env: vars,
                    cwd: Some(cwd.to_path_buf()),
                    auto_approve: server.auto_approve.clone(),
                }
            })
            .collect()
    }

    fn validate(&self) -> Result<()> {
        if let Some(repo) = &self.github.repo {
            let parts: Vec<&str> = repo.split('/').collect();
            if parts.len() != 2 || parts.iter().any(|part| part.trim().is_empty()) {
                bail!("[github] 的 repo 要写成 \"owner/name\"，现在是「{repo}」");
            }
        }
        for (name, server) in &self.mcp {
            let plain =
                |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_';
            if name.is_empty() || name.len() > 24 || !name.chars().all(plain) {
                bail!("[mcp.{name}] 的名字只能用小写字母、数字、横线和下划线，最多 24 个字符");
            }
            if server.command.trim().is_empty() {
                bail!("[mcp.{name}] 要写 command：启动这个服务的命令");
            }
        }
        if !self.telegram.api_base.starts_with("http") {
            bail!(
                "[telegram] 的 api_base 要是一个 http(s) 地址，现在是「{}」",
                self.telegram.api_base
            );
        }
        if !self.github.api_base.starts_with("http") {
            bail!(
                "[github] 的 api_base 要是一个 http(s) 地址，现在是「{}」",
                self.github.api_base
            );
        }
        for (tier, model) in self.tiers.iter() {
            if !self.providers.contains_key(&model.provider) {
                bail!(
                    "[tiers] 的 {tier} 引用了不存在的模型服务「{}」（[providers] 里有：{}）",
                    model.provider,
                    self.providers
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("、")
                );
            }
            if model.model.trim().is_empty() {
                bail!("[tiers] 的 {tier} 没有填 model");
            }
            if model.context_window.is_some_and(|n| n < MIN_CONTEXT_WINDOW) {
                bail!("[tiers] 的 {tier} 的 context_window 太小，至少要 {MIN_CONTEXT_WINDOW}");
            }
        }
        for (name, provider) in &self.providers {
            if provider.api_key_env.trim().is_empty() {
                bail!("[providers.{name}] 没有填 api_key_env");
            }
            if let Some(header) = &provider.session_header {
                // HTTP 请求头名字允许的字符（RFC 9110 的 token）。
                let valid = !header.is_empty()
                    && header
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c));
                if !valid {
                    bail!("[providers.{name}] 的 session_header「{header}」不是合法的请求头名字");
                }
            }
        }
        for (name, limit) in [
            ("daily_limit", self.budget.daily_limit),
            ("task_limit", self.budget.task_limit),
        ] {
            if !limit.is_finite() || limit < 0.0 {
                bail!("[budget] 的 {name} 必须是不小于 0 的数字");
            }
        }
        for (model, price) in &self.prices {
            let amounts = [price.input, price.cached_input.unwrap_or(0.0), price.output];
            if amounts.iter().any(|n| !n.is_finite() || *n < 0.0) {
                bail!("[prices] 里 {model} 的价格必须是不小于 0 的数字");
            }
        }
        Ok(())
    }
}

impl Config {
    /// 花费的上限和价格，交给编排层用。
    pub fn budget(&self) -> sa_orchestrator::Budget {
        let price = |price: &Price| sa_orchestrator::Price {
            input: price.input,
            cached_input: price.cached_input.unwrap_or(price.input),
            output: price.output,
        };
        sa_orchestrator::Budget {
            currency: self.budget.currency.clone(),
            daily_limit: self.budget.daily_limit,
            task_limit: self.budget.task_limit,
            default_price: price(self.prices.get("default").unwrap_or(&FALLBACK_PRICE)),
            prices: self
                .prices
                .iter()
                .filter(|(model, _)| model.as_str() != "default")
                .map(|(model, value)| (model.clone(), price(value)))
                .collect(),
        }
    }

    /// 启动时告诉人现在的上限是多少。
    pub fn budget_notice(&self) -> String {
        let currency = &self.budget.currency;
        let show = |limit: f64| {
            if limit > 0.0 {
                format!("{limit} {currency}")
            } else {
                "不限".to_string()
            }
        };
        format!(
            "花费上限：每天 {}，每个任务 {}。花费按 config.toml 里 [prices] 的价格估算，和模型服务实际扣的钱可能有出入。",
            show(self.budget.daily_limit),
            show(self.budget.task_limit)
        )
    }
}

impl Tiers {
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &TierModel)> {
        [
            ("cheap", &self.cheap),
            ("mid", &self.mid),
            ("strong", &self.strong),
        ]
        .into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        let config = Config::parse(DEFAULT_CONFIG_TOML).unwrap();
        assert_eq!(config.server.port, 4517);
        assert!(config.server.host.is_loopback());
        assert_eq!(config.tiers.strong.provider, "deepseek");
        assert_eq!(
            config.providers["deepseek"].kind,
            ProviderKind::OpenaiCompat
        );
    }

    #[test]
    fn tier_thinking_is_optional() {
        let config = Config::parse(DEFAULT_CONFIG_TOML).unwrap();
        assert_eq!(config.tiers.cheap.thinking, Some(false));
        assert_eq!(config.tiers.strong.thinking, None);
    }

    #[test]
    fn tier_context_window_is_optional_and_checked() {
        let config = Config::parse(DEFAULT_CONFIG_TOML).unwrap();
        assert_eq!(config.tiers.mid.context_window, None);
        let with_window = DEFAULT_CONFIG_TOML.replace(
            r#"mid = { provider = "deepseek", model = "deepseek-flash" }"#,
            r#"mid = { provider = "deepseek", model = "deepseek-flash", context_window = 128000 }"#,
        );
        let config = Config::parse(&with_window).unwrap();
        assert_eq!(config.tiers.mid.context_window, Some(128_000));
        let error = Config::parse(&with_window.replace("128000", "4000")).unwrap_err();
        assert!(
            error.to_string().contains("context_window 太小"),
            "{error:#}"
        );
    }

    #[test]
    fn provider_session_header_is_optional_and_checked() {
        let config = Config::parse(DEFAULT_CONFIG_TOML).unwrap();
        assert_eq!(config.providers["deepseek"].session_header, None);

        let with_header = DEFAULT_CONFIG_TOML.replace(
            r#"api_key_env = "DEEPSEEK_API_KEY""#,
            "api_key_env = \"DEEPSEEK_API_KEY\"\nsession_header = \"x-opencode-session\"",
        );
        let config = Config::parse(&with_header).unwrap();
        assert_eq!(
            config.providers["deepseek"].session_header.as_deref(),
            Some("x-opencode-session")
        );
        for valid in ["x.session-id", "X_Session~1"] {
            let text = with_header.replace("x-opencode-session", valid);
            let config = Config::parse(&text).unwrap();
            assert_eq!(
                config.providers["deepseek"].session_header.as_deref(),
                Some(valid)
            );
        }
        for invalid in ["x session", "x:session", "会话", ""] {
            let text = with_header.replace("x-opencode-session", invalid);
            let error = Config::parse(&text).unwrap_err().to_string();
            assert!(error.contains("session_header"), "{invalid}: {error}");
        }
    }

    #[test]
    fn rejects_tier_pointing_at_unknown_provider() {
        let text = DEFAULT_CONFIG_TOML.replace(
            r#"strong = { provider = "deepseek""#,
            r#"strong = { provider = "nope""#,
        );
        let err = Config::parse(&text).unwrap_err().to_string();
        assert!(err.contains("strong") && err.contains("nope"), "{err}");
    }

    #[test]
    fn rejects_unknown_keys_and_negative_budget() {
        let typo = DEFAULT_CONFIG_TOML.replace("port = 4517", "prot = 4517");
        assert!(Config::parse(&typo).is_err());
        let negative = DEFAULT_CONFIG_TOML.replace("daily_limit = 50.0", "daily_limit = -1.0");
        assert!(Config::parse(&negative).is_err());
    }

    #[test]
    fn load_or_default_falls_back_when_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(
            Config::load_or_default(&path).unwrap(),
            Config::parse(DEFAULT_CONFIG_TOML).unwrap()
        );
        std::fs::write(&path, "not toml [").unwrap();
        assert!(Config::load_or_default(&path).is_err());
    }

    #[test]
    fn configs_written_before_outside_links_still_load() {
        // M5 及以前生成的配置：[github] 里没有 Webhook 的两项，也没有 [telegram]。
        let cut = DEFAULT_CONFIG_TOML.find("# 接收 GitHub 事件").unwrap();
        let old = &DEFAULT_CONFIG_TOML[..cut];
        assert!(old.contains("[github]") && !old.contains("telegram"));
        let config = Config::parse(old).unwrap();
        assert_eq!(config.github.webhook_secret_env, "GITHUB_WEBHOOK_SECRET");
        assert!(config.github.issue_labels.is_empty());
        assert_eq!(config.telegram, Telegram::default());
        assert_eq!(config, Config::parse(DEFAULT_CONFIG_TOML).unwrap());

        let bad = DEFAULT_CONFIG_TOML.replace("https://api.telegram.org", "api.telegram.org");
        let error = Config::parse(&bad).unwrap_err();
        assert!(format!("{error:#}").contains("[telegram]"), "{error:#}");
    }

    #[test]
    fn mcp_servers_are_read_checked_and_turned_into_specs() {
        assert!(Config::parse(DEFAULT_CONFIG_TOML).unwrap().mcp.is_empty());
        let with = |extra: &str| Config::parse(&format!("{DEFAULT_CONFIG_TOML}\n{extra}"));
        let config = with(
            r#"
[mcp.files]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
env = { MODE = "ro" }
env_from = ["FILES_TOKEN", "NOT_SET"]
auto_approve = ["read_file"]

[mcp.off]
command = "uvx"
enabled = false
"#,
        )
        .unwrap();
        let env = |key: &str| (key == "FILES_TOKEN").then(|| "t0ken".to_string());
        let specs = config.mcp_specs(Path::new("/work"), env);
        // 停用的不启动；没设的环境变量不传。
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "files");
        assert_eq!(specs[0].command, "npx");
        assert_eq!(specs[0].args.len(), 3);
        assert_eq!(
            specs[0].env,
            [
                ("FILES_TOKEN".to_string(), "t0ken".to_string()),
                ("MODE".to_string(), "ro".to_string())
            ]
        );
        assert_eq!(specs[0].auto_approve, ["read_file"]);
        assert_eq!(specs[0].cwd.as_deref(), Some(Path::new("/work")));

        for (bad, needle) in [
            ("[mcp.\"My Files\"]\ncommand = \"npx\"", "名字只能用"),
            ("[mcp.files]\ncommand = \" \"", "要写 command"),
            ("[mcp.files]\nargs = []", "command"),
            ("[mcp.files]\ncommand = \"npx\"\nurl = \"http://x\"", "url"),
        ] {
            let error = format!("{:#}", with(bad).unwrap_err());
            assert!(error.contains(needle), "{bad}: {error}");
        }
    }

    #[test]
    fn configs_written_before_budgets_were_enforced_still_load() {
        // M4 及以前生成的配置：[budget] 里没有 task_limit，也没有 [prices]。
        let start = DEFAULT_CONFIG_TOML.find("[prices]").unwrap();
        let end = DEFAULT_CONFIG_TOML.find("[github]").unwrap();
        let old = format!(
            "{}{}",
            &DEFAULT_CONFIG_TOML[..start],
            &DEFAULT_CONFIG_TOML[end..]
        )
        .replace("task_limit = 10.0\n", "");
        assert!(!old.contains("task_limit") && !old.contains("[prices]"));
        let config = Config::parse(&old).unwrap();
        assert_eq!(config.budget.task_limit, DEFAULT_TASK_LIMIT);
        let budget = config.budget();
        assert_eq!(budget.daily_limit, 50.0);
        assert_eq!(budget.default_price.input, FALLBACK_PRICE.input);
        assert_eq!(budget.default_price.cached_input, FALLBACK_PRICE.input);
        assert!(budget.prices.is_empty());
    }

    #[test]
    fn prices_are_per_model_with_a_default_and_checked() {
        let text = DEFAULT_CONFIG_TOML.replace(
            r#"# "deepseek-flash" = { input = 1.0, cached_input = 0.1, output = 2.0 }"#,
            r#""deepseek-flash" = { input = 1.0, output = 2.0 }"#,
        );
        let budget = Config::parse(&text).unwrap().budget();
        assert_eq!(budget.task_limit, 10.0);
        assert_eq!(budget.default_price.output, 30.0);
        let flash = budget.prices["deepseek-flash"];
        assert_eq!(
            (flash.input, flash.cached_input, flash.output),
            (1.0, 1.0, 2.0)
        );
        assert!(!budget.prices.contains_key("default"));

        let negative = text.replace("output = 2.0", "output = -2.0");
        let error = Config::parse(&negative).unwrap_err().to_string();
        assert!(error.contains("deepseek-flash"), "{error}");
        let negative = DEFAULT_CONFIG_TOML.replace("task_limit = 10.0", "task_limit = -1");
        let error = Config::parse(&negative).unwrap_err().to_string();
        assert!(error.contains("task_limit"), "{error}");
    }

    #[test]
    fn the_startup_notice_states_the_limits() {
        let config = Config::parse(DEFAULT_CONFIG_TOML).unwrap();
        let notice = config.budget_notice();
        assert!(notice.contains("每天 50 CNY，每个任务 10 CNY"), "{notice}");
        let unlimited = DEFAULT_CONFIG_TOML.replace("daily_limit = 50.0", "daily_limit = 0");
        let notice = Config::parse(&unlimited).unwrap().budget_notice();
        assert!(notice.contains("每天 不限"), "{notice}");
    }
}
