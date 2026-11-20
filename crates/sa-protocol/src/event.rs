use crate::{Millis, RoomId};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// 事件种类。数据库和 SSE 里都用点分字符串（如 `room.created`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub enum EventKind {
    #[serde(rename = "room.created")]
    RoomCreated,
    #[serde(rename = "bot.created")]
    BotCreated,
    /// Bot 的资料改了。payload 是完整的 [`crate::Bot`]。
    #[serde(rename = "bot.updated")]
    BotUpdated,
    /// payload 是 [`crate::BotRef`]。
    #[serde(rename = "bot.deleted")]
    BotDeleted,
    /// 房间的名字、入口 Bot 或成员变了。payload 是完整的 [`crate::Room`]。
    #[serde(rename = "room.updated")]
    RoomUpdated,
    /// payload 是 [`crate::RoomActivity`]。
    #[serde(rename = "room.activity")]
    RoomActivity,
    /// payload 是 [`crate::MessageRef`]。
    #[serde(rename = "message.deleted")]
    MessageDeleted,
    /// 房间里多了一条消息。payload 是完整的 [`crate::Message`]。
    #[serde(rename = "message.created")]
    MessageCreated,
    /// 一条消息变了（工具步骤、状态、最终内容）。payload 是完整的 [`crate::Message`]。
    #[serde(rename = "message.updated")]
    MessageUpdated,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RoomCreated => "room.created",
            Self::BotCreated => "bot.created",
            Self::BotUpdated => "bot.updated",
            Self::BotDeleted => "bot.deleted",
            Self::RoomUpdated => "room.updated",
            Self::RoomActivity => "room.activity",
            Self::MessageDeleted => "message.deleted",
            Self::MessageCreated => "message.created",
            Self::MessageUpdated => "message.updated",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "room.created" => Some(Self::RoomCreated),
            "bot.created" => Some(Self::BotCreated),
            "bot.updated" => Some(Self::BotUpdated),
            "bot.deleted" => Some(Self::BotDeleted),
            "room.updated" => Some(Self::RoomUpdated),
            "room.activity" => Some(Self::RoomActivity),
            "message.deleted" => Some(Self::MessageDeleted),
            "message.created" => Some(Self::MessageCreated),
            "message.updated" => Some(Self::MessageUpdated),
            _ => None,
        }
    }
}

/// 一条已落库的事件。`seq` 全局自增，是 SSE 的续传游标。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct Event {
    #[ts(type = "number")]
    pub seq: i64,
    pub room_id: Option<RoomId>,
    pub kind: EventKind,
    #[ts(type = "unknown")]
    pub payload: serde_json::Value,
    #[ts(type = "number")]
    pub created_at: Millis,
}

/// SSE 连接建立后的第一条消息：告诉客户端当前游标，客户端据此加载快照。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct StreamReady {
    #[ts(type = "number")]
    pub cursor: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_kind_round_trips_and_matches_serde() {
        for kind in [
            EventKind::RoomCreated,
            EventKind::BotCreated,
            EventKind::BotUpdated,
            EventKind::BotDeleted,
            EventKind::RoomUpdated,
            EventKind::RoomActivity,
            EventKind::MessageDeleted,
            EventKind::MessageCreated,
            EventKind::MessageUpdated,
        ] {
            assert_eq!(EventKind::parse(kind.as_str()), Some(kind));
            assert_eq!(
                serde_json::to_string(&kind).unwrap(),
                format!("\"{}\"", kind.as_str())
            );
        }
    }
}
