use crate::{Result, StoreError};
use rusqlite::{params, Connection, Row};
use sa_protocol::{Bot, BotId, BotKind, ModelTier};

pub fn insert(conn: &Connection, bot: &Bot) -> Result<()> {
    conn.execute(
        "INSERT INTO bots (id, name, kind, tier, enabled, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            bot.id.as_str(),
            bot.name,
            bot.kind.as_str(),
            bot.tier.as_str(),
            bot.enabled,
            bot.created_at
        ],
    )?;
    Ok(())
}

pub fn list(conn: &Connection) -> Result<Vec<Bot>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, kind, tier, enabled, created_at FROM bots ORDER BY created_at, id",
    )?;
    let rows = stmt.query_map([], read_row)?;
    rows.map(|row| row?).collect()
}

pub fn get(conn: &Connection, id: &BotId) -> Result<Option<Bot>> {
    Ok(list(conn)?.into_iter().find(|bot| &bot.id == id))
}

pub fn count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM bots", [], |row| row.get(0))?)
}

fn read_row(row: &Row<'_>) -> rusqlite::Result<Result<Bot>> {
    let kind: String = row.get(2)?;
    let tier: String = row.get(3)?;
    let (Some(kind), Some(tier)) = (BotKind::parse(&kind), ModelTier::parse(&tier)) else {
        return Ok(Err(StoreError::Corrupt(format!(
            "bots 表里有未知的 kind/tier：{kind}/{tier}"
        ))));
    };
    Ok(Ok(Bot {
        id: BotId::from(row.get::<_, String>(0)?),
        name: row.get(1)?,
        kind,
        tier,
        enabled: row.get(4)?,
        created_at: row.get(5)?,
    }))
}
