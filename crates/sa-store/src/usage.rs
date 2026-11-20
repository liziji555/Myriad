use crate::Result;
use rusqlite::{params, Connection};
use sa_protocol::{BotId, Millis, RoomId, SessionId};

/// 一次模型调用的用量。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRow {
    pub room_id: Option<RoomId>,
    pub bot_id: Option<BotId>,
    pub session_id: Option<SessionId>,
    pub model: String,
    pub in_tokens: i64,
    pub out_tokens: i64,
    pub cached_tokens: i64,
    pub created_at: Millis,
}

pub fn insert(conn: &Connection, row: &UsageRow) -> Result<()> {
    conn.execute(
        "INSERT INTO usage (room_id, bot_id, session_id, model, in_tokens, out_tokens, cached_tokens, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            row.room_id.as_ref().map(RoomId::as_str),
            row.bot_id.as_ref().map(BotId::as_str),
            row.session_id.as_ref().map(SessionId::as_str),
            row.model,
            row.in_tokens,
            row.out_tokens,
            row.cached_tokens,
            row.created_at
        ],
    )?;
    Ok(())
}

/// `(输入, 输出)` token 的总和，`since` 之后的调用。
pub fn totals_since(conn: &Connection, since: Millis) -> Result<(i64, i64)> {
    Ok(conn.query_row(
        "SELECT COALESCE(SUM(in_tokens), 0), COALESCE(SUM(out_tokens), 0) FROM usage WHERE created_at >= ?1",
        [since],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?)
}
