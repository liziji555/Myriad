//! 把配置里的模型档位换成能调用的模型；界面里的「模型」页也是改这里。

use crate::config::{Config, Provider as ProviderConfig, ProviderKind, TierModel, Tiers};
use crate::paths::DataDir;
use anyhow::{Context, Result};
use futures_util::future::BoxFuture;
use futures_util::StreamExt;
use sa_llm::{
    Accumulator, Anthropic, ChatMessage, ChatRequest, LlmError, MockProvider, OpenAiCompat,
    Provider,
};
use sa_orchestrator::{ModelRouter, ResolvedModel};
use sa_protocol::{
    ModelProvider, ModelProviderInput, ModelSettings, ModelTier, SaveModelsRequest,
    TestModelRequest, TestModelResult, TierChoice,
};
use sa_server::ModelAdmin;
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

type Env = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// 试一次模型最多等这么久。
const TEST_TIMEOUT: Duration = Duration::from_secs(60);

pub struct ConfigModels {
    env: Env,
    /// 设置存在哪。没有的时候（`sa chat` 之外的测试）只能看不能改。
    files: Option<Files>,
    state: Mutex<State>,
}

struct Files {
    config: PathBuf,
    keys: PathBuf,
}

struct State {
    providers: BTreeMap<String, ProviderConfig>,
    tiers: Tiers,
    /// 在界面里填的密钥：模型服务的名字 → 密钥。比环境变量优先。
    keys: BTreeMap<String, String>,
    /// 每个模型服务只建一个客户端，连接可以复用。
    clients: HashMap<String, Arc<dyn Provider>>,
}

impl ConfigModels {
    pub fn new(config: Config) -> Self {
        Self::with_env(config, Box::new(|name| std::env::var(name).ok()))
    }

    pub fn with_env(config: Config, env: Env) -> Self {
        Self {
            env,
            files: None,
            state: Mutex::new(State {
                providers: config.providers,
                tiers: config.tiers,
                keys: BTreeMap::new(),
                clients: HashMap::new(),
            }),
        }
    }

    /// 和 [`ConfigModels::new`] 一样，另外读出界面里填过的密钥，并且能把改动存回数据目录。
    pub fn open(config: Config, data_dir: &DataDir) -> Result<Self> {
        let mut models = Self::new(config);
        let keys = data_dir.model_keys();
        models
            .state
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .keys = load_keys(&keys)?;
        models.files = Some(Files {
            config: data_dir.config(),
            keys,
        });
        Ok(models)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn key(&self, state: &State, name: &str) -> Option<String> {
        let saved = state.keys.get(name).cloned();
        let from_env = || (self.env)(&state.providers.get(name)?.api_key_env);
        saved
            .or_else(from_env)
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty())
    }
}

/// 界面里填过的密钥。没有这个文件就是一个都没填过。
pub fn load_keys(path: &Path) -> Result<BTreeMap<String, String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("{} 有误", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e).with_context(|| format!("读取 {} 失败", path.display())),
    }
}

fn client(
    kind: ProviderKind,
    base_url: &str,
    key: String,
    session_header: Option<&str>,
) -> Result<Arc<dyn Provider>, LlmError> {
    Ok(match kind {
        ProviderKind::OpenaiCompat => {
            let mut client = OpenAiCompat::new(base_url, key)?;
            if let Some(header) = session_header {
                client = client.with_session_header(header)?;
            }
            Arc::new(client)
        }
        ProviderKind::Anthropic => {
            let mut client = Anthropic::new(base_url, key)?;
            if let Some(header) = session_header {
                client = client.with_session_header(header)?;
            }
            Arc::new(client)
        }
    })
}

impl ModelRouter for ConfigModels {
    fn resolve(&self, tier: ModelTier) -> Result<ResolvedModel, String> {
        let mut state = self.state();
        let choice = match tier {
            ModelTier::Cheap => &state.tiers.cheap,
            ModelTier::Mid => &state.tiers.mid,
            ModelTier::Strong => &state.tiers.strong,
        }
        .clone();
        let name = &choice.provider;
        let provider = state
            .providers
            .get(name)
            .ok_or_else(|| format!("config.toml 里没有模型服务「{name}」"))?;
        let found = match state.clients.get(name) {
            Some(client) => client.clone(),
            None => {
                let var = &provider.api_key_env;
                let key = self.key(&state, name).ok_or_else(|| {
                    format!(
                        "没有模型密钥：请在界面的「模型」页给「{name}」填上 API Key，\
                         或者把它放进环境变量 {var} 后重新启动。\
                         只想先看看流程的话，可以加 --mock 用不联网的假模型。"
                    )
                })?;
                let header = provider.session_header.as_deref();
                let client = client(provider.kind, &provider.base_url, key, header)
                    .map_err(|e| format!("模型服务「{name}」配置有误：{e}"))?;
                state.clients.insert(name.clone(), client.clone());
                client
            }
        };
        Ok(ResolvedModel {
            provider: found,
            model: choice.model,
            thinking: choice.thinking,
            context_window: choice.context_window,
        })
    }
}

