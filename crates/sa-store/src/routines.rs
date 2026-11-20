//! 定时任务和它们的租约。

use crate::{Result, StoreError};
use rusqlite::{params, Connection, OptionalExtension, Row};
use sa_protocol::{BotId, RoomId, Routine, RoutineId, RoutineResult};

const COLUMNS: &str = "id, room_id, bot_id, title, prompt, schedule, enabled, next_at, last_at, \
                       last_result, last_note, failures, created_at, updated_at";

pub fn insert(conn: &Connection, routine: &Routine) -> Result<()> {
    conn.execute(
        "INSERT INTO routines (id, room_id, bot_id, title, prompt, schedule, enabled, next_at, last_at,
                               last_result, last_note, failures, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            routine.id.as_str(),
            routine.room_id.as_str(),
            routine.bot_id.as_str(),
            routine.title,
            routine.prompt,
            routine.schedule,
            routine.enabled,
            routine.next_at,
            routine.last_at,
            routine.last_result.map(RoutineResult::as_str),
            routine.last_note,
            routine.failures,
            routine.created_at,
            routine.updated_at,
        ],
    )?;
    Ok(())
}

/// 整条覆盖（租约不动）。没有这一条时返回 `false`。
pub fn update(conn: &Connection, routine: &Routine) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE routines SET bot_id = ?2, title = ?3, prompt = ?4, schedule = ?5, enabled = ?6,
                next_at = ?7, last_at = ?8, last_result = ?9, last_note = ?10, failures = ?11,
                updated_at = ?12
         WHERE id = ?1",
        params![
            routine.id.as_str(),
            routine.bot_id.as_str(),
            routine.title,
            routine.prompt,
            routine.schedule,
            routine.enabled,
            routine.next_at,
            routine.last_at,
            routine.last_result.map(RoutineResult::as_str),
            routine.last_note,
            routine.failures,
            routine.updated_at,
        ],
    )?;
    Ok(changed > 0)
}

pub fn delete(conn: &Connection, id: &RoutineId) -> Result<bool> {
    Ok(conn.execute("DELETE FROM routines WHERE id = ?1", [id.as_str()])? > 0)
}

pub fn get(conn: &Connection, id: &RoutineId) -> Result<Option<Routine>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM routines WHERE id = ?1"),
        [id.as_str()],
        read,
    )
    .optional()?
    .transpose()
}

/// 全部定时任务，先建的在前。
pub fn list(conn: &Connection) -> Result<Vec<Routine>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM routines ORDER BY created_at, id"
    ))?;
    let rows = stmt.query_map([], read)?;
    rows.map(|row| row?).collect()
}

/// 已经到点、而且没有被别人占着的定时任务。
pub fn due(conn: &Connection, now: i64) -> Result<Vec<Routine>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM routines
         WHERE enabled = 1 AND next_at IS NOT NULL AND next_at <= ?1
           AND (lease_owner IS NULL OR lease_until <= ?1)
         ORDER BY next_at, id"
    ))?;
    let rows = stmt.query_map([now], read)?;
    rows.map(|row| row?).collect()
}

/// 抢租约：没人占着、或者占着的人过期了才抢得到。
pub fn claim(conn: &Connection, id: &RoutineId, owner: &str, now: i64, until: i64) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE routines SET lease_owner = ?2, lease_until = ?4
         WHERE id = ?1 AND (lease_owner IS NULL OR lease_until <= ?3)",
        params![id.as_str(), owner, now, until],
    )?;
    Ok(changed > 0)
}

/// 还租约。只有自己占着的才还得了（过期后被别人接手的不动）。
pub fn release(conn: &Connection, id: &RoutineId, owner: &str) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE routines SET lease_owner = NULL, lease_until = NULL
         WHERE id = ?1 AND lease_owner = ?2",
        params![id.as_str(), owner],
    )?;
    Ok(changed > 0)
}

/// 现在是谁占着。
pub fn lease(conn: &Connection, id: &RoutineId, now: i64) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT lease_owner FROM routines WHERE id = ?1 AND lease_until > ?2",
            params![id.as_str(), now],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

