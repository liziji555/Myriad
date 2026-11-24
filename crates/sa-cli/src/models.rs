//! 把配置里的模型档位换成能调用的模型。

use crate::config::{Config, ProviderKind};
use sa_llm::{Anthropic, MockProvider, OpenAiCompat, Provider};
use sa_orchestrator::{ModelRouter, ResolvedModel};
use sa_protocol::ModelTier;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

type Env = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

pub struct ConfigModels {
    config: Config,
    env: Env,
    /// 每个模型服务只建一个客户端，连接可以复用。
    clients: Mutex<HashMap<String, Arc<dyn Provider>>>,
}

impl ConfigModels {
    pub fn new(config: Config) -> Self {
        Self::with_env(config, Box::new(|name| std::env::var(name).ok()))
    }

    pub fn with_env(config: Config, env: Env) -> Self {
        Self {
            config,
            env,
            clients: Mutex::default(),
        }
    }
}

impl ModelRouter for ConfigModels {
    fn resolve(&self, tier: ModelTier) -> Result<ResolvedModel, String> {
        let tiers = &self.config.tiers;
        let choice = match tier {
            ModelTier::Cheap => &tiers.cheap,
            ModelTier::Mid => &tiers.mid,
            ModelTier::Strong => &tiers.strong,
        };
        let name = &choice.provider;
        let provider = self
            .config
            .providers
            .get(name)
            .ok_or_else(|| format!("config.toml 里没有模型服务「{name}」"))?;
        let mut clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        let client = match clients.get(name) {
            Some(client) => client.clone(),
            None => {
                let var = &provider.api_key_env;
                let key = (self.env)(var)
                    .map(|key| key.trim().to_string())
                    .filter(|key| !key.is_empty())
                    .ok_or_else(|| {
                        format!(
                            "没有模型密钥：请把「{name}」的 API Key 放进环境变量 {var}，然后重新启动。\
                             只想先看看流程的话，可以加 --mock 用不联网的假模型。"
                        )
                    })?;
                let invalid = |e| format!("模型服务「{name}」配置有误：{e}");
                let header = provider.session_header.as_deref();
                let client: Arc<dyn Provider> = match provider.kind {
                    ProviderKind::OpenaiCompat => {
                        let mut client =
                            OpenAiCompat::new(&provider.base_url, key).map_err(invalid)?;
                        if let Some(header) = header {
                            client = client.with_session_header(header).map_err(invalid)?;
                        }
                        Arc::new(client)
                    }
                    ProviderKind::Anthropic => {
                        let mut client =
                            Anthropic::new(&provider.base_url, key).map_err(invalid)?;
                        if let Some(header) = header {
                            client = client.with_session_header(header).map_err(invalid)?;
                        }
                        Arc::new(client)
                    }
                };
                clients.insert(name.clone(), client.clone());
                client
            }
        };
        Ok(ResolvedModel {
            provider: client,
            model: choice.model.clone(),
            thinking: choice.thinking,
            context_window: choice.context_window,
        })
    }
}

/// `--mock`：所有档位都用不联网的假模型。
///
/// `context_window` 用来在演示时把窗口调小，几条消息就能看到自动压缩。
pub fn mock(context_window: Option<usize>) -> sa_orchestrator::SingleModel {
    sa_orchestrator::SingleModel(ResolvedModel {
        provider: Arc::new(MockProvider::demo()),
        model: "mock".into(),
        thinking: None,
        context_window,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_CONFIG_TOML;

    fn models(text: &str, key: Option<&'static str>) -> ConfigModels {
        ConfigModels::with_env(
            Config::parse(text).unwrap(),
            Box::new(move |name| {
                assert_eq!(name, "DEEPSEEK_API_KEY");
                key.map(str::to_string)
            }),
        )
    }

    #[test]
    fn resolves_each_tier_to_its_configured_model() {
        let models = models(DEFAULT_CONFIG_TOML, Some("sk-test"));
        let cheap = models.resolve(ModelTier::Cheap).unwrap();
        assert_eq!(cheap.model, "deepseek-flash");
        assert_eq!(cheap.thinking, Some(false));
        let strong = models.resolve(ModelTier::Strong).unwrap();
        assert_eq!(strong.model, "deepseek-v4-pro");
        assert_eq!(strong.thinking, None);
        // 同一个模型服务共用一个客户端。
        assert!(Arc::ptr_eq(&cheap.provider, &strong.provider));
    }

    #[test]
    fn missing_or_blank_key_names_the_variable_and_the_way_out() {
        for key in [None, Some("  ")] {
            let err = models(DEFAULT_CONFIG_TOML, key)
                .resolve(ModelTier::Mid)
                .err()
                .unwrap();
            assert!(
                err.contains("DEEPSEEK_API_KEY") && err.contains("--mock"),
                "{err}"
            );
        }
    }

    #[test]
    fn anthropic_kind_resolves_to_a_client() {
        let text =
            DEFAULT_CONFIG_TOML.replace(r#"kind = "openai_compat""#, r#"kind = "anthropic""#);
        let mid = models(&text, Some("sk-test"))
            .resolve(ModelTier::Mid)
            .unwrap();
        assert_eq!(mid.model, "deepseek-flash");
        assert_eq!(
            mid.provider.capabilities(&mid.model).context_window,
            200_000
        );
    }
}
