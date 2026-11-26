use crate::{
    ApprovalId, ArtifactId, BotId, LinkId, MessageId, Millis, NoticeId, QueuedId, RoomId,
    RoutineId, RuleId, RunId, TaskId,
};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Bot 的职责类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum BotKind {
    Assistant,
    Router,
    Worker,
    Group,
    Watchdog,
}

/// 模型档位。Bot 只写档位，具体模型在 `config.toml` 里映射。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ModelTier {
    Cheap,
    Mid,
    Strong,
}

macro_rules! str_enum {
    ($ty:ident { $($variant:ident => $s:literal),+ $(,)? }) => {
        impl $ty {
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $s),+ }
            }

            pub fn parse(s: &str) -> Option<Self> {
                match s { $($s => Some(Self::$variant),)+ _ => None }
            }
        }
    };
}

str_enum!(BotKind {
    Assistant => "assistant",
    Router => "router",
    Worker => "worker",
    Group => "group",
    Watchdog => "watchdog",
});

/// Bot 由谁来干活：内置的（直接调模型），或者本机装好的某个 harness 命令行。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum Harness {
    #[default]
    Builtin,
    ClaudeCode,
    Codex,
    OpenCode,
    Gemini,
    Kimi,
    /// 支持 ACP（Agent Client Protocol）的任意 Agent CLI，例如 `cursor-agent acp`、`gemini --acp`。
    Acp,
    /// 自己指定的命令，按 Myriad 约定的格式一行一个 JSON 报告每一步。
    Command,
}

str_enum!(Harness {
    Builtin => "builtin",
    ClaudeCode => "claude_code",
    Codex => "codex",
    OpenCode => "open_code",
    Gemini => "gemini",
    Kimi => "kimi",
    Acp => "acp",
    Command => "command",
});

impl Harness {
    /// 给人看的名字。
    pub fn label(self) -> &'static str {
        match self {
            Self::Builtin => "内置",
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
            Self::OpenCode => "OpenCode",
            Self::Gemini => "Gemini CLI",
            Self::Kimi => "Kimi Code",
            Self::Acp => "ACP 通用 Agent",
            Self::Command => "自定义命令",
        }
    }

    /// 没写命令时默认跑哪个程序。内置、ACP 和自定义命令没有。
    pub fn default_program(self) -> Option<&'static str> {
        match self {
            Self::ClaudeCode => Some("claude"),
            Self::Codex => Some("codex"),
            Self::OpenCode => Some("opencode"),
            Self::Gemini => Some("gemini"),
            Self::Kimi => Some("kimi"),
            Self::Builtin | Self::Acp | Self::Command => None,
        }
    }
}

str_enum!(ModelTier {
    Cheap => "cheap",
    Mid => "mid",
    Strong => "strong",
});

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Bot {
    pub id: BotId,
    pub name: String,
    pub kind: BotKind,
    pub tier: ModelTier,
    /// 这个 Bot 的人设和分工，接在系统提示词后面。可以为空。
    #[serde(default)]
    pub persona: String,
    pub enabled: bool,
    #[ts(type = "number")]
    pub created_at: Millis,
    /// 由谁来干活。不是内置的话，`tier` 不用。
    #[serde(default)]
    pub harness: Harness,
    /// harness 的命令：程序和参数，用空格隔开。空的就用这种 harness 的默认程序。
    #[serde(default)]
    pub harness_command: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Room {
    pub id: RoomId,
    pub name: String,
    /// 房间的入口 Bot：没有 @ 任何人时由它接话。
    pub host_bot_id: Option<BotId>,
    pub member_bot_ids: Vec<BotId>,
    /// 没点名的话怎么接。
    #[serde(default)]
    pub discussion: Discussion,
    /// 轮流讨论时一句话转几圈。
    #[serde(default = "default_rounds")]
    pub rounds: u32,
    /// 房间用哪个共享目录当工作目录（别的电脑用 `sa share` 共享过来的）。没有就用服务自己的工作目录。
    #[serde(default)]
    #[ts(optional)]
    pub share: Option<String>,
    /// Bot 要改文件、执行命令时问不问人。
    #[serde(default)]
    pub permission: PermissionMode,
    #[ts(type = "number")]
    pub created_at: Millis,
    #[ts(type = "number")]
    pub updated_at: Millis,
}

/// 房间里的 Bot 要做需要批准的事时怎么办。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum PermissionMode {
    /// 每次都问人。
    #[default]
    Ask,
    /// 不问，直接做。做了什么照样记在审计里。
    Full,
    /// 规划模式：只读探索代码，先通过 `submit_plan` 提交方案等人批准后再切入执行。
    Plan,
}

