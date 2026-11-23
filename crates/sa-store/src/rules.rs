use crate::{Result, StoreError};
use rusqlite::{params, Connection, OptionalExtension};
use sa_protocol::{PermissionRule, RoomId, RuleId};

/// 记下一条「总是允许」。同样的已经有了就返回原来那条。
pub fn insert(conn: &Connection, rule: &PermissionRule) -> Result<PermissionRule> {
    conn.execute(
        "INSERT INTO permission_rules (id, room_id, tool, pattern, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (room_id, tool, pattern) DO NOTHING",
        params![
            rule.id.as_str(),
            rule.room_id.as_str(),
            rule.tool,
            rule.pattern,
            rule.created_at
        ],
    )?;
    find(conn, &rule.room_id, &rule.tool, &rule.pattern)?
        .ok_or_else(|| StoreError::Corrupt("刚写入的规则读不出来".into()))
}

pub fn find(
    conn: &Connection,
    room_id: &RoomId,
    tool: &str,
    pattern: &str,
) -> Result<Option<PermissionRule>> {
    Ok(conn
        .query_row(
            "SELECT id, room_id, tool, pattern, created_at FROM permission_rules
             WHERE room_id = ?1 AND tool = ?2 AND pattern = ?3",
            params![room_id.as_str(), tool, pattern],
            from_row,
        )
        .optional()?)
}

pub fn list(conn: &Connection, room_id: &RoomId) -> Result<Vec<PermissionRule>> {
    let mut statement = conn.prepare(
        "SELECT id, room_id, tool, pattern, created_at FROM permission_rules
         WHERE room_id = ?1 ORDER BY created_at, id",
    )?;
    let rows = statement.query_map([room_id.as_str()], from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn get(conn: &Connection, id: &RuleId) -> Result<Option<PermissionRule>> {
    Ok(conn
        .query_row(
            "SELECT id, room_id, tool, pattern, created_at FROM permission_rules WHERE id = ?1",
            [id.as_str()],
            from_row,
        )
        .optional()?)
}

/// 删除一条。返回是否真的删掉了。
pub fn delete(conn: &Connection, id: &RuleId) -> Result<bool> {
    Ok(conn.execute("DELETE FROM permission_rules WHERE id = ?1", [id.as_str()])? > 0)
}

fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PermissionRule> {
    Ok(PermissionRule {
        id: RuleId::from(row.get::<_, String>(0)?),
        room_id: RoomId::from(row.get::<_, String>(1)?),
        tool: row.get(2)?,
        pattern: row.get(3)?,
        created_at: row.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{rooms, Store};
    use sa_protocol::Room;

    fn room(store: &Store, name: &str) -> RoomId {
        let room = Room {
            discussion: Default::default(),
            rounds: sa_protocol::DEFAULT_ROUNDS,
            id: RoomId::new(),
            name: name.into(),
            host_bot_id: None,
            member_bot_ids: vec![],
            created_at: 1,
            updated_at: 1,
        };
        store.write(|tx| rooms::insert(tx, &room)).unwrap();
        room.id
    }

    fn rule(room_id: &RoomId, tool: &str, pattern: &str) -> PermissionRule {
        PermissionRule {
            id: RuleId::new(),
            room_id: room_id.clone(),
            tool: tool.into(),
            pattern: pattern.into(),
            created_at: 5,
        }
    }

    #[test]
    fn rules_are_per_room_exact_and_deduplicated() {
        let store = Store::open_in_memory().unwrap();
        let (a, b) = (room(&store, "甲"), room(&store, "乙"));
        let first = store
            .write(|tx| insert(tx, &rule(&a, "shell", "cargo test")))
            .unwrap();
        // 同样的再记一次，还是原来那条。
        let again = store
            .write(|tx| insert(tx, &rule(&a, "shell", "cargo test")))
            .unwrap();
        assert_eq!(again, first);
        store
            .write(|tx| insert(tx, &rule(&a, "write", "/tmp/x")))
            .unwrap();

        let found = |room: &RoomId, tool: &str, pattern: &str| {
            store
                .read(|c| find(c, room, tool, pattern))
                .unwrap()
                .is_some()
        };
        assert!(found(&a, "shell", "cargo test"));
        assert!(!found(&a, "shell", "cargo test --release"));
        assert!(!found(&a, "write", "cargo test"));
        assert!(!found(&b, "shell", "cargo test"));
        assert_eq!(store.read(|c| list(c, &a)).unwrap().len(), 2);
        assert!(store.read(|c| list(c, &b)).unwrap().is_empty());

        assert!(store.write(|tx| delete(tx, &first.id)).unwrap());
        assert!(!store.write(|tx| delete(tx, &first.id)).unwrap());
        assert!(!found(&a, "shell", "cargo test"));
    }
}
