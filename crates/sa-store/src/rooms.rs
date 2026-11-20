use crate::Result;
use rusqlite::{params, Connection};
use sa_protocol::{BotId, Room, RoomId};
use std::collections::HashMap;

/// 写入房间及其成员。
pub fn insert(conn: &Connection, room: &Room) -> Result<()> {
    conn.execute(
        "INSERT INTO rooms (id, name, host_bot_id, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            room.id.as_str(),
            room.name,
            room.host_bot_id.as_ref().map(BotId::as_str),
            room.created_at,
            room.updated_at
        ],
    )?;
    for bot_id in &room.member_bot_ids {
        conn.execute(
            "INSERT INTO room_members (room_id, bot_id, joined_at) VALUES (?1, ?2, ?3)",
            params![room.id.as_str(), bot_id.as_str(), room.created_at],
        )?;
    }
    Ok(())
}

/// 全部房间，最近活动的在前。
pub fn list(conn: &Connection) -> Result<Vec<Room>> {
    let mut members: HashMap<String, Vec<BotId>> = HashMap::new();
    let mut stmt =
        conn.prepare("SELECT room_id, bot_id FROM room_members ORDER BY joined_at, bot_id")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (room_id, bot_id) = row?;
        members
            .entry(room_id)
            .or_default()
            .push(BotId::from(bot_id));
    }

    let mut stmt = conn.prepare(
        "SELECT id, name, host_bot_id, created_at, updated_at FROM rooms ORDER BY updated_at DESC, id DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        let id: String = row.get(0)?;
        Ok(Room {
            member_bot_ids: members.remove(&id).unwrap_or_default(),
            id: RoomId::from(id),
            name: row.get(1)?,
            host_bot_id: row.get::<_, Option<String>>(2)?.map(BotId::from),
            created_at: row.get(3)?,
            updated_at: row.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn get(conn: &Connection, id: &RoomId) -> Result<Option<Room>> {
    Ok(list(conn)?.into_iter().find(|room| &room.id == id))
}

/// 改房间的名字和入口 Bot。
pub fn update(conn: &Connection, room: &Room) -> Result<()> {
    conn.execute(
        "UPDATE rooms SET name = ?2, host_bot_id = ?3, updated_at = ?4 WHERE id = ?1",
        params![
            room.id.as_str(),
            room.name,
            room.host_bot_id.as_ref().map(BotId::as_str),
            room.updated_at
        ],
    )?;
    Ok(())
}

/// 把 Bot 拉进房间。已经在里面时什么都不做。
pub fn add_member(conn: &Connection, room_id: &RoomId, bot_id: &BotId, now: i64) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO room_members (room_id, bot_id, joined_at) VALUES (?1, ?2, ?3)",
        params![room_id.as_str(), bot_id.as_str(), now],
    )?;
    Ok(())
}

/// 把 Bot 请出房间。返回它之前是不是成员。
pub fn remove_member(conn: &Connection, room_id: &RoomId, bot_id: &BotId) -> Result<bool> {
    let changed = conn.execute(
        "DELETE FROM room_members WHERE room_id = ?1 AND bot_id = ?2",
        params![room_id.as_str(), bot_id.as_str()],
    )?;
    Ok(changed > 0)
}

pub fn count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM rooms", [], |row| row.get(0))?)
}