str_enum!(PermissionMode {
    Ask => "ask",
    Full => "full",
    Plan => "plan",
});

/// 别的电脑用 `sa share` 共享过来的目录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Share {
    pub name: String,
    /// 目录在那台电脑上的位置，给人看的。
    pub root: String,
    /// 只能看，不能改。
    pub read_only: bool,
    /// 那台电脑现在连着。
    pub online: bool,
    /// 那台电脑允许执行命令时用哪种命令行（`bash`、`PowerShell`）。
    #[serde(default)]
    #[ts(optional)]
    pub shell: Option<String>,
}

/// 房间里没点名的话怎么接。点了名的总是只给被点到的。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum Discussion {
    /// 挑一个最合适的 Bot 接。
    #[default]
    Normal,
    /// 每个 Bot 各答一份，同时答。
    Parallel,
    /// 按成员顺序一个接一个说，后说的看得到前面的，转 `rounds` 圈。
    Rounds,
    /// 每个 Bot 各答一份，答的时候看不到别人这一次的答案。
    Blind,
    /// 由入口 Bot 当主 Agent：它把活派给成员，成员做完自动交回给它，由它决定下一步。
    Lead,
}

str_enum!(Discussion {
    Normal => "normal",
    Parallel => "parallel",
    Rounds => "rounds",
    Blind => "blind",
    Lead => "lead",
});

/// 轮流讨论默认转两圈。
pub const DEFAULT_ROUNDS: u32 = 2;
/// 共享目录的名字最多这么多字。
pub const MAX_SHARE_NAME_CHARS: usize = 40;
/// 轮流讨论最多转这么多圈。
pub const MAX_ROUNDS: u32 = 3;

fn default_rounds() -> u32 {
    DEFAULT_ROUNDS
}

/// 消息是谁发的。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum Sender {
    User,
    Bot { bot_id: BotId },
}

/// 一条消息的生命周期。人发的消息一直是 `complete`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum MessageState {
    /// Bot 还在写。
    Streaming,
    Complete,
    /// 人点了停止。
    Cancelled,
    /// 写到一半进程退出了。
    Interrupted,
    /// 出错了，原因在 `error` 里。
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum StepState {
    /// 在等人批准，见 [`ToolStep::approval`]。
    Waiting,
    Running,
    Ok,
    Error,
    Cancelled,
    /// 人没有批准（拒绝了，或者一直没处理），没有执行。
    Denied,
}

str_enum!(MessageState {
    Streaming => "streaming",
    Complete => "complete",
    Cancelled => "cancelled",
    Interrupted => "interrupted",
    Failed => "failed",
});

/// Bot 在一次回复里调用的一个工具。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ToolStep {
    pub id: String,
    pub tool: String,
    /// 给人看的一句话，例如「读取 README.md」。
    pub title: String,
    pub state: StepState,
    /// 这一步要人批准时才有。批准或拒绝之后仍然留着，方便回看当时问的是什么。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub approval: Option<StepApproval>,
    /// 这一步交回来的结果（失败时是原因），截短过，给人展开看。没有结果、被停下的步骤没有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub output: Option<String>,
}

