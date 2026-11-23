//! 编排层：房间里来了消息，找到该接话的 Bot，跑一轮对话，把过程和结果写回房间。
//!
//! 每个有活动的房间一个后台任务。房间里每个 Bot 一条道：几个 Bot 可以同时说话（有上限，多的排队），
//! 每个 Bot 同时只说一轮；不同房间互不等待。
//!
//! 谁来接话：人 `@` 了谁就是谁；没有点名时，房间里只有一个 Bot 就是它，
//! 有几个就让便宜的模型按分工挑一个（见 [`router`]）。Bot 之间用 `ask_bot` 互相点名，
//! 次数和转手的上限见 [`episode`]。

mod agent;
mod ask;
mod budget;
mod deliver;
mod episode;
mod extensions;
mod github;
mod room;
mod router;
mod routines;
mod serial;
mod telegram;
mod watchdog;
mod work;
mod workdir;

pub use budget::{Budget, Price};
pub use episode::{MAX_BOT_TURNS, MAX_HOP, MAX_PAIR_ASKS};
pub use github::{GitHub, DEFAULT_API_BASE};
pub use routines::Routines;
pub use sa_adapters::telegram::Telegram;
pub use telegram::TelegramOptions;
pub use watchdog::Watchdog;
pub use work::MAX_PARALLEL_RUNS;

use episode::Episode;
use sa_engine::{Engine, EngineError, Incoming, Receipt};
use sa_llm::Provider;
use sa_protocol::{
    ApprovalDecision, ApprovalId, Bot, BotId, Message, MessageKind, ModelTier, RoomId, Sender,
};
use sa_tools::{Fence, ToolRegistry};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicI64;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{oneshot, Notify};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const DEFAULT_PROMPT: &str = include_str!("../prompts/chat.md");
const TEAM_RULES: &str = include_str!("../prompts/team.md");
const WORK_PROMPT: &str = include_str!("../prompts/work.md");

/// 一个档位实际用哪个模型。
#[derive(Clone)]
pub struct ResolvedModel {
    pub provider: Arc<dyn Provider>,
    pub model: String,
    pub thinking: Option<bool>,
    /// 这个模型的上下文窗口（token）。不填就用模型服务报的大小。
    pub context_window: Option<usize>,
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
    /// 任务的独立工作目录放在哪。
    pub work_dir: PathBuf,
    /// 怎么连 GitHub（开 PR 用）。`None` 表示不提供开 PR 的工具。
    pub github: Option<GitHub>,
    /// 花费的上限和价格。
    pub budget: Budget,
    /// 看门狗看什么、多严。
    pub watchdog: Watchdog,
    /// 定时任务多久看一遍、等多久。
    pub routines: Routines,
    /// 一个房间里最多几个 Bot 同时说话。多的排队。
    pub max_parallel_bots: usize,
}

/// 一个房间里默认最多 3 个 Bot 同时说话。
pub const DEFAULT_MAX_PARALLEL_BOTS: usize = 3;

/// 审批卡默认等 10 分钟。
pub const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error("服务正在关闭")]
    ShuttingDown,
    /// 现在做不了（比如任务的改动带不回工作目录）。文字是给人看的原因。
    #[error("{0}")]
    Conflict(String),
}

pub struct Orchestrator {
    shared: Arc<Shared>,
    rooms: Mutex<HashMap<RoomId, RoomHandle>>,
}

pub(crate) struct Shared {
    pub engine: Arc<Engine>,
    pub models: Arc<dyn ModelRouter>,
    /// 每个 Bot 都有的工具。连上外部 MCP 服务之后会变多，所以每一轮开始时取一份当时的。
    pub tools: std::sync::RwLock<ToolRegistry>,
    /// 技能放在哪个目录。没有设就是没有技能。
    pub skills_dir: Mutex<Option<PathBuf>>,
    /// 外部 MCP 服务，和它们现在怎么样。
    pub mcp: Mutex<Vec<extensions::Mcp>>,
    pub settings: Settings,
    /// 触发后所有房间停下手里的事并退出。
    pub shutdown: CancellationToken,
    /// 正在等人答复的审批。
    pub approvals: Mutex<HashMap<ApprovalId, oneshot::Sender<ApprovalDecision>>>,
    /// 正在跑的任务。
    pub works: work::Works,
    /// 这个进程在租约上签的名字。
    pub owner: String,
    /// 连上 Telegram 之后，机器人在那边的用户名。
    pub telegram_username: Mutex<Option<String>>,
    /// 正在做定时任务的房间。一个房间里的定时任务一个一个来。
    pub routine_rooms: Mutex<std::collections::HashSet<RoomId>>,
}

