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
    /// 多了一个任务。payload 是完整的 [`crate::Task`]。
    #[serde(rename = "task.created")]
    TaskCreated,
    /// 任务变了（状态、负责人、交付物）。payload 是完整的 [`crate::Task`]。
    #[serde(rename = "task.updated")]
    TaskUpdated,
    /// 任务的一次运行开始或结束了。payload 是完整的 [`crate::Run`]。
    #[serde(rename = "run.updated")]
    RunUpdated,
    /// 多了一条给人看的提醒。payload 是完整的 [`crate::Notice`]。
    #[serde(rename = "notice.created")]
    NoticeCreated,
    /// 提醒被人收起来了。payload 是完整的 [`crate::Notice`]。
    #[serde(rename = "notice.updated")]
    NoticeUpdated,
    /// 多了一个定时任务。payload 是完整的 [`crate::Routine`]。
    #[serde(rename = "routine.created")]
    RoutineCreated,
    /// 定时任务变了（改了内容、启停、到点之后的结果）。payload 是完整的 [`crate::Routine`]。
    #[serde(rename = "routine.updated")]
    RoutineUpdated,
    /// 定时任务被删掉了。payload 是 [`crate::RoutineRef`]。
    #[serde(rename = "routine.deleted")]
    RoutineDeleted,
    /// 房间连上了外面一个地方。payload 是完整的 [`crate::Link`]。
    #[serde(rename = "link.created")]
    LinkCreated,
    /// 连接断开了。payload 是 [`crate::LinkRef`]。
    #[serde(rename = "link.deleted")]
    LinkDeleted,
    /// 房间的待发队列变了（多了、改了、挪了、取消了、发出去了）。payload 是 [`crate::RoomQueue`]。
    #[serde(rename = "queue.updated")]
    QueueUpdated,
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
            Self::TaskCreated => "task.created",
            Self::TaskUpdated => "task.updated",
            Self::RunUpdated => "run.updated",
            Self::NoticeCreated => "notice.created",
            Self::NoticeUpdated => "notice.updated",
            Self::RoutineCreated => "routine.created",
            Self::RoutineUpdated => "routine.updated",
            Self::RoutineDeleted => "routine.deleted",
            Self::LinkCreated => "link.created",
            Self::LinkDeleted => "link.deleted",
            Self::QueueUpdated => "queue.updated",
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
            "task.created" => Some(Self::TaskCreated),
            "task.updated" => Some(Self::TaskUpdated),
            "run.updated" => Some(Self::RunUpdated),
            "notice.created" => Some(Self::NoticeCreated),
            "notice.updated" => Some(Self::NoticeUpdated),
            "routine.created" => Some(Self::RoutineCreated),
            "routine.updated" => Some(Self::RoutineUpdated),
            "routine.deleted" => Some(Self::RoutineDeleted),
            "link.created" => Some(Self::LinkCreated),
            "link.deleted" => Some(Self::LinkDeleted),
            "queue.updated" => Some(Self::QueueUpdated),
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
            EventKind::TaskCreated,
            EventKind::TaskUpdated,
            EventKind::NoticeCreated,
            EventKind::NoticeUpdated,
            EventKind::RoutineCreated,
            EventKind::RoutineUpdated,
            EventKind::RoutineDeleted,
            EventKind::LinkCreated,
            EventKind::LinkDeleted,
            EventKind::RunUpdated,
            EventKind::QueueUpdated,
        ] {
            assert_eq!(EventKind::parse(kind.as_str()), Some(kind));
            assert_eq!(
                serde_json::to_string(&kind).unwrap(),
                format!("\"{}\"", kind.as_str())
            );
        }
    }
}