/// 一次审批：Bot 想做一件要人点头的事。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct StepApproval {
    pub id: ApprovalId,
    /// 为什么要问，例如「要执行命令：cargo test」。
    pub reason: String,
    /// 到这个时间还没人处理就按拒绝算。
    #[ts(type = "number")]
    pub expires_at: Millis,
    /// 这张卡能不能选「这个任务里的命令都允许」：只有任务里的命令能，而且还没选过。
    #[serde(default)]
    pub can_allow_task: bool,
    /// 这张卡能不能选「总是允许」：只有能记成一条许可的操作能；共享目录里的操作每次都问。
    #[serde(default)]
    pub can_allow_always: bool,
}

/// 人对一次审批的答复。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ApprovalDecision {
    /// 只允许这一次。
    Once,
    /// 允许，并且这个房间里以后同样的操作不再问。
    Always,
    /// 允许，并且这个任务之后的命令都不再问。只对任务里的命令有意义，用在别处等于只允许这一次。
    Task,
    Deny,
}

/// 一条「总是允许」：这个房间里，这个工具做这件事不用再问。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct PermissionRule {
    pub id: RuleId,
    pub room_id: RoomId,
    pub tool: String,
    /// 具体允许的是什么：`shell` 是命令原文，写文件类工具是文件的完整路径。
    pub pattern: String,
    #[ts(type = "number")]
    pub created_at: Millis,
}

/// 待发队列里的一句话：Bot 忙的时候人说的，房间闲下来按顺序发出去。发出去之后就不在队列里了。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct QueuedInput {
    pub id: QueuedId,
    pub room_id: RoomId,
    pub content: String,
    #[ts(type = "number")]
    pub created_at: Millis,
    #[ts(type = "number")]
    pub updated_at: Millis,
}

/// 一个房间的待发队列，排在前面的先发。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct RoomQueue {
    pub room_id: RoomId,
    pub items: Vec<QueuedInput>,
}

/// 一条消息在时间线上是什么。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum MessageKind {
    /// 普通的一句话。
    #[default]
    Chat,
    /// 任务卡：Bot（或人）开了一个任务。
    Task,
    /// 任务的一次运行：过程（工具步骤、审批）都在这条消息上。
    Run,
    /// 交付卡：一次运行交出来的结果。
    Delivery,
    /// 定时任务到点了：内容是存好的那段话，`mentions` 是要做这件事的 Bot。
    /// 不是人刚说的话，Bot 没什么可说时可以不出声。
    Routine,
    /// 外面发生的事（GitHub 上新开了 issue、有人评论、CI 跑完了）：内容是给人看的一句话加原文，
    /// `mentions` 是该看这件事的 Bot；空着就只是摆在时间线上，没有 Bot 接。
    Event,
}

str_enum!(MessageKind {
    Chat => "chat",
    Task => "task",
    Run => "run",
    Delivery => "delivery",
    Routine => "routine",
    Event => "event",
});

/// 房间之外的地方。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum Source {
    Github,
    Telegram,
}

str_enum!(Source {
    Github => "github",
    Telegram => "telegram",
});

/// 一条消息是从外面哪里来的。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Origin {
    pub source: Source,
    /// 给人看的出处：Telegram 是说话的人，GitHub 是 `owner/name#12`。
    pub label: String,
    /// 原文在哪里看。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub url: Option<String>,
    /// 回话送到哪里（Telegram 的聊天 id）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub reply_to: Option<String>,
}

/// 房间和外面一个地方的连接：GitHub 仓库的事件进这个房间，Telegram 聊天里的话进这个房间。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Link {
    pub id: LinkId,
    pub source: Source,
    /// 外面那一头：GitHub 是小写的 `owner/name`，Telegram 是聊天 id。
    pub external: String,
    pub room_id: RoomId,
    /// 给人看的名字：仓库名，或者 Telegram 聊天的名字。
    pub title: String,
    #[ts(type = "number")]
    pub created_at: Millis,
}

/// 一个被断开的连接。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct LinkRef {
    pub id: LinkId,
    pub room_id: RoomId,
}

