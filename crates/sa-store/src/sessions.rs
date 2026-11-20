use crate::Result;
use rusqlite::{params, Connection, OptionalExtension};
use sa_protocol::{BotId, Millis, RoomId, SessionId};

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
