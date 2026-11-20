//! 给人看的提醒。

use crate::{Result, StoreError};
use rusqlite::{params, Connection, OptionalExtension, Row};
use sa_protocol::{Notice, NoticeId, NoticeKind, NoticeLevel, RoomId, TaskId};

const COLUMNS: &str = "id, kind, level, room_id, task_id, text, created_at, dismissed_at";

/// 记一条提醒。`key` 相同的已经记过就不再记，返回 `false`。
pub fn insert(conn: &Connection, key: &str, notice: &Notice) -> Result<bool> {
    let added = conn.execute(
        "INSERT INTO notices (id, key, kind, level, room_id, task_id, text, created_at, dismissed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT (key) DO NOTHING",
        params![
            notice.id.as_str(),
            key,
            notice.kind.as_str(),
            notice.level.as_str(),
            notice.room_id.as_ref().map(RoomId::as_str),
            notice.task_id.as_ref().map(TaskId::as_str),
            notice.text,
            notice.created_at,
            notice.dismissed_at,
        ],
    )?;
    Ok(added > 0)
}

pub fn get(conn: &Connection, id: &NoticeId) -> Result<Option<Notice>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM notices WHERE id = ?1"),
        [id.as_str()],
        read,
    )
    .optional()?
    .transpose()
}

/// 还没被收起来的提醒，新的在前。
pub fn list_open(conn: &Connection, limit: u32) -> Result<Vec<Notice>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM notices WHERE dismissed_at IS NULL ORDER BY created_at DESC, id DESC LIMIT ?1"
    ))?;
    let rows = stmt.query_map([limit], read)?;
    rows.map(|row| row?).collect()
}

/// 收起一条提醒。已经收起的不改，返回 `false`。
pub fn dismiss(conn: &Connection, id: &NoticeId, now: i64) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE notices SET dismissed_at = ?2 WHERE id = ?1 AND dismissed_at IS NULL",
        params![id.as_str(), now],
    )?;
    Ok(changed > 0)
}

fn read(row: &Row<'_>) -> rusqlite::Result<Result<Notice>> {
    let id: String = row.get(0)?;
    let kind: String = row.get(1)?;
    let level: String = row.get(2)?;
    let (Some(kind), Some(level)) = (NoticeKind::parse(&kind), NoticeLevel::parse(&level)) else {
        return Ok(Err(StoreError::Corrupt(format!(
            "提醒 {id} 的种类或级别未知：{kind} / {level}"
        ))));
    };
    Ok(Ok(Notice {
        id: NoticeId::from(id),
        kind,
        level,
        room_id: row.get::<_, Option<String>>(3)?.map(RoomId::from),
        task_id: row.get::<_, Option<String>>(4)?.map(TaskId::from),
        text: row.get(5)?,
        created_at: row.get(6)?,
        dismissed_at: row.get(7)?,
    }))
}