/// 把一个 Telegram 聊天连到房间用的配对码。只在生成的时候给一次。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Pairing {
    pub code: String,
    #[ts(type = "number")]
    pub expires_at: Millis,
    /// 接着配对码用的 Telegram 机器人的用户名。还没连上 Telegram 时没有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub bot_username: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Message {
    pub id: MessageId,
    /// 房间内的先后顺序，也是翻页游标。
    #[ts(type = "number")]
    pub seq: i64,
    pub room_id: RoomId,
    pub sender: Sender,
    pub content: String,
    /// 这条消息点名了哪些 Bot（人写的 `@名字`，或者 Bot 用 `ask_bot` 找的那一个）。
    #[serde(default)]
    pub mentions: Vec<BotId>,
    /// Bot 之间转了几手：人发的是 0，被人叫到的 Bot 回的是 0，它再去找别的 Bot 是 1，依此类推。
    #[serde(default)]
    pub hop: u32,
    /// 这条消息是在回哪几条：Bot 的回复是叫起它的那句话（人说的、交接给它的、成员交回来的结果），
    /// Bot 交接时说的话是它所在的那条回复。人说的话是空的。
    #[serde(default)]
    pub reply_to: Vec<MessageId>,
    #[serde(default)]
    pub kind: MessageKind,
    /// 任务卡、运行、交付卡是哪个任务的。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub task_id: Option<TaskId>,
    /// 运行和交付卡是哪一次运行的。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub run_id: Option<RunId>,
    /// 从外面来的消息是从哪里来的。在界面里直接发的没有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub origin: Option<Origin>,
    pub steps: Vec<ToolStep>,
    pub state: MessageState,
    pub error: Option<String>,
    /// 每变一次加一。客户端只接受比手上更新的版本，重复或乱序的更新不会造成回退。
    #[ts(type = "number")]
    pub rev: i64,
    #[ts(type = "number")]
    pub created_at: Millis,
    #[ts(type = "number")]
    pub updated_at: Millis,
}

/// 任务的状态。
///
/// ```text
/// draft → open → working → review → done
///                  │  ▲       │
///                  │  └───────┘ 打回重做
///                  ├→ waiting  等人回答
///                  ├→ blocked  卡住了，需要人
///                  └→ dropped  判定不需要做
/// 任何没结束的状态都可以被人取消（cancelled）。
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum TaskState {
    /// 建好了，还没有派给谁。
    Draft,
    /// 等着开工。
    Open,
    /// 有一次运行正在进行。
    Working,
    /// Bot 有问题要问人，等回答。
    Waiting,
    /// 有结果了，等人验收。
    Review,
    /// 卡住了（出错、缺权限、被人停下），需要人处理。
    Blocked,
    Done,
    /// Bot 判定这件事不需要做。
    Dropped,
    Cancelled,
}

str_enum!(TaskState {
    Draft => "draft",
    Open => "open",
    Working => "working",
    Waiting => "waiting",
    Review => "review",
    Blocked => "blocked",
    Done => "done",
    Dropped => "dropped",
    Cancelled => "cancelled",
});

impl TaskState {
    /// 已经结束，不会再动了。
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Dropped | Self::Cancelled)
    }

    /// 能不能从这个状态走到 `to`。
    pub fn can_become(self, to: TaskState) -> bool {
        use TaskState::*;
        match (self, to) {
            (from, Cancelled) => !from.is_terminal(),
            (Draft, Open) => true,
            (Open, Working | Blocked) => true,
            (Working, Review | Waiting | Blocked | Dropped | Open) => true,
            (Waiting | Blocked, Open) => true,
            (Review, Done | Open) => true,
            _ => false,
        }
    }
}

/// 交付物是什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ArtifactKind {
    /// 一个 PR，`reference` 是它的网址。
    Pr,
    /// 一个分支，`reference` 是分支名。
    Branch,
    /// 一个改过的文件，`reference` 是相对路径。
    File,
    /// 一份文字报告。
    Report,
}

