//! 领域层：房间、Bot、消息、事件的业务规则。
//!
//! 约定：所有状态变更都经过这里，并且「改状态」和「记事件」在同一个事务里。
//! 这一层不调用模型、不发 HTTP。

mod messages;
mod rules;

use sa_protocol::{
    Bot, BotId, BotKind, Event, EventKind, Message, MessageDelta, MessageId, Millis, ModelTier,
    Room, RoomId,
};
use sa_store::{bots, events, rooms, Store, StoreError};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, watch};

pub use messages::UsageRecord;

pub const MAX_ROOM_NAME_CHARS: usize = 60;
pub const MAX_MESSAGE_CHARS: usize = 100_000;
/// 流式片段的广播缓冲。订阅者落后超过这个数会收到 `Lagged`，应当重新取快照。
const LIVE_BUFFER: usize = 1024;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    NotFound(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub type Result<T> = std::result::Result<T, EngineError>;

pub struct Engine {
    store: Store,
    /// 最新事件的 seq。写入提交后更新，SSE 连接靠它被唤醒。
    head: watch::Sender<i64>,
    /// 正在写的 Bot 消息。流式片段只改这里并广播，不逐片落库；
    /// 步骤变化和结束时才把整条消息写回数据库。
    drafts: Mutex<HashMap<MessageId, Message>>,
    live: broadcast::Sender<MessageDelta>,
}

impl Engine {
    pub fn new(store: Store) -> Result<Self> {
        let head = store.read(events::head)?;
        Ok(Self {
            store,
            head: watch::Sender::new(head),
            drafts: Mutex::new(HashMap::new()),
            live: broadcast::Sender::new(LIVE_BUFFER),
        })
    }

    /// 全新的数据库里放一个默认 Bot 和一个默认房间，让界面打开就有东西。
    pub fn ensure_defaults(&self) -> Result<()> {
        let now = now_ms();
        let seq = self.store.write(|tx| {
            if bots::count(tx)? > 0 || rooms::count(tx)? > 0 {
                return Ok(None);
            }
            let bot = Bot {
                id: BotId::new(),
                name: "助手".into(),
                kind: BotKind::Assistant,
                tier: ModelTier::Mid,
                enabled: true,
                created_at: now,
            };
            bots::insert(tx, &bot)?;
            events::append(tx, None, EventKind::BotCreated, &to_json(&bot), now)?;

            let room = new_room("开发房间".into(), Some(bot.id), now);
            rooms::insert(tx, &room)?;
            let seq = events::append(
                tx,
                Some(&room.id),
                EventKind::RoomCreated,
                &to_json(&room),
                now,
            )?;
            Ok(Some(seq))
        })?;
        if let Some(seq) = seq {
            self.publish(seq);
        }
        Ok(())
    }

    pub fn list_rooms(&self) -> Result<Vec<Room>> {
        Ok(self.store.read(rooms::list)?)
    }

    pub fn list_bots(&self) -> Result<Vec<Bot>> {
        Ok(self.store.read(bots::list)?)
    }

    pub fn get_room(&self, id: &RoomId) -> Result<Room> {
        self.store
            .read(|c| rooms::get(c, id))?
            .ok_or_else(|| EngineError::NotFound("没有这个房间".into()))
    }

    pub fn get_bot(&self, id: &BotId) -> Result<Bot> {
        self.store
            .read(|c| bots::get(c, id))?
            .ok_or_else(|| EngineError::NotFound("没有这个 Bot".into()))
    }

    /// 新建房间。入口 Bot 取最早创建的、已启用的 Bot（没有则为空）。
    pub fn create_room(&self, name: &str) -> Result<Room> {
        let name = name.trim();
        if name.is_empty() {
            return Err(EngineError::Invalid("房间名不能为空".into()));
        }
        if name.chars().count() > MAX_ROOM_NAME_CHARS {
            return Err(EngineError::Invalid(format!(
                "房间名不能超过 {MAX_ROOM_NAME_CHARS} 个字"
            )));
        }
        let now = now_ms();
        let (room, seq) = self.store.write(|tx| {
            let host = bots::list(tx)?
                .into_iter()
                .find(|b| b.enabled)
                .map(|b| b.id);
            let room = new_room(name.to_string(), host, now);
            rooms::insert(tx, &room)?;
            let seq = events::append(
                tx,
                Some(&room.id),
                EventKind::RoomCreated,
                &to_json(&room),
                now,
            )?;
            Ok((room, seq))
        })?;
        self.publish(seq);
        Ok(room)
    }

    pub fn events_after(&self, cursor: i64, limit: u32) -> Result<Vec<Event>> {
        Ok(self.store.read(|c| events::list_after(c, cursor, limit))?)
    }

    /// 当前最新事件的 seq。
    pub fn event_head(&self) -> i64 {
        *self.head.borrow()
    }

    /// 订阅「有新事件」的通知；收到后用 [`Engine::events_after`] 取内容。
    pub fn subscribe(&self) -> watch::Receiver<i64> {
        self.head.subscribe()
    }

    fn publish(&self, seq: i64) {
        // 通知发生在释放数据库锁之后，两个写入者的通知可能乱序，所以只前进不后退。
        self.head.send_if_modified(|head| {
            let advanced = seq > *head;
            if advanced {
                *head = seq;
            }
            advanced
        });
    }
}

fn new_room(name: String, host: Option<BotId>, now: Millis) -> Room {
    Room {
        id: RoomId::new(),
        name,
        member_bot_ids: host.iter().cloned().collect(),
        host_bot_id: host,
        created_at: now,
        updated_at: now,
    }
}

fn to_json<T: serde::Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).expect("协议类型总能序列化成 JSON")
}

