//! 对外面做过的事（推送分支、开 PR）。同一件事只做一次：做之前先查，做完记下结果。

use crate::Result;
use rusqlite::{params, Connection, OptionalExtension};
use sa_protocol::TaskId;

/// 这件事做过的话，返回当时记下的结果（JSON 文本）。
pub fn get(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT result FROM effects WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()?)
}

/// 记下一件做完的事。已经记过的不改，返回 `false`。
pub fn put(
    conn: &Connection,
    key: &str,
    task_id: &TaskId,
    kind: &str,
    result: &str,
    now: i64,
) -> Result<bool> {
    let added = conn.execute(
        "INSERT INTO effects (key, task_id, kind, result, created_at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (key) DO NOTHING",
        params![key, task_id.as_str(), kind, result, now],
    )?;
    Ok(added > 0)
}
