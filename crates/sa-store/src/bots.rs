use crate::{Result, StoreError};
use rusqlite::{params, Connection, Row};
use sa_protocol::{Bot, BotId, BotKind, Harness, ModelTier};

pub fn insert(conn: &Connection, bot: &Bot) -> Result<()> {
    conn.execute(
        "INSERT INTO bots (id, name, kind, tier, persona, enabled, created_at, harness, harness_command, is_master, role, avatar_shape, avatar_color, avatar_image) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            bot.id.as_str(),
            bot.name,
            bot.kind.as_str(),
            bot.tier.as_str(),
            bot.persona,
            bot.enabled,
            bot.created_at,
            bot.harness.as_str(),
            bot.harness_command,
            bot.is_master,
            bot.role,
            bot.avatar_shape,
            bot.avatar_color,
            bot.avatar_image
        ],
    )?;
    Ok(())
}

pub fn list(conn: &Connection) -> Result<Vec<Bot>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, kind, tier, enabled, created_at, persona, harness, harness_command, is_master, role, avatar_shape, avatar_color, avatar_image FROM bots ORDER BY created_at, id",
    )?;
    let rows = stmt.query_map([], read_row)?;
    rows.map(|row| row?).collect()
}

pub fn get(conn: &Connection, id: &BotId) -> Result<Option<Bot>> {
    Ok(list(conn)?.into_iter().find(|bot| &bot.id == id))
}

/// 用 `bot` 的可变部分（名字、档位、人设、是否启用、形象与主控身份）覆盖已有的那一行。
pub fn update(conn: &Connection, bot: &Bot) -> Result<()> {
    let changed = conn.execute(
        "UPDATE bots SET name = ?2, tier = ?3, persona = ?4, enabled = ?5, harness = ?6, harness_command = ?7, is_master = ?8, role = ?9, avatar_shape = ?10, avatar_color = ?11, avatar_image = ?12 WHERE id = ?1",
        params![
            bot.id.as_str(),
            bot.name,
            bot.tier.as_str(),
            bot.persona,
            bot.enabled,
            bot.harness.as_str(),
            bot.harness_command,
            bot.is_master,
            bot.role,
            bot.avatar_shape,
            bot.avatar_color,
            bot.avatar_image
        ],
    )?;
    if changed == 0 {
        return Err(StoreError::Corrupt(format!("Bot {} 不存在", bot.id)));
    }
    Ok(())
}

/// 删掉一个 Bot。它的房间成员关系和会话跟着删；它发过的消息留着。返回是否真的删了。
pub fn delete(conn: &Connection, id: &BotId) -> Result<bool> {
    Ok(conn.execute("DELETE FROM bots WHERE id = ?1", [id.as_str()])? > 0)
}

pub fn count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM bots", [], |row| row.get(0))?)
}

fn read_row(row: &Row<'_>) -> rusqlite::Result<Result<Bot>> {
    let kind: String = row.get(2)?;
    let tier: String = row.get(3)?;
    let harness: String = row.get(7)?;
    let (Some(kind), Some(tier), Some(harness)) = (
        BotKind::parse(&kind),
        ModelTier::parse(&tier),
        Harness::parse(&harness),
    ) else {
        return Ok(Err(StoreError::Corrupt(format!(
            "bots 表里有未知的 kind/tier/harness：{kind}/{tier}/{harness}"
        ))));
    };
    Ok(Ok(Bot {
        id: BotId::from(row.get::<_, String>(0)?),
        name: row.get(1)?,
        kind,
        tier,
        enabled: row.get(4)?,
        created_at: row.get(5)?,
        persona: row.get(6)?,
        harness,
        harness_command: row.get(8)?,
        is_master: row.get(9)?,
        role: row.get(10)?,
        avatar_shape: row.get(11)?,
        avatar_color: row.get(12)?,
        avatar_image: row.get(13)?,
    }))
}