fn choice(tier: &TierModel) -> TierChoice {
    TierChoice {
        provider: tier.provider.clone(),
        model: tier.model.clone(),
        thinking: tier.thinking,
        context_window: tier.context_window.and_then(|n| u32::try_from(n).ok()),
    }
}

fn tier(choice: TierChoice) -> TierModel {
    TierModel {
        provider: choice.provider.trim().to_string(),
        model: choice.model.trim().to_string(),
        thinking: choice.thinking,
        context_window: choice.context_window.map(|n| n as usize),
    }
}

impl ConfigModels {
    fn view(&self, state: &State) -> ModelSettings {
        ModelSettings {
            providers: state
                .providers
                .iter()
                .map(|(name, provider)| ModelProvider {
                    name: name.clone(),
                    kind: provider.kind,
                    base_url: provider.base_url.clone(),
                    has_key: self.key(state, name).is_some(),
                })
                .collect(),
            cheap: choice(&state.tiers.cheap),
            mid: choice(&state.tiers.mid),
            strong: choice(&state.tiers.strong),
        }
    }

    fn try_save(&self, request: SaveModelsRequest) -> Result<ModelSettings> {
        let files = self
            .files
            .as_ref()
            .context("这次运行没有可以写的配置文件")?;
        let mut state = self.state();
        let mut providers = BTreeMap::new();
        let mut keys = BTreeMap::new();
        for input in request.providers {
            let name = input.name.trim().to_string();
            let plain =
                |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_';
            anyhow::ensure!(
                !name.is_empty() && name.len() <= 24 && name.chars().all(plain),
                "模型服务的名字只能用小写字母、数字、横线和下划线，最多 24 个字符（现在是「{name}」）"
            );
            let base_url = input.base_url.trim().to_string();
            anyhow::ensure!(
                reqwest::Url::parse(&base_url)
                    .is_ok_and(|url| matches!(url.scheme(), "http" | "https")),
                "「{name}」的地址要是一个 http(s) 地址"
            );
            let old = state.providers.get(&name);
            let key = input.api_key.map(|key| key.trim().to_string());
            if let Some(key) = key.filter(|key| !key.is_empty()) {
                keys.insert(name.clone(), key);
            } else if let Some(key) = state.keys.get(&name) {
                keys.insert(name.clone(), key.clone());
            }
            let provider = ProviderConfig {
                kind: input.kind,
                base_url,
                api_key_env: old.map_or_else(
                    || format!("{}_API_KEY", name.to_uppercase().replace('-', "_")),
                    |old| old.api_key_env.clone(),
                ),
                session_header: old.and_then(|old| old.session_header.clone()),
            };
            anyhow::ensure!(
                providers.insert(name.clone(), provider).is_none(),
                "有两个模型服务都叫「{name}」"
            );
        }
        let tiers = Tiers {
            cheap: tier(request.cheap),
            mid: tier(request.mid),
            strong: tier(request.strong),
        };
        for (_, tier) in tiers.iter() {
            let name = &tier.provider;
            let has_key = |provider: &ProviderConfig| {
                let saved = keys.get(name).cloned();
                saved
                    .or_else(|| (self.env)(&provider.api_key_env))
                    .is_some_and(|key| !key.trim().is_empty())
            };
            // 不存在的模型服务下面检查整份配置时会说。
            anyhow::ensure!(
                providers.get(name).is_none_or(has_key),
                "「{name}」还没有填 API Key"
            );
        }

        let text = std::fs::read_to_string(&files.config)
            .with_context(|| format!("读取 {} 失败", files.config.display()))?;
        let text = rewrite(&text, &providers, &tiers)?;
        // 写之前按整份配置的规矩再查一遍：查不过就什么都不改。
        Config::parse(&text)?;
        // 中途断了也不能让哪份配置找不到自己的密钥：先让密钥文件新旧都有，换好配置再去掉不用的。
        let mut both = state.keys.clone();
        both.extend(keys.clone());
        replace_private(&files.keys, &toml::to_string(&both)?)?;
        replace_private(&files.config, &text)?;
        if both != keys {
            replace_private(&files.keys, &toml::to_string(&keys)?)?;
        }
        state.providers = providers;
        state.tiers = tiers;
        state.keys = keys;
        state.clients.clear();
        Ok(self.view(&state))
    }
}

