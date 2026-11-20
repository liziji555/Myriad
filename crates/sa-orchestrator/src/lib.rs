//! 编排层：房间里来了消息，找到该接话的 Bot，跑一轮对话，把过程和结果写回房间。
//!
//! 每个有活动的房间一个后台任务，房间里的对话一轮一轮串行；不同房间互不等待。
//! 这一版每个房间只有入口 Bot 会回应。

mod room;

use sa_engine::{Engine, EngineError};
use sa_llm::Provider;
use sa_protocol::{ApprovalDecision, ApprovalId, Message, ModelTier, RoomId};
use sa_tools::{Fence, ToolRegistry};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{oneshot, Notify};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const DEFAULT_PROMPT: &str = include_str!("../prompts/chat.md");

/// 一个档位实际用哪个模型。
#[derive(Clone)]
pub struct ResolvedModel {
    pub provider: Arc<dyn Provider>,
    pub model: String,
    pub thinking: Option<bool>,
}

/// 把模型档位换成能调用的模型。失败时返回给人看的原因（比如没有配置密钥）。
pub trait ModelRouter: Send + Sync {
    fn resolve(&self, tier: ModelTier) -> Result<ResolvedModel, String>;
}

#[derive(Debug, Clone)]
pub struct Settings {
    /// 会话记录放在哪个目录。
    pub sessions_dir: PathBuf,
    /// 文件类工具能看到的范围。
    pub fence: Fence,
    /// 系统提示词模板；`None` 用内置的。支持 `{bot_name}` 和 `{workspace}`。
    pub prompt: Option<String>,
    /// 一轮里最多调用几次模型。
    pub max_steps: usize,
    /// 允许 `web_fetch` 访问本机和内网。只在测试里打开。
    pub allow_private_network: bool,
    /// 审批卡等多久没人处理就按拒绝算。
    pub approval_timeout: Duration,
}

/// 审批卡默认等 10 分钟。
pub const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error("服务正在关闭")]
    ShuttingDown,
}

pub struct Orchestrator {
    shared: Arc<Shared>,
    rooms: Mutex<HashMap<RoomId, RoomHandle>>,
}

pub(crate) struct Shared {
    pub engine: Arc<Engine>,
    pub models: Arc<dyn ModelRouter>,
    pub tools: ToolRegistry,
    pub settings: Settings,
    /// 触发后所有房间停下手里的事并退出。
    pub shutdown: CancellationToken,
    /// 正在等人答复的审批。
    pub approvals: Mutex<HashMap<ApprovalId, oneshot::Sender<ApprovalDecision>>>,
}

struct RoomHandle {
    wake: Arc<Notify>,
    /// 正在跑的那一轮的取消开关；空闲时是 `None`。
    current: Arc<Mutex<Option<CancellationToken>>>,
    task: JoinHandle<()>,
}

impl Orchestrator {
    pub fn new(engine: Arc<Engine>, models: Arc<dyn ModelRouter>, settings: Settings) -> Self {
        Self {
            shared: Arc::new(Shared {
                engine,
                models,
                tools: ToolRegistry::standard(),
                settings,
                shutdown: CancellationToken::new(),
                approvals: Mutex::new(HashMap::new()),
            }),
            rooms: Mutex::new(HashMap::new()),
        }
    }

    /// 人在房间里发一条消息。消息落库后立刻返回，回复在后台进行。
    pub async fn post_message(
        &self,
        room_id: &RoomId,
        content: String,
    ) -> Result<Message, OrchestratorError> {
        if self.shared.shutdown.is_cancelled() {
            return Err(OrchestratorError::ShuttingDown);
        }
        let engine = self.shared.engine.clone();
        let room = room_id.clone();
        let message = room::db(move || {
            let room = engine.get_room(&room)?;
            if room.host_bot_id.is_none() {
                return Err(EngineError::Invalid(
                    "这个房间里还没有 Bot，发了也没人回应".into(),
                ));
            }
            engine.post_user_message(&room.id, &content)
        })
        .await?;
        self.wake(room_id);
        Ok(message)
    }

    /// 停下房间里正在进行的回复。返回是否真的有东西被停下。
    pub fn stop(&self, room_id: &RoomId) -> bool {
        let rooms = self.rooms.lock().unwrap();
        let Some(handle) = rooms.get(room_id) else {
            return false;
        };
        let current = handle.current.lock().unwrap();
        match current.as_ref() {
            Some(cancel) => {
                cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// 人答复一张审批卡。返回 `false` 表示这张卡已经不在等了（答过了、过期了，或者那一轮已经停了）。
    pub fn decide(&self, id: &ApprovalId, decision: ApprovalDecision) -> bool {
        let waiting = self.shared.approvals.lock().unwrap().remove(id);
        match waiting {
            Some(reply) => reply.send(decision).is_ok(),
            None => false,
        }
    }

    /// 停下所有房间并等它们收尾。正在写的回复会标成「中断」。
    pub async fn shutdown(&self) {
        self.shared.shutdown.cancel();
        let handles: Vec<RoomHandle> = self
            .rooms
            .lock()
            .unwrap()
            .drain()
            .map(|(_, handle)| handle)
            .collect();
        for handle in handles {
            let _ = handle.task.await;
        }
    }

    fn wake(&self, room_id: &RoomId) {
        let mut rooms = self.rooms.lock().unwrap();
        let handle = rooms.entry(room_id.clone()).or_insert_with(|| {
            let wake = Arc::new(Notify::new());
            let current = Arc::new(Mutex::new(None));
            let task = tokio::spawn(room::run(
                self.shared.clone(),
                room_id.clone(),
                wake.clone(),
                current.clone(),
            ));
            RoomHandle {
                wake,
                current,
                task,
            }
        });
        // notify_one 会存一个许可：房间正忙时也不会漏掉这次唤醒。
        handle.wake.notify_one();
    }
}

pub(crate) fn render_prompt(settings: &Settings, bot_name: &str) -> String {
    settings
        .prompt
        .as_deref()
        .unwrap_or(DEFAULT_PROMPT)
        .replace("{bot_name}", bot_name)
        .replace("{workspace}", &settings.fence.root_display())
}

/// 所有档位都用同一个模型。测试和 `--mock` 用。
pub struct SingleModel(pub ResolvedModel);

impl ModelRouter for SingleModel {
    fn resolve(&self, _tier: ModelTier) -> Result<ResolvedModel, String> {
        Ok(self.0.clone())
    }
}