str_enum!(ArtifactKind {
    Pr => "pr",
    Branch => "branch",
    File => "file",
    Report => "report",
});

/// 任务交出来的一样东西。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Artifact {
    pub id: ArtifactId,
    pub task_id: TaskId,
    pub run_id: RunId,
    pub kind: ArtifactKind,
    pub reference: String,
    /// 一句话说明，例如文件是「新增」「修改」还是「删除」。
    pub summary: String,
    #[ts(type = "number")]
    pub created_at: Millis,
}

/// 一件被派出去的工作。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Task {
    pub id: TaskId,
    pub room_id: RoomId,
    pub title: String,
    /// 要做什么。
    pub scope: String,
    /// 做到什么程度算完成。可以为空。
    pub acceptance: String,
    /// 谁负责做。
    pub owner_bot_id: Option<BotId>,
    pub state: TaskState,
    /// 对当前状态的一句说明：为什么卡住、在等什么、人打回时说了什么。
    pub note: String,
    /// 谁开的这个任务。
    pub created_by: Sender,
    /// 这个任务在哪个独立目录里干活。还没开工时是空的。
    pub workdir: Option<String>,
    /// 改动所在的分支（工作目录是 git 仓库时才有）。
    pub branch: Option<String>,
    /// 现在的交付物。
    pub artifacts: Vec<Artifact>,
    /// 每变一次加一。
    #[ts(type = "number")]
    pub rev: i64,
    #[ts(type = "number")]
    pub created_at: Millis,
    #[ts(type = "number")]
    pub updated_at: Millis,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum RunState {
    Running,
    Succeeded,
    Failed,
    /// 人停下的。
    Cancelled,
    /// 跑到一半进程退出了。
    Interrupted,
}

str_enum!(RunState {
    Running => "running",
    Succeeded => "succeeded",
    Failed => "failed",
    Cancelled => "cancelled",
    Interrupted => "interrupted",
});

/// Bot 对一次运行的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ReportStatus {
    /// 做完了，等验收。
    Done,
    /// 做不下去，需要人处理。
    Blocked,
    /// 有问题要先问人。
    NeedsInput,
    /// 这件事不需要做。
    Dropped,
}

str_enum!(ReportStatus {
    Done => "done",
    Blocked => "blocked",
    NeedsInput => "needs_input",
    Dropped => "dropped",
});

/// 一次运行结束时交的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Report {
    pub status: ReportStatus,
    /// 做了什么、结果如何；卡住或要问人时，写清楚卡在哪、想问什么。
    pub summary: String,
}

/// 任务的一次执行。一个任务同时最多有一次在进行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Run {
    pub id: RunId,
    pub task_id: TaskId,
    pub room_id: RoomId,
    pub bot_id: BotId,
    /// 房间里记录这次运行过程的那条消息。
    pub message_id: MessageId,
    pub state: RunState,
    /// 这是这个任务的第几次运行，从 1 开始。
    pub attempt: u32,
    #[ts(type = "number")]
    pub started_at: Millis,
    #[ts(type = "number | null")]
    pub ended_at: Option<Millis>,
    pub report: Option<Report>,
    pub error: Option<String>,
}

/// 房间里现在谁在说话、谁在排队。没有人说话、没有人排队、也不在挑人，表示这一轮交流结束了。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct RoomActivity {
    pub room_id: RoomId,
    /// 最先开始说话的那个 Bot。几个 Bot 同时说时看 `speakers`；留着它给旧的界面用。
    pub speaking: Option<BotId>,
    /// 正在说话的所有 Bot，按开始的先后排。
    #[serde(default)]
    pub speakers: Vec<BotId>,
    pub queued: Vec<BotId>,
    /// 正在挑由谁来接话（还没有 Bot 开始说）。这时也可以点「停止」。
    #[serde(default)]
    pub routing: bool,
    /// 房间已经处理到哪条消息（`seq`）。发消息的人据此判断自己那条有没有被处理完。
    #[ts(type = "number")]
    pub handled_seq: i64,
}