struct RoomHandle {
    link: Arc<RoomLink>,
    task: JoinHandle<()>,
}

/// 房间的后台任务和外面（发消息、停止、`ask_bot`）共用的状态。
/// 说话的、排队的、是不是在挑人。
pub(crate) type Announced = (Vec<BotId>, Vec<BotId>, bool, i64);

pub(crate) struct RoomLink {
    /// 正在说话的 Bot 和各自的取消开关，按开始的先后排。几个 Bot 可以同时说，每个 Bot 同时只说一轮。
    pub lanes: Mutex<Vec<(BotId, CancellationToken)>>,
    /// 已经开始写回复的那几个 Bot（有道但还没开口的不算），按开口的先后排。
    pub speaking: Mutex<Vec<BotId>>,
    /// 最近一次告诉外面的「谁在说、谁在排、是不是在挑人」：没变就不再发。
    pub announced: Mutex<Option<Announced>>,
    /// 正在挑由谁来接话时，那次挑人的取消开关。
    pub routing: Mutex<Option<CancellationToken>>,
    /// 对话里改文件、执行命令的那一下要拿着它：同一个房间里的 Bot 一个一个来，不会同时改同一个工作目录。
    pub writing: tokio::sync::Mutex<()>,
    /// 叫醒房间的后台任务（有新的 Bot 要开始说话时）。
    pub wake: Arc<Notify>,
    episode: Mutex<Episode>,
    /// 人发的消息已经处理到哪一条（`seq`）。`-1` 表示还没从会话记录里算出来。
    pub handled: AtomicI64,
    /// 次数上限按人说的哪一条重新算过了：同一句话被几个 Bot 先后读到，只重新算一次。
    pub renewed: AtomicI64,
    /// 房间的后台任务是不是正在干活（挑人、Bot 说话、排着队）。
    pub active: std::sync::atomic::AtomicBool,
}

