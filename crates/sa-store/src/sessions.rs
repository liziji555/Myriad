use crate::Result;
use rusqlite::{params, Connection, OptionalExtension};
use sa_protocol::{BotId, Millis, RoomId, SessionId, TaskId};

/// 某个 Bot 在某个房间里的入口会话；没有就是 `None`。只读，不新建。
pub fn find_front(
    conn: &Connection,
    room_id: &RoomId,
    bot_id: &BotId,
) -> Result<Option<SessionId>> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM sessions WHERE room_id = ?1 AND bot_id = ?2 AND layer = 'front'",
            params![room_id.as_str(), bot_id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(existing.map(SessionId::from))
}

/// 某个 Bot 在某个房间里的入口会话；没有就新建。
pub fn ensure_front(
    conn: &Connection,
    room_id: &RoomId,
    bot_id: &BotId,
    now: Millis,
) -> Result<SessionId> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM sessions WHERE room_id = ?1 AND bot_id = ?2 AND layer = 'front'",
            params![room_id.as_str(), bot_id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok(SessionId::from(id));
    }
    let id = SessionId::new();
    conn.execute(
        "INSERT INTO sessions (id, room_id, bot_id, layer, created_at) VALUES (?1, ?2, ?3, 'front', ?4)",
        params![id.as_str(), room_id.as_str(), bot_id.as_str(), now],
    )?;
    Ok(id)
}

/// 某个任务的工作会话；没有就新建。一个任务从头到尾只用这一个，换了负责人也接着用。
pub fn ensure_work(
    conn: &Connection,
    room_id: &RoomId,
    bot_id: &BotId,
    task_id: &TaskId,
    now: Millis,
) -> Result<SessionId> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM sessions WHERE task_id = ?1 AND layer = 'work'",
            [task_id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok(SessionId::from(id));
    }
    let id = SessionId::new();
    conn.execute(
        "INSERT INTO sessions (id, room_id, bot_id, layer, created_at, task_id) VALUES (?1, ?2, ?3, 'work', ?4, ?5)",
        params![id.as_str(), room_id.as_str(), bot_id.as_str(), now, task_id.as_str()],
    )?;
    Ok(id)
}