/// 一条被撤掉的消息（Bot 决定不说话时，它那条空的回复会被撤掉）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct MessageRef {
    pub id: MessageId,
    pub room_id: RoomId,
}

/// 一个被删掉的 Bot。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct BotRef {
    pub id: BotId,
}

/// 流式输出的一小段文字。不落库，只在事件流里即时转发；`rev` 是追加之后的版本号。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct MessageDelta {
    pub message_id: MessageId,
    pub room_id: RoomId,
    #[ts(type = "number")]
    pub rev: i64,
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_states_only_move_along_the_allowed_edges() {
        use TaskState::*;
        assert!(Draft.can_become(Open));
        assert!(Open.can_become(Working));
        assert!(Working.can_become(Review));
        assert!(Review.can_become(Done));
        // 打回重做。
        assert!(Review.can_become(Open));
        // 进程重启时正在做的任务回到待开工。
        assert!(Working.can_become(Open));
        assert!(Blocked.can_become(Open));
        assert!(Waiting.can_become(Open));
        // 不能跳过验收，也不能凭空开工。
        assert!(!Working.can_become(Done));
        assert!(!Draft.can_become(Working));
        assert!(!Open.can_become(Review));
        // 结束了的不再动，也不能再取消。
        for end in [Done, Dropped, Cancelled] {
            assert!(end.is_terminal());
            for to in [
                Draft, Open, Working, Waiting, Review, Blocked, Done, Dropped, Cancelled,
            ] {
                assert!(!end.can_become(to), "{end:?} -> {to:?}");
            }
        }
        for live in [Draft, Open, Working, Waiting, Review, Blocked] {
            assert!(live.can_become(Cancelled));
        }
    }

    #[test]
    fn str_enums_round_trip_and_match_serde() {
        for kind in [
            BotKind::Assistant,
            BotKind::Router,
            BotKind::Worker,
            BotKind::Group,
            BotKind::Watchdog,
        ] {
            assert_eq!(BotKind::parse(kind.as_str()), Some(kind));
            assert_eq!(
                serde_json::to_string(&kind).unwrap(),
                format!("\"{}\"", kind.as_str())
            );
        }
        for tier in [ModelTier::Cheap, ModelTier::Mid, ModelTier::Strong] {
            assert_eq!(ModelTier::parse(tier.as_str()), Some(tier));
            assert_eq!(
                serde_json::to_string(&tier).unwrap(),
                format!("\"{}\"", tier.as_str())
            );
        }
        assert_eq!(BotKind::parse("nope"), None);
        for state in [
            TaskState::Draft,
            TaskState::Open,
            TaskState::Working,
            TaskState::Waiting,
            TaskState::Review,
            TaskState::Blocked,
            TaskState::Done,
            TaskState::Dropped,
            TaskState::Cancelled,
        ] {
            assert_eq!(TaskState::parse(state.as_str()), Some(state));
            assert_eq!(
                serde_json::to_string(&state).unwrap(),
                format!("\"{}\"", state.as_str())
            );
        }
    }
}

/// 提醒是关于什么的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum NoticeKind {
    /// 花费快到或者已经到上限。
    Budget,
    /// 一次运行跑得太久。
    StuckRun,
    /// 任务很久没有动静。
    StuckTask,
    /// Bot 在重复说一样的话。
    Loop,
    /// 一个任务花得比平常多得多。
    Spend,
    /// 排队的任务太多或者等太久。
    Backlog,
    /// 最近失败的运行太多。
    Failures,
    /// 磁盘快满了或者数据库太大。
    Storage,
}

