use crate::{BotId, MessageId, Millis, RoomId};
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
    Running,
    Ok,
    Error,
    Cancelled,
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
    }
}