pub fn now_ms() -> Millis {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as Millis)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Engine {
        Engine::new(Store::open_in_memory().unwrap()).unwrap()
    }

    #[test]
    fn ensure_defaults_seeds_once() {
        let engine = engine();
        engine.ensure_defaults().unwrap();
        engine.ensure_defaults().unwrap();

        let bots = engine.list_bots().unwrap();
        let rooms = engine.list_rooms().unwrap();
        assert_eq!(bots.len(), 1);
        assert_eq!(rooms.len(), 1);
        assert_eq!(rooms[0].host_bot_id.as_ref(), Some(&bots[0].id));
        assert_eq!(rooms[0].member_bot_ids, vec![bots[0].id.clone()]);
        assert_eq!(engine.event_head(), 2);
    }

    #[test]
    fn create_room_records_event_and_wakes_subscribers() {
        let engine = engine();
        engine.ensure_defaults().unwrap();
        let before = engine.event_head();
        let mut rx = engine.subscribe();
        rx.mark_unchanged();

        let room = engine.create_room("  个人助理  ").unwrap();
        assert_eq!(room.name, "个人助理");
        assert!(room.host_bot_id.is_some());
        assert!(rx.has_changed().unwrap());

        let new_events = engine.events_after(before, 10).unwrap();
        assert_eq!(new_events.len(), 1);
        assert_eq!(new_events[0].kind, EventKind::RoomCreated);
        assert_eq!(new_events[0].room_id.as_ref(), Some(&room.id));
        assert_eq!(new_events[0].payload, to_json(&room));
        assert_eq!(engine.event_head(), new_events[0].seq);
    }

    #[test]
    fn create_room_without_bots_has_no_host() {
        let room = engine().create_room("空房间").unwrap();
        assert_eq!(room.host_bot_id, None);
        assert!(room.member_bot_ids.is_empty());
    }

    #[test]
    fn create_room_rejects_bad_names() {
        let engine = engine();
        assert!(matches!(
            engine.create_room("   "),
            Err(EngineError::Invalid(_))
        ));
        let long = "字".repeat(MAX_ROOM_NAME_CHARS + 1);
        assert!(matches!(
            engine.create_room(&long),
            Err(EngineError::Invalid(_))
        ));
        assert!(engine
            .create_room(&"字".repeat(MAX_ROOM_NAME_CHARS))
            .is_ok());
        assert_eq!(engine.event_head(), 1);
    }

    #[test]
    fn head_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let engine = Engine::new(Store::open(&path).unwrap()).unwrap();
            engine.ensure_defaults().unwrap();
        }
        let engine = Engine::new(Store::open(&path).unwrap()).unwrap();
        assert_eq!(engine.event_head(), 2);
        assert_eq!(engine.list_rooms().unwrap().len(), 1);
    }
}
