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
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GitHub {
    /// 接口地址。GitHub Enterprise 是 `https://主机/api/v3`。
    pub api_base: String,
    /// 访问令牌放在哪个环境变量里。
    pub token_env: String,
    /// `owner/name`。不写就从工作目录的 origin 地址里认。
    pub repo: Option<String>,
}

impl Default for GitHub {
    fn default() -> Self {
        Self {
            api_base: sa_orchestrator::DEFAULT_API_BASE.to_string(),
            token_env: "GITHUB_TOKEN".to_string(),
            repo: None,
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
    pub daily_limit: f64,
    pub currency: String,
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

    fn validate(&self) -> Result<()> {
        if let Some(repo) = &self.github.repo {
            let parts: Vec<&str> = repo.split('/').collect();
            if parts.len() != 2 || parts.iter().any(|part| part.trim().is_empty()) {
                bail!("[github] 的 repo 要写成 \"owner/name\"，现在是「{repo}」");
            }
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
        let limit = self.budget.daily_limit;
        if !limit.is_finite() || limit < 0.0 {
            bail!("[budget] 的 daily_limit 必须是不小于 0 的数字");
        }
        Ok(())
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
}