/// 把配置文件里的 `[providers]` 和 `[tiers]` 换成新的，别的部分和注释原样留着。
fn rewrite(
    text: &str,
    providers: &BTreeMap<String, ProviderConfig>,
    tiers: &Tiers,
) -> Result<String> {
    use toml_edit::{value, DocumentMut, InlineTable, Item, Table};
    let mut doc: DocumentMut = text.parse().context("config.toml 有误")?;
    let table = doc
        .entry("providers")
        .or_insert_with(|| {
            let mut table = Table::new();
            table.set_implicit(true);
            Item::Table(table)
        })
        .as_table_mut()
        .context("config.toml 的 providers 不是一节")?;
    let gone: Vec<String> = table
        .iter()
        .map(|(name, _)| name.to_string())
        .filter(|name| !providers.contains_key(name))
        .collect();
    for name in gone {
        table.remove(&name);
    }
    for (name, provider) in providers {
        let entry = table
            .entry(name)
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_mut()
            .with_context(|| format!("config.toml 的 providers.{name} 不是一节"))?;
        let kind = match provider.kind {
            ProviderKind::OpenaiCompat => "openai_compat",
            ProviderKind::Anthropic => "anthropic",
        };
        entry["kind"] = value(kind);
        entry["base_url"] = value(&provider.base_url);
        entry["api_key_env"] = value(&provider.api_key_env);
    }
    let table = doc
        .entry("tiers")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .context("config.toml 的 tiers 不是一节")?;
    for (name, tier) in tiers.iter() {
        let mut inline = InlineTable::new();
        inline.insert("provider", tier.provider.as_str().into());
        inline.insert("model", tier.model.as_str().into());
        if let Some(thinking) = tier.thinking {
            inline.insert("thinking", thinking.into());
        }
        if let Some(window) = tier.context_window {
            inline.insert("context_window", (window as i64).into());
        }
        table[name] = value(inline);
    }
    Ok(doc.to_string())
}

/// 先写到旁边的临时文件再换过去：写到一半断电也不会留下半个文件。只有当前用户能读写。
fn replace_private(path: &Path, contents: &str) -> Result<()> {
    let temp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&temp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let written = options
        .open(&temp)
        .and_then(|mut file| {
            file.write_all(contents.as_bytes())
                .and_then(|()| file.sync_all())
        })
        .and_then(|()| std::fs::rename(&temp, path));
    written.with_context(|| format!("写入 {} 失败", path.display()))
}

impl ModelAdmin for ConfigModels {
    fn settings(&self) -> ModelSettings {
        self.view(&self.state())
    }

    fn save(&self, request: SaveModelsRequest) -> Result<ModelSettings, String> {
        self.try_save(request).map_err(|e| format!("{e:#}"))
    }

    fn test(&self, request: TestModelRequest) -> BoxFuture<'static, TestModelResult> {
        let ModelProviderInput {
            name,
            kind,
            base_url,
            api_key,
        } = request.provider;
        let state = self.state();
        let name = name.trim();
        let key = api_key
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty())
            .or_else(|| self.key(&state, name));
        let header = state
            .providers
            .get(name)
            .and_then(|provider| provider.session_header.clone());
        drop(state);
        let model = request.model.trim().to_string();
        Box::pin(async move {
            let failed = |message: String| TestModelResult { ok: false, message };
            let Some(key) = key else {
                return failed("还没有填 API Key".into());
            };
            if model.is_empty() {
                return failed("还没有填模型的名字".into());
            }
            let client = match client(kind, base_url.trim(), key, header.as_deref()) {
                Ok(client) => client,
                Err(error) => return failed(error.to_string()),
            };
            match tokio::time::timeout(TEST_TIMEOUT, say_hello(client, model)).await {
                Ok(Ok(reply)) => TestModelResult {
                    ok: true,
                    message: reply,
                },
                Ok(Err(error)) => failed(error.to_string()),
                Err(_) => failed(LlmError::Timeout.to_string()),
            }
        })
    }
}