str_enum!(NoticeKind {
    Budget => "budget",
    StuckRun => "stuck_run",
    StuckTask => "stuck_task",
    Loop => "loop",
    Spend => "spend",
    Backlog => "backlog",
    Failures => "failures",
    Storage => "storage",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum NoticeLevel {
    Info,
    Warning,
}

str_enum!(NoticeLevel {
    Info => "info",
    Warning => "warning",
});

/// 给人看的一条提醒。不是谁说的话：它出现在时间线上方，可以收起来。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Notice {
    pub id: NoticeId,
    pub kind: NoticeKind,
    pub level: NoticeLevel,
    /// 关于哪个房间。`None` 表示和所有房间都有关。
    pub room_id: Option<RoomId>,
    pub task_id: Option<TaskId>,
    pub text: String,
    #[ts(type = "number")]
    pub created_at: Millis,
    #[ts(type = "number | null")]
    pub dismissed_at: Option<Millis>,
}

/// 一段时间里的模型用量。`cost` 的单位是配置里 `[budget]` 的货币。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Spend {
    pub cost: f64,
    #[ts(type = "number")]
    pub input_tokens: i64,
    #[ts(type = "number")]
    pub output_tokens: i64,
    #[ts(type = "number")]
    pub cached_tokens: i64,
    #[ts(type = "number")]
    pub calls: i64,
}

/// 用量里的一行：某一天、某个模型、某个 Bot 或某个任务。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SpendRow {
    /// 这一行是谁：日期（`2026-11-20`）、模型名、Bot 或任务的 id。
    pub key: String,
    /// 给人看的名字。
    pub label: String,
    pub spend: Spend,
}

/// 「用量」面板的全部内容。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct UsageSummary {
    pub currency: String,
    /// 每天的花费上限。0 表示不限。
    pub daily_limit: f64,
    /// 每个任务的花费上限。0 表示不限。
    pub task_limit: f64,
    pub today: Spend,
    /// 最近 7 天，从早到晚，最后一行是今天。
    pub days: Vec<SpendRow>,
    /// 今天按模型分。
    pub models: Vec<SpendRow>,
    /// 今天按 Bot 分。
    pub bots: Vec<SpendRow>,
    /// 今天花得最多的任务。
    pub tasks: Vec<SpendRow>,
}

/// 定时任务上一次到点的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum RoutineResult {
    /// Bot 说了话。
    Spoke,
    /// Bot 看过了，没什么可说。
    Silent,
    /// 没做成（模型报错、被停下）。
    Failed,
    /// 这一次没做：错过太久、今天的花费到上限了、Bot 不在房间里。
    Skipped,
}

str_enum!(RoutineResult {
    Spoke => "spoke",
    Silent => "silent",
    Failed => "failed",
    Skipped => "skipped",
});

/// 定时任务：到点让一个 Bot 在房间里做一件事。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Routine {
    pub id: RoutineId,
    pub room_id: RoomId,
    pub bot_id: BotId,
    pub title: String,
    /// 到点时交给 Bot 的那段话。
    pub prompt: String,
    /// 五段式 cron（分 时 日 月 周），按本机时间算。
    pub schedule: String,
    pub enabled: bool,
    /// 下一次什么时候到点。停用时是 `None`。
    #[ts(type = "number | null")]
    pub next_at: Option<Millis>,
    #[ts(type = "number | null")]
    pub last_at: Option<Millis>,
    pub last_result: Option<RoutineResult>,
    /// 上一次没做成或者没做的原因。
    pub last_note: String,
    /// 连续没做成的次数。
    pub failures: u32,
    #[ts(type = "number")]
    pub created_at: Millis,
    #[ts(type = "number")]
    pub updated_at: Millis,
}

/// 一个被删掉的定时任务。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct RoutineRef {
    pub id: RoutineId,
    pub room_id: RoomId,
}

/// 审计记录里，这件事是谁做的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum AuditActor {
    /// 人：在界面、命令行里做的。
    User,
    Bot,
    /// 程序自己：看门狗、预算守卫、定时器、启动时的恢复。
    System,
    /// 外面来的：GitHub、Telegram。
    Outside,
}

str_enum!(AuditActor {
    User => "user",
    Bot => "bot",
    System => "system",
    Outside => "outside",
});

