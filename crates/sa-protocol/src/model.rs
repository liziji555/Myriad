use crate::{
    ApprovalId, ArtifactId, BotId, MessageId, Millis, RoomId, RuleId, RunId, TaskId,
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Room {
    pub id: RoomId,
    pub name: String,
    /// 房间的入口 Bot：没有 @ 任何人时由它接话。
    pub host_bot_id: Option<BotId>,
    pub member_bot_ids: Vec<BotId>,
    #[ts(type = "number")]
    pub created_at: Millis,
    #[ts(type = "number")]
    pub updated_at: Millis,
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
}

str_enum!(MessageKind {
    Chat => "chat",
    Task => "task",
    Run => "run",
    Delivery => "delivery",
});

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
    pub speaking: Option<BotId>,
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
            for to in [Draft, Open, Working, Waiting, Review, Blocked, Done, Dropped, Cancelled] {
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