fn read(row: &Row<'_>) -> rusqlite::Result<Result<Routine>> {
    let id: String = row.get(0)?;
    let last_result = match row.get::<_, Option<String>>(9)? {
        None => None,
        Some(text) => match RoutineResult::parse(&text) {
            Some(result) => Some(result),
            None => {
                return Ok(Err(StoreError::Corrupt(format!(
                    "定时任务 {id} 上一次的结果未知：{text}"
                ))))
            }
        },
    };
    Ok(Ok(Routine {
        id: RoutineId::from(id),
        room_id: RoomId::from(row.get::<_, String>(1)?),
        bot_id: BotId::from(row.get::<_, String>(2)?),
        title: row.get(3)?,
        prompt: row.get(4)?,
        schedule: row.get(5)?,
        enabled: row.get(6)?,
        next_at: row.get(7)?,
        last_at: row.get(8)?,
        last_result,
        last_note: row.get(10)?,
        failures: row.get(11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bots, rooms, Store};
    use sa_protocol::{Bot, BotKind, ModelTier, Room};

    fn routine(room: &RoomId, bot: &BotId, next_at: i64) -> Routine {
        Routine {
            id: RoutineId::new(),
            room_id: room.clone(),
            bot_id: bot.clone(),
            title: "早报".into(),
            prompt: "看看有什么新消息".into(),
            schedule: "0 9 * * *".into(),
            enabled: true,
            next_at: Some(next_at),
            last_at: None,
            last_result: None,
            last_note: String::new(),
            failures: 0,
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn a_lease_has_one_owner_until_it_expires_or_is_released() {
        let store = Store::open_in_memory().unwrap();
        let (room, bot) = seed(&store);
        let item = routine(&room, &bot, 100);
        store.write(|tx| insert(tx, &item)).unwrap();

        store
            .write(|tx| {
                assert!(due(tx, 99)?.is_empty());
                assert_eq!(due(tx, 100)?, vec![item.clone()]);
                assert!(claim(tx, &item.id, "a", 100, 200)?);
                // 被占着的不算到点，别人也抢不到。
                assert!(due(tx, 150)?.is_empty());
                assert!(!claim(tx, &item.id, "b", 150, 250)?);
                assert_eq!(lease(tx, &item.id, 150)?.as_deref(), Some("a"));
                // 别人还不了。
                assert!(!release(tx, &item.id, "b")?);
                // 过期之后别人可以接手，原来的主人就还不了了。
                assert_eq!(due(tx, 200)?.len(), 1);
                assert!(claim(tx, &item.id, "b", 200, 300)?);
                assert!(!release(tx, &item.id, "a")?);
                assert!(release(tx, &item.id, "b")?);
                assert_eq!(lease(tx, &item.id, 250)?, None);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn update_keeps_the_lease_and_delete_follows_the_bot() {
        let store = Store::open_in_memory().unwrap();
        let (room, bot) = seed(&store);
        let mut item = routine(&room, &bot, 100);
        store
            .write(|tx| {
                insert(tx, &item)?;
                assert!(claim(tx, &item.id, "a", 100, 200)?);
                item.enabled = false;
                item.next_at = None;
                item.last_result = Some(RoutineResult::Silent);
                item.failures = 2;
                assert!(update(tx, &item)?);
                assert_eq!(get(tx, &item.id)?, Some(item.clone()));
                assert_eq!(lease(tx, &item.id, 150)?.as_deref(), Some("a"));
                assert_eq!(list(tx)?, vec![item.clone()]);
                bots::delete(tx, &bot)?;
                assert_eq!(get(tx, &item.id)?, None);
                assert!(!delete(tx, &item.id)?);
                Ok(())
            })
            .unwrap();
    }

    fn seed(store: &Store) -> (RoomId, BotId) {
        let bot = Bot {
            id: BotId::new(),
            name: "助手".into(),
            kind: BotKind::Assistant,
            tier: ModelTier::Mid,
            persona: String::new(),
            enabled: true,
            created_at: 1,
        };
        let room = Room {
            id: RoomId::new(),
            name: "房间".into(),
            host_bot_id: Some(bot.id.clone()),
            member_bot_ids: vec![bot.id.clone()],
            created_at: 1,
            updated_at: 1,
        };
        store
            .write(|tx| {
                bots::insert(tx, &bot)?;
                rooms::insert(tx, &room)
            })
            .unwrap();
        (room.id, bot.id)
    }
}
