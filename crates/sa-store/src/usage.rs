use crate::Result;
use rusqlite::{params, Connection};
use sa_protocol::{BotId, Millis, RoomId, SessionId, TaskId};

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
    /// 按当时的价格算下来花了多少，单位是百万分之一个货币单位。
    pub cost_micros: i64,
    pub created_at: Millis,
}

/// 若干次调用加起来的用量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sum {
    pub cost_micros: i64,
    pub in_tokens: i64,
    pub out_tokens: i64,
    pub cached_tokens: i64,
    pub calls: i64,
}

/// 分组统计时按什么分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    Model,
    /// 不属于任何 Bot 的调用（挑选由谁接话）归在空字符串下。
    Bot,
    /// 只算任务里的调用。
    Task,
}

const SUMS: &str = "COALESCE(SUM(u.cost_micros), 0), COALESCE(SUM(u.in_tokens), 0), COALESCE(SUM(u.out_tokens), 0), COALESCE(SUM(u.cached_tokens), 0), COUNT(*)";

pub fn insert(conn: &Connection, row: &UsageRow) -> Result<()> {
    conn.execute(
        // 是哪个任务花的在这时就记死：会话以后可能跟着 Bot 一起被删。
        "INSERT INTO usage (room_id, bot_id, session_id, model, in_tokens, out_tokens, cached_tokens, cost_micros, created_at, task_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, (SELECT task_id FROM sessions WHERE id = ?3))",
        params![
            row.room_id.as_ref().map(RoomId::as_str),
            row.bot_id.as_ref().map(BotId::as_str),
            row.session_id.as_ref().map(SessionId::as_str),
            row.model,
            row.in_tokens,
            row.out_tokens,
            row.cached_tokens,
            row.cost_micros,
            row.created_at
        ],
    )?;
    Ok(())
}

/// `[from, to)` 这段时间里所有调用的总和。
pub fn sum_between(conn: &Connection, from: Millis, to: Millis) -> Result<Sum> {
    Ok(conn.query_row(
        &format!("SELECT {SUMS} FROM usage u WHERE u.created_at >= ?1 AND u.created_at < ?2"),
        [from, to],
        |row| read_sum(row, 0),
    )?)
}

/// `since` 之后一共花了多少。
pub fn cost_since(conn: &Connection, since: Millis) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COALESCE(SUM(cost_micros), 0) FROM usage WHERE created_at >= ?1",
        [since],
        |row| row.get(0),
    )?)
}

/// 一个任务从开始到现在一共花了多少（它派出去的子代理也算）。
pub fn task_cost(conn: &Connection, task_id: &TaskId) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COALESCE(SUM(cost_micros), 0) FROM usage WHERE task_id = ?1",
        [task_id.as_str()],
        |row| row.get(0),
    )?)
}

/// 每个任务从开始到现在一共花了多少。一次模型都没调用过的任务不在里面。
pub fn task_costs(conn: &Connection) -> Result<Vec<(TaskId, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT task_id, SUM(cost_micros) FROM usage
         WHERE task_id IS NOT NULL
         GROUP BY task_id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((TaskId::from(row.get::<_, String>(0)?), row.get(1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// `[from, to)` 这段时间里的调用分组求和，花得多的在前。
pub fn grouped(
    conn: &Connection,
    by: GroupBy,
    from: Millis,
    to: Millis,
) -> Result<Vec<(String, Sum)>> {
    let (key, join, filter) = match by {
        GroupBy::Model => ("u.model", "", ""),
        GroupBy::Bot => ("COALESCE(u.bot_id, '')", "", ""),
        GroupBy::Task => ("u.task_id", "", "AND u.task_id IS NOT NULL"),
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT {key}, {SUMS} FROM usage u {join}
         WHERE u.created_at >= ?1 AND u.created_at < ?2 {filter}
         GROUP BY 1 ORDER BY 2 DESC, 3 DESC, 1"
    ))?;
    let rows = stmt.query_map([from, to], |row| Ok((row.get(0)?, read_sum(row, 1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn read_sum(row: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<Sum> {
    Ok(Sum {
        cost_micros: row.get(at)?,
        in_tokens: row.get(at + 1)?,
        out_tokens: row.get(at + 2)?,
        cached_tokens: row.get(at + 3)?,
        calls: row.get(at + 4)?,
    })
}
