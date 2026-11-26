//! 领域层：房间、Bot、消息、事件的业务规则。
//!
//! 约定：所有状态变更都经过这里，并且「改状态」和「记事件」在同一个事务里。
//! 这一层不调用模型、不发 HTTP。

mod audit;
mod budget;
pub mod cron;
mod desks;
mod external;
mod messages;
mod queue;
mod routines;
mod rules;
mod tasks;
mod team;
mod templates;

use sa_protocol::{
    Bot, BotId, BotKind, Event, EventKind, Message, MessageDelta, MessageId, Millis, ModelTier,
    Room, RoomId,
};
use sa_store::{bots, events, rooms, Store, StoreError};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, watch};

pub use audit::{
    Audit, AuditFilter, AUDIT_KEEP_DAYS, MAX_AUDIT_DETAIL_CHARS, MAX_AUDIT_SUMMARY_CHARS,
};
pub use budget::{micros_to_cost, NewNotice, TaskSpend, UsageBreakdown};
pub use desks::Desk;
pub use external::{Incoming, IncomingKind, Receipt, PAIRING_TTL_MS};
pub use messages::UsageRecord;
pub use routines::{
    RoutinePatch, MAX_ROOM_ROUTINES, MAX_ROUTINE_FAILURES, MAX_ROUTINE_TITLE_CHARS,
};
pub use sa_store::external::Outbound;
pub use sa_store::tasks::Workdir;
pub use tasks::{
    state_label, NewTask, RunOutcome, MAX_ACTIVE_TASKS, MAX_TASK_TEXT_CHARS, MAX_TASK_TITLE_CHARS,
};
pub use team::{find_mentions, BotPatch, MAX_BOT_NAME_CHARS, MAX_PERSONA_CHARS, MAX_ROOM_MEMBERS};

pub use templates::{
    parse_template, template_file_name, template_text, MAX_TEMPLATE_BOTS, MAX_TEMPLATE_BYTES,
    TEMPLATE_VERSION,
};
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

/// 新房间的名字合不合规矩。
pub(crate) fn check_room_name(name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        return Err(EngineError::Invalid("房间名不能为空".into()));
    }
    if name.chars().count() > MAX_ROOM_NAME_CHARS {
        return Err(EngineError::Invalid(format!(
            "房间名不能超过 {MAX_ROOM_NAME_CHARS} 个字"
        )));
    }
    Ok(())
}

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
                persona: String::new(),
                enabled: true,
                created_at: now,
                harness: Default::default(),
                harness_command: String::new(),
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
        self.create_room_with(&sa_protocol::CreateRoomRequest {
            name: name.to_string(),
            member_bot_ids: None,
            host_bot_id: None,
            discussion: None,
        })
    }

    /// 新建房间，支持直接指定成员 Bot 列表、入口主控 Bot 和讨论模式（如 1v1 私聊或组建多 Bot 协作房间）。
    pub fn create_room_with(&self, req: &sa_protocol::CreateRoomRequest) -> Result<Room> {
        let name = req.name.trim();
        check_room_name(name)?;
        let now = now_ms();
        let wanted_members = req.member_bot_ids.clone().unwrap_or_default();
        let wanted_host = req.host_bot_id.clone();
        let wanted_discussion = req.discussion;
        let (room, seq) = self.store.write(|tx| {
            let all_bots = bots::list(tx)?;
            let mut members: Vec<BotId> = Vec::new();
            for id in &wanted_members {
                if !all_bots.iter().any(|b| &b.id == id) {
                    return Ok(Err(EngineError::NotFound("指定的成员 Bot 不存在".into())));
                }
                if !members.contains(id) {
                    members.push(id.clone());
                }
            }
            if members.len() > team::MAX_ROOM_MEMBERS {
                return Ok(Err(EngineError::Invalid(format!(
                    "一个房间最多 {} 个 Bot",
                    team::MAX_ROOM_MEMBERS
                ))));
            }
            let host = if let Some(ref h) = wanted_host {
                if !all_bots.iter().any(|b| &b.id == h) {
                    return Ok(Err(EngineError::NotFound("指定的主控 Bot 不存在".into())));
                }
                if !members.contains(h) {
                    if members.len() >= team::MAX_ROOM_MEMBERS {
                        return Ok(Err(EngineError::Invalid(format!(
                            "一个房间最多 {} 个 Bot",
                            team::MAX_ROOM_MEMBERS
                        ))));
                    }
                    members.insert(0, h.clone());
                }
                Some(h.clone())
            } else if let Some(first) = members.first() {
                Some(first.clone())
            } else {
                let first_enabled = all_bots.into_iter().find(|b| b.enabled).map(|b| b.id);
                if let Some(ref id) = first_enabled {
                    members.push(id.clone());
                }
                first_enabled
            };
            let mut room = new_room(name.to_string(), host, now);
            room.member_bot_ids = members;
            if let Some(discussion) = wanted_discussion {
                room.discussion = discussion;
            }
            rooms::insert(tx, &room)?;
            let seq = events::append(
                tx,
                Some(&room.id),
                EventKind::RoomCreated,
                &to_json(&room),
                now,
            )?;
            Ok(Ok((room, seq)))
        })??;
        self.publish(seq);
        self.audit(
            Audit::user("room.created", format!("新建了房间「{}」", room.name)).room(&room.id),
        );
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
        discussion: Default::default(),
        rounds: sa_protocol::DEFAULT_ROUNDS,
        share: None,
        permission: Default::default(),
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

    #[test]
    fn create_room_with_sets_members_host_and_discussion() {
        let engine = engine();
        let a = engine
            .create_bot("主控", sa_protocol::ModelTier::Strong, "")
            .unwrap();
        let b = engine
            .create_bot("前端专家", sa_protocol::ModelTier::Mid, "UI")
            .unwrap();

        let room = engine
            .create_room_with(&sa_protocol::CreateRoomRequest {
                name: "前端重构组".into(),
                member_bot_ids: Some(vec![b.id.clone(), a.id.clone(), b.id.clone()]),
                host_bot_id: Some(a.id.clone()),
                discussion: Some(sa_protocol::Discussion::Lead),
            })
            .unwrap();

        assert_eq!(room.name, "前端重构组");
        assert_eq!(room.member_bot_ids, vec![b.id.clone(), a.id.clone()]);
        assert_eq!(room.host_bot_id.as_ref(), Some(&a.id));
        assert_eq!(room.discussion, sa_protocol::Discussion::Lead);

        // 从存储回读也必须保持建房时的成员顺序，不能被 BotId 字典序打乱。
        let stored = engine.get_room(&room.id).unwrap();
        assert_eq!(stored.member_bot_ids, vec![b.id, a.id]);
    }
}