/// 审计日志里的一条：谁在什么时候做了一件要留底的事。只追加，不改不删（太旧的整批清掉）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AuditEntry {
    /// 先后顺序，也是翻页游标。
    #[ts(type = "number")]
    pub id: i64,
    #[ts(type = "number")]
    pub at: Millis,
    pub actor: AuditActor,
    /// 给人看的名字：Bot 的名字、「GitHub」；人和程序自己是空的。
    pub actor_name: String,
    /// 做了什么，点分的名字，例如 `approval.decided`、`tool.shell`、`task.accepted`。
    pub action: String,
    pub room_id: Option<RoomId>,
    pub task_id: Option<TaskId>,
    /// 一句话说清楚这件事。
    pub summary: String,
    /// 细节：命令原文、参数、原因。没有就是空的。
    pub detail: Option<String>,
}

/// 一页审计记录，新的在前。`has_more` 表示更早的还有。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AuditPage {
    pub entries: Vec<AuditEntry>,
    pub has_more: bool,
}

/// harness 工位里改过的一个文件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DeskChange {
    pub path: String,
    /// 「新增」「修改」「删除」。
    pub label: String,
}

/// 入口 Bot 在这个房间里的对话占了模型上下文窗口的多少。单位是估算的 token。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ContextUsage {
    /// 现在每次调用模型要带的有多大：对话，加上这个会话上一轮带的系统提示词和工具说明
    /// （服务重启后、说第一句之前不知道这部分，只算对话）。和内核判断要不要压缩时算的是同一个数。
    pub used: u32,
    /// 到这么大就自动压缩前面的部分。
    pub compact_at: u32,
    /// 模型的上下文窗口。
    pub window: u32,
}

/// 一个 harness Bot 在房间里的工位：它在自己的独立目录里干活，改动由人决定带不带回去。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct HarnessDesk {
    pub bot_id: BotId,
    /// 独立目录在哪。还没开（或者已经带回、丢掉）时是空的。
    pub path: Option<String>,
    pub branch: Option<String>,
    /// 和开工时比，改了哪些文件。
    pub changes: Vec<DeskChange>,
}

/// 检查更新的结果：当前版本、GitHub 上最新的 Release 版本以及能不能在线更新。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct UpdateStatus {
    /// 当前正在运行的版本（不含 `v`）。
    pub current: String,
    /// GitHub 上最新 Release 的版本号（不含 `v`）。还没有 Release 时为空。
    pub latest: Option<String>,
    /// 是否有比当前更新的版本。
    pub available: bool,
    /// 最新 Release 的网页链接。
    pub release_url: Option<String>,
    /// 更新说明（截短后的摘要）。
    pub notes: Option<String>,
    /// 当前平台是否有对应二进制且支持一键替换更新。
    pub can_self_update: bool,
    /// 当前平台对应的服务端产物文件名（如 `sa-linux-x86_64`）。
    pub asset_name: Option<String>,
    /// 本次检查的时间（毫秒时间戳）。
    pub checked_at: Millis,
}

/// 触发服务端在线更新后的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct UpdateApplyResult {
    /// 刚刚替换成的版本号。
    pub updated_to: String,
    /// 服务是否正在自动重启以切到新版本。
    pub restarting: bool,
    /// 给人看的一句话说明。
    pub message: String,
}

/// 探测本机已安装的外部 Agent CLI 的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct HarnessProbe {
    pub harness: Harness,
    pub program: String,
    pub label: String,
    pub available: bool,
    pub path: Option<String>,
    pub version: Option<String>,
}

/// 独立工位里单个文件的 Unified Diff 预览。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DeskDiff {
    pub path: String,
    pub binary: bool,
    pub truncated: bool,
    pub patch: String,
}

/// 工作目录或独立工位里的一条文件/目录项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct WorkspaceEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    #[ts(type = "number")]
    pub size: u64,
}

/// 工作目录或独立工位里的单个文本文件预览。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct WorkspaceFile {
    pub path: String,
    pub binary: bool,
    pub truncated: bool,
    pub content: String,
}
