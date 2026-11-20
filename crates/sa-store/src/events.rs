use crate::{Result, StoreError};
use rusqlite::{params, Connection};
use sa_protocol::{Event, EventKind, Millis, RoomId};

/// 追加一条事件，返回它的 `seq`。
pub fn append(
    conn: &Connection,
    room_id: Option<&RoomId>,
    kind: EventKind,
    payload: &serde_json::Value,
    created_at: Millis,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO events (room_id, kind, payload_json, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![
            room_id.map(RoomId::as_str),
            kind.as_str(),
            payload.to_string(),
            created_at
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// `seq` 大于 `cursor` 的事件，按 `seq` 升序，最多 `limit` 条。
pub fn list_after(conn: &Connection, cursor: i64, limit: u32) -> Result<Vec<Event>> {
    let mut stmt = conn.prepare(
        "SELECT seq, room_id, kind, payload_json, created_at FROM events WHERE seq > ?1 ORDER BY seq LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![cursor, limit], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
        ))
    })?;
    rows.map(|row| {
        let (seq, room_id, kind, payload, created_at) = row?;
        let kind = EventKind::parse(&kind)
            .ok_or_else(|| StoreError::Corrupt(format!("事件 {seq} 的种类未知：{kind}")))?;
        let payload = serde_json::from_str(&payload)
            .map_err(|e| StoreError::Corrupt(format!("事件 {seq} 的内容不是合法 JSON：{e}")))?;
        Ok(Event {
            seq,
            room_id: room_id.map(RoomId::from),
            kind,
            payload,
            created_at,
        })
    })
    .collect()
}

/// 最新一条事件的 `seq`；没有事件时为 0。
pub fn head(conn: &Connection) -> Result<i64> {
    Ok(
        conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |row| {
            row.get(0)
        })?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use serde_json::json;

    #[test]
    fn append_assigns_increasing_seq_and_list_after_resumes() {
        let store = Store::open_in_memory().unwrap();
        let room = RoomId::new();
        let (first, second) = store
            .write(|tx| {
                let a = append(
                    tx,
                    Some(&room),
                    EventKind::RoomCreated,
                    &json!({"n": 1}),
                    10,
                )?;
                let b = append(tx, None, EventKind::BotCreated, &json!({"n": 2}), 11)?;
                Ok((a, b))
            })
            .unwrap();
        assert!(second > first);

        let all = store.read(|c| list_after(c, 0, 100)).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].room_id.as_ref(), Some(&room));
        assert_eq!(all[0].payload, json!({"n": 1}));

        let tail = store.read(|c| list_after(c, first, 100)).unwrap();
        assert_eq!(tail.iter().map(|e| e.seq).collect::<Vec<_>>(), [second]);
        assert_eq!(store.read(head).unwrap(), second);
    }

    #[test]
    fn list_after_respects_limit() {
        let store = Store::open_in_memory().unwrap();
        store
            .write(|tx| {
                for i in 0..5 {
                    append(tx, None, EventKind::BotCreated, &json!({}), i)?;
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(store.read(|c| list_after(c, 0, 3)).unwrap().len(), 3);
    }
}