/// 让模型回一句话。回了字就算通。
async fn say_hello(client: Arc<dyn Provider>, model: String) -> Result<String, LlmError> {
    let request = ChatRequest {
        model,
        messages: vec![ChatMessage::user("这是一次连接测试。请只回复两个字：收到")],
        tools: vec![],
        max_tokens: Some(256),
        thinking: None,
        session_id: None,
    };
    let mut stream = client.stream(request, CancellationToken::new()).await?;
    let mut reply = Accumulator::default();
    while let Some(event) = stream.next().await {
        reply.push(&event?);
    }
    let text = reply.text.trim();
    if text.is_empty() {
        return Err(LlmError::Protocol("模型没有回任何字".into()));
    }
    Ok(text.chars().take(200).collect())
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

    fn opened(dir: &Path) -> ConfigModels {
        let data_dir = DataDir::resolve(Some(dir.to_path_buf())).unwrap();
        std::fs::write(data_dir.config(), DEFAULT_CONFIG_TOML).unwrap();
        let config = Config::load(&data_dir.config()).unwrap();
        let mut models = ConfigModels::open(config, &data_dir).unwrap();
        models.env = Box::new(|_| None);
        models
    }

    fn pick(provider: &str, model: &str) -> TierChoice {
        TierChoice {
            provider: provider.into(),
            model: model.into(),
            thinking: None,
            context_window: None,
        }
    }

    fn request(key: Option<&str>) -> SaveModelsRequest {
        SaveModelsRequest {
            providers: vec![ModelProviderInput {
                name: "mine".into(),
                kind: ProviderKind::Anthropic,
                base_url: "https://example.com".into(),
                api_key: key.map(str::to_string),
            }],
            cheap: pick("mine", "small"),
            mid: pick("mine", "small"),
            strong: pick("mine", "big"),
        }
    }

    #[test]
    fn saving_takes_effect_at_once_and_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let models = opened(dir.path());
        assert!(models.resolve(ModelTier::Mid).is_err());

        let saved = models.save(request(Some("sk-secret"))).unwrap();
        assert_eq!(saved.providers.len(), 1);
        assert!(saved.providers[0].has_key);
        assert_eq!(models.resolve(ModelTier::Strong).unwrap().model, "big");

        // 密钥不进 config.toml；别的节和注释还在。
        let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(!text.contains("sk-secret"));
        assert!(text.contains("[github]") && text.contains("# "));
        assert!(!text.contains("[providers.deepseek]"));

        // 不重新填密钥就沿用存着的；重新打开也还在。
        models.save(request(None)).unwrap();
        let data_dir = DataDir::resolve(Some(dir.path().to_path_buf())).unwrap();
        let config = Config::load(&data_dir.config()).unwrap();
        let mut again = ConfigModels::open(config, &data_dir).unwrap();
        again.env = Box::new(|_| None);
        assert_eq!(again.resolve(ModelTier::Cheap).unwrap().model, "small");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(data_dir.model_keys())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_bad_save_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let models = opened(dir.path());
        let before = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        let mut no_key = request(None);
        assert!(models.save(no_key.clone()).unwrap_err().contains("API Key"));
        no_key.providers[0].api_key = Some("k".into());
        no_key.strong = pick("gone", "big");
        assert!(models
            .save(no_key)
            .unwrap_err()
            .contains("不存在的模型服务"));
        let mut bad_name = request(Some("k"));
        bad_name.providers[0].name = "My Model".into();
        assert!(models.save(bad_name).unwrap_err().contains("小写字母"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config.toml")).unwrap(),
            before
        );
        assert_eq!(models.settings().providers[0].name, "deepseek");
    }

    #[tokio::test]
    async fn test_reports_why_a_model_is_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let models = opened(dir.path());
        let mut request = TestModelRequest {
            provider: request(None).providers.remove(0),
            model: "small".into(),
        };
        let result = models.test(request.clone()).await;
        assert!(
            !result.ok && result.message.contains("API Key"),
            "{}",
            result.message
        );
        // 没人在听的端口：马上连不上。
        request.provider.api_key = Some("k".into());
        request.provider.base_url = "http://127.0.0.1:9".into();
        let result = models.test(request).await;
        assert!(!result.ok);
    }
}