impl RoomLink {
    fn new() -> Self {
        Self {
            lanes: Mutex::default(),
            speaking: Mutex::default(),
            announced: Mutex::new(Some((vec![], vec![], false, 0))),
            routing: Mutex::default(),
            writing: tokio::sync::Mutex::new(()),
            wake: Arc::new(Notify::new()),
            episode: Mutex::new(Episode::default()),
            handled: AtomicI64::new(-1),
            renewed: AtomicI64::new(0),
            active: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn episode(&self) -> std::sync::MutexGuard<'_, Episode> {
        self.episode.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 正在说话的 Bot，按开始的先后排。
    pub fn running(&self) -> Vec<BotId> {
        let lanes = self.lanes.lock().unwrap();
        lanes.iter().map(|(bot_id, _)| bot_id.clone()).collect()
    }

    /// 有没有 Bot 在说话、或者正在挑人。
    pub fn busy(&self) -> bool {
        !self.lanes.lock().unwrap().is_empty() || self.routing.lock().unwrap().is_some()
    }
}

impl Orchestrator {
    pub fn new(engine: Arc<Engine>, models: Arc<dyn ModelRouter>, settings: Settings) -> Self {
        Self {
            shared: Arc::new(Shared {
                engine,
                models,
                tools: std::sync::RwLock::new(ToolRegistry::standard()),
                skills_dir: Mutex::default(),
                mcp: Mutex::default(),
                settings,
                shutdown: CancellationToken::new(),
                approvals: Mutex::new(HashMap::new()),
                works: work::Works::default(),
                owner: format!("sa-{}", sa_protocol::RoutineId::new()),
                telegram_username: Mutex::default(),
                routine_rooms: Mutex::default(),
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
        self.arrived(&message);
        Ok(message)
    }

    /// 外面送来一件事（GitHub 事件、Telegram 里的一句话）。收下的成了房间里的消息，回应在后台进行。
    pub async fn receive(&self, incoming: Incoming) -> Result<Receipt, OrchestratorError> {
        if self.shared.shutdown.is_cancelled() {
            return Err(OrchestratorError::ShuttingDown);
        }
        let engine = self.shared.engine.clone();
        let receipt = room::db(move || engine.receive(incoming)).await?;
        if let Receipt::Stored(message) = &receipt {
            self.arrived(message);
        }
        Ok(receipt)
    }

    /// 机器人在 Telegram 上的用户名。还没连上时是 `None`。
    pub fn telegram_username(&self) -> Option<String> {
        self.shared.telegram_username.lock().unwrap().clone()
    }

    /// 启动时调用：上次退出时还没有 Bot 接的话（人说的、外面来的事），现在接着回应。
    /// 返回叫醒了几个房间。
    ///
    /// 只看每个房间的最后一条消息：它是人说的或者外面来的事，就叫醒房间，由房间自己从会话记录里
    /// 算出哪些话还没有 Bot 看过。已经有 Bot 看到、回到一半中断的不重来：那一轮可能已经动过手了，
    /// 重做一遍不安全，中断的回复留在时间线上由人决定。到点没做成的定时任务也不补做。
    pub fn resume_rooms(&self) -> Result<usize, EngineError> {
        let mut woken = 0;
        for room in self.shared.engine.list_rooms()? {
            if room.host_bot_id.is_none() {
                continue;
            }
            let page = self.shared.engine.list_messages(&room.id, None, 1)?;
            let waiting = page.messages.last().is_some_and(|last| {
                last.sender == Sender::User
                    && matches!(last.kind, MessageKind::Chat | MessageKind::Event)
            });
            if waiting {
                self.wake(&room.id);
                woken += 1;
            }
        }
        Ok(woken)
    }

    /// 房间里来了一条要有人回应的消息：叫醒房间。不打断正在说话的 Bot：
    /// 点名找别的 Bot 的，被点到的会同时开始；没点名的，正在说的 Bot 在下一步会看到。
    fn arrived(&self, message: &Message) {
        self.wake(&message.room_id);
    }

    /// 停下房间里所有正在进行的回复，排着队的 Bot 也不再说话。返回是否真的有东西被停下。
    pub fn stop(&self, room_id: &RoomId) -> bool {
        let rooms = self.rooms.lock().unwrap();
        let Some(handle) = rooms.get(room_id) else {
            return false;
        };
        let link = &handle.link;
        let queued = {
            let mut episode = link.episode();
            let queued = !episode.queued().is_empty();
            episode.clear_queue();
            queued
        };
        let mut stopped = queued;
        if let Some(cancel) = link.routing.lock().unwrap().as_ref() {
            cancel.cancel();
            stopped = true;
        }
        for (_, cancel) in link.lanes.lock().unwrap().iter() {
            cancel.cancel();
            stopped = true;
        }
        stopped
    }

    /// 只停下房间里这一个 Bot：它正在说的停下，它排着的不再说。别的 Bot 照常。
    pub fn stop_bot(&self, room_id: &RoomId, bot_id: &BotId) -> bool {
        let rooms = self.rooms.lock().unwrap();
        let Some(handle) = rooms.get(room_id) else {
            return false;
        };
        let link = &handle.link;
        let queued = {
            let mut episode = link.episode();
            let queued = episode.queued().contains(bot_id);
            episode.drop_bot(bot_id);
            queued
        };
        let speaking = {
            let lanes = link.lanes.lock().unwrap();
            let lane = lanes.iter().find(|(id, _)| id == bot_id);
            lane.inspect(|(_, cancel)| cancel.cancel()).is_some()
        };
        if queued {
            // 排队的情况变了：让房间重新报一次。
            link.wake.notify_one();
        }
        speaking || queued
    }

    /// 人答复一张审批卡。返回 `false` 表示这张卡已经不在等了（答过了、过期了，或者那一轮已经停了）。
    pub fn decide(&self, id: &ApprovalId, decision: ApprovalDecision) -> bool {
        let waiting = self.shared.approvals.lock().unwrap().remove(id);
        match waiting {
            Some(reply) => reply.send(decision).is_ok(),
            None => false,
        }
    }

    /// 「用量」面板的内容：今天和最近几天花了多少，按模型、Bot、任务分。
    pub async fn usage_summary(&self) -> Result<sa_protocol::UsageSummary, OrchestratorError> {
        Ok(budget::summary(&self.shared).await?)
    }

    /// 今天的花费是不是到了每日上限：到了的话排队的任务不会开工。
    pub async fn daily_limit_reached(&self) -> bool {
        budget::daily_exhausted(&self.shared).await
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
        work::shutdown(&self.shared).await;
        extensions::shutdown(&self.shared).await;
    }

    fn wake(&self, room_id: &RoomId) {
        let mut rooms = self.rooms.lock().unwrap();
        let handle = rooms.entry(room_id.clone()).or_insert_with(|| {
            let link = Arc::new(RoomLink::new());
            let task = tokio::spawn(room::run(
                self.shared.clone(),
                room_id.clone(),
                link.clone(),
            ));
            RoomHandle { link, task }
        });
        // notify_one 会存一个许可：房间正忙时也不会漏掉这次唤醒。
        handle.link.wake.notify_one();
    }
}

/// 这个 Bot 在这个房间里的系统提示词。`others` 是房间里别的 Bot。
///
/// 自定义的提示词里没有写 `{persona}`、`{team}` 时，这两段接在最后：换了提示词，协作规则也不会丢。
///
/// `skills` 是这个 Bot 能用的技能那一段（没有就是空的），同样可以用 `{skills}` 指定放在哪。
pub(crate) fn render_prompt(
    settings: &Settings,
    bot: &Bot,
    others: &[Bot],
    skills: &str,
) -> String {
    let template = settings.prompt.as_deref().unwrap_or(DEFAULT_PROMPT);
    let persona = if bot.persona.is_empty() {
        String::new()
    } else {
        format!("你的分工和人设：\n{}", bot.persona)
    };
    let team = if others.is_empty() {
        String::new()
    } else {
        let mut members = String::new();
        for other in others {
            let role: String = other
                .persona
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(100)
                .collect();
            let role = if role.is_empty() {
                "（没有写分工）"
            } else {
                &role
            };
            members.push_str(&format!("- {}：{role}\n", other.name));
        }
        TEAM_RULES.replace("{members}", members.trim_end())
    };
    let mut prompt = template
        .replace("{bot_name}", &bot.name)
        .replace("{workspace}", &settings.fence.root_display())
        .replace("{persona}", &persona)
        .replace("{team}", &team)
        .replace("{skills}", skills);
    let skills = skills.to_string();
    for (placeholder, block) in [
        ("{persona}", persona),
        ("{team}", team),
        ("{skills}", skills),
    ] {
        if !template.contains(placeholder) && !block.is_empty() {
            prompt = format!("{}\n\n{block}", prompt.trim_end());
        }
    }
    prompt.trim_end().to_string() + "\n"
}

/// 这个 Bot 在任务的工作会话里的系统提示词。`fence` 是任务的独立目录。
pub(crate) fn render_work_prompt(bot: &Bot, fence: &Fence, skills: &str) -> String {
    let persona = if bot.persona.is_empty() {
        String::new()
    } else {
        format!("你的分工和人设：\n{}", bot.persona)
    };
    let prompt = WORK_PROMPT
        .replace("{bot_name}", &bot.name)
        .replace("{workspace}", &fence.root_display())
        .replace("{persona}", &persona);
    let mut prompt = prompt.trim_end().to_string();
    if !skills.is_empty() {
        prompt = format!("{prompt}\n\n{skills}");
    }
    prompt + "\n"
}

/// 所有档位都用同一个模型。测试和 `--mock` 用。
pub struct SingleModel(pub ResolvedModel);

impl ModelRouter for SingleModel {
    fn resolve(&self, _tier: ModelTier) -> Result<ResolvedModel, String> {
        Ok(self.0.clone())
    }
}
