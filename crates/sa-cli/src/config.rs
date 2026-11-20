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
}

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
        }
        for (name, provider) in &self.providers {
            if provider.api_key_env.trim().is_empty() {
                bail!("[providers.{name}] 没有填 api_key_env");
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
