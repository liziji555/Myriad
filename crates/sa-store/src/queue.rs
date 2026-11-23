use crate::Result;
use rusqlite::{params, Connection, OptionalExtension};
use sa_protocol::{QueuedId, QueuedInput, RoomId};

const COLUMNS: &str = "id, room_id, content, created_at, updated_at";

/// 排到这个房间队列的最后。
pub fn push(conn: &Connection, item: &QueuedInput) -> Result<()> {
    conn.execute(
        "INSERT INTO queued_inputs (id, room_id, content, position, created_at, updated_at)
         VALUES (?1, ?2, ?3,
                 (SELECT COALESCE(MAX(position), 0) + 1 FROM queued_inputs WHERE room_id = ?2),
                 ?4, ?5)",
        params![
            item.id.as_str(),
            item.room_id.as_str(),
            item.content,
            item.created_at,
            item.updated_at
        ],
    )?;
    Ok(())
}

/// 一个房间的队列，排在前面的先。
pub fn list(conn: &Connection, room_id: &RoomId) -> Result<Vec<QueuedInput>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM queued_inputs WHERE room_id = ?1 ORDER BY position, id"
    ))?;
    let rows = statement.query_map([room_id.as_str()], from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn get(conn: &Connection, id: &QueuedId) -> Result<Option<QueuedInput>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM queued_inputs WHERE id = ?1"),
            [id.as_str()],
            from_row,
        )
        .optional()?)
}

/// 有排着的话的房间。
pub fn rooms(conn: &Connection) -> Result<Vec<RoomId>> {
    let mut statement = conn.prepare("SELECT DISTINCT room_id FROM queued_inputs")?;
    let rows = statement.query_map([], |row| Ok(RoomId::from(row.get::<_, String>(0)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn set_content(conn: &Connection, id: &QueuedId, content: &str, now: i64) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE queued_inputs SET content = ?2, updated_at = ?3 WHERE id = ?1",
        params![id.as_str(), content, now],
    )? > 0)
}

/// 和前一条（`offset < 0`）或后一条换位置。已经在头上或尾上就不动。
pub fn shift(conn: &Connection, id: &QueuedId, offset: i32) -> Result<()> {
    let Some((room_id, position)) = conn
        .query_row(
            "SELECT room_id, position FROM queued_inputs WHERE id = ?1",
            [id.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?
    else {
        return Ok(());
    };
    let neighbour = if offset < 0 {
        "SELECT id, position FROM queued_inputs WHERE room_id = ?1 AND position < ?2
         ORDER BY position DESC LIMIT 1"
    } else {
        "SELECT id, position FROM queued_inputs WHERE room_id = ?1 AND position > ?2
         ORDER BY position LIMIT 1"
    };
    let Some((other, other_position)) = conn
        .query_row(neighbour, params![room_id, position], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .optional()?
    else {
        return Ok(());
    };
    conn.execute(
        "UPDATE queued_inputs SET position = ?2 WHERE id = ?1",
        params![id.as_str(), other_position],
    )?;
    conn.execute(
        "UPDATE queued_inputs SET position = ?2 WHERE id = ?1",
        params![other, position],
    )?;
    Ok(())
}

/// 删掉一条。返回是否真的删掉了。
pub fn delete(conn: &Connection, id: &QueuedId) -> Result<bool> {
    Ok(conn.execute("DELETE FROM queued_inputs WHERE id = ?1", [id.as_str()])? > 0)
}

fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<QueuedInput> {
    Ok(QueuedInput {
        id: QueuedId::from(row.get::<_, String>(0)?),
        room_id: RoomId::from(row.get::<_, String>(1)?),
        content: row.get(2)?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{rooms, Store};
    use sa_protocol::Room;

    fn room(store: &Store) -> RoomId {
        let room = Room {
            id: RoomId::new(),
            name: "房间".into(),
            host_bot_id: None,
            member_bot_ids: vec![],
            created_at: 1,
            updated_at: 1,
        };
        store.write(|tx| rooms::insert(tx, &room)).unwrap();
        room.id
    }

    fn item(room_id: &RoomId, content: &str) -> QueuedInput {
        QueuedInput {
            id: QueuedId::new(),
            room_id: room_id.clone(),
            content: content.into(),
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn queue_keeps_order_per_room_and_moves_by_one() {
        let store = Store::open_in_memory().unwrap();
        let (a, b) = (room(&store), room(&store));
        let (one, two, three) = (item(&a, "一"), item(&a, "二"), item(&a, "三"));
        for queued in [&one, &two, &three, &item(&b, "别处")] {
            store.write(|tx| push(tx, queued)).unwrap();
        }
        let contents = |room: &RoomId| -> Vec<String> {
            store
                .read(|c| list(c, room))
                .unwrap()
                .into_iter()
                .map(|q| q.content)
                .collect()
        };
        assert_eq!(contents(&a), ["一", "二", "三"]);
        store.write(|tx| shift(tx, &three.id, -1)).unwrap();
        assert_eq!(contents(&a), ["一", "三", "二"]);
        store.write(|tx| shift(tx, &one.id, -1)).unwrap();
        store.write(|tx| shift(tx, &two.id, 1)).unwrap();
        assert_eq!(contents(&a), ["一", "三", "二"]);
        assert!(store
            .write(|tx| set_content(tx, &one.id, "改过", 2))
            .unwrap());
        assert!(store.write(|tx| delete(tx, &three.id)).unwrap());
        assert!(!store.write(|tx| delete(tx, &three.id)).unwrap());
        assert_eq!(contents(&a), ["改过", "二"]);
        assert_eq!(contents(&b), ["别处"]);
        let mut with_queue = store.read(rooms).unwrap();
        with_queue.sort_by(|x, y| x.as_str().cmp(y.as_str()));
        let mut expected = vec![a, b];
        expected.sort_by(|x, y| x.as_str().cmp(y.as_str()));
        assert_eq!(with_queue, expected);
    }
}
