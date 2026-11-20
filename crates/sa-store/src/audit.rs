//! 审计日志：只追加的一张表。

use crate::{Result, StoreError};
use rusqlite::{params, Connection, Row};
use sa_protocol::{AuditActor, AuditEntry, RoomId, TaskId};

/// 要找哪些记录。都不填就是全部。
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub room_id: Option<RoomId>,
    pub task_id: Option<TaskId>,
    /// 动作名的开头，例如 `approval.` 或者 `tool.shell`。
    pub action: Option<String>,
    /// 只要比这个 id 更早的（翻页用）。
    pub before: Option<i64>,
}

/// 记一条。`entry.id` 不用填，返回数据库给的 id。
pub fn insert(conn: &Connection, entry: &AuditEntry) -> Result<i64> {
    conn.execute(
        "INSERT INTO audit (at, actor, actor_name, action, room_id, task_id, summary, detail)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            entry.at,
            entry.actor.as_str(),
            entry.actor_name,
            entry.action,
            entry.room_id.as_ref().map(RoomId::as_str),
            entry.task_id.as_ref().map(TaskId::as_str),
            entry.summary,
            entry.detail,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 符合条件的记录，新的在前，最多 `limit` 条。
pub fn list(conn: &Connection, filter: &Filter, limit: u32) -> Result<Vec<AuditEntry>> {
    // 动作名按开头匹配：把 LIKE 的通配符当普通字符。
    let prefix = filter.action.as_ref().map(|action| {
        let escaped = action
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        format!("{escaped}%")
    });
    let mut stmt = conn.prepare(
        "SELECT id, at, actor, actor_name, action, room_id, task_id, summary, detail FROM audit
         WHERE (?1 IS NULL OR room_id = ?1)
           AND (?2 IS NULL OR task_id = ?2)
           AND (?3 IS NULL OR action LIKE ?3 ESCAPE '\\')
           AND (?4 IS NULL OR id < ?4)
         ORDER BY id DESC LIMIT ?5",
    )?;
    let rows = stmt.query_map(
        params![
            filter.room_id.as_ref().map(RoomId::as_str),
            filter.task_id.as_ref().map(TaskId::as_str),
            prefix,
            filter.before,
            limit,
        ],
        read,
    )?;
    rows.map(|row| row?).collect()
}

/// 清掉 `before` 这个时间之前的记录。返回清了几条。
pub fn prune(conn: &Connection, before: i64) -> Result<usize> {
    Ok(conn.execute("DELETE FROM audit WHERE at < ?1", [before])?)
}

fn read(row: &Row<'_>) -> rusqlite::Result<Result<AuditEntry>> {
    let actor: String = row.get(2)?;
    let room_id: Option<String> = row.get(5)?;
    let task_id: Option<String> = row.get(6)?;
    let Some(actor) = AuditActor::parse(&actor) else {
        return Ok(Err(StoreError::Corrupt(format!(
            "审计记录里不认识的 actor：{actor}"
        ))));
    };
    Ok(Ok(AuditEntry {
        id: row.get(0)?,
        at: row.get(1)?,
        actor,
        actor_name: row.get(3)?,
        action: row.get(4)?,
        room_id: room_id.map(RoomId::from),
        task_id: task_id.map(TaskId::from),
        summary: row.get(7)?,
        detail: row.get(8)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    fn entry(at: i64, action: &str, room: Option<&str>, task: Option<&str>) -> AuditEntry {
        AuditEntry {
            id: 0,
            at,
            actor: AuditActor::Bot,
            actor_name: "Coder".into(),
            action: action.into(),
            room_id: room.map(|id| RoomId::from(id.to_string())),
            task_id: task.map(|id| TaskId::from(id.to_string())),
            summary: format!("做了 {action}"),
            detail: Some("细节".into()),
        }
    }

    #[test]
    fn entries_are_appended_filtered_paged_and_pruned() {
        let store = Store::open_in_memory().unwrap();
        store
            .write(|tx| {
                insert(tx, &entry(10, "tool.shell", Some("r1"), Some("t1")))?;
                insert(tx, &entry(20, "approval.decided", Some("r1"), None))?;
                insert(tx, &entry(30, "tool_shellx", Some("r2"), None))?;
                insert(tx, &entry(40, "tool.shell", Some("r2"), Some("t2")))?;
                Ok(())
            })
            .unwrap();
        let find = |filter: Filter, limit| {
            let found = store.read(|c| list(c, &filter, limit)).unwrap();
            found.into_iter().map(|e| e.at).collect::<Vec<_>>()
        };
        // 新的在前；id 是递增的。
        assert_eq!(find(Filter::default(), 10), [40, 30, 20, 10]);
        let room = |id: &str| Some(RoomId::from(id.to_string()));
        assert_eq!(
            find(
                Filter {
                    room_id: room("r1"),
                    ..Default::default()
                },
                10
            ),
            [20, 10]
        );
        let task = Filter {
            task_id: Some(TaskId::from("t2".to_string())),
            ..Default::default()
        };
        assert_eq!(find(task, 10), [40]);
        // 按动作名的开头找；下划线和百分号是普通字符，不是通配符。
        let action = |prefix: &str| Filter {
            action: Some(prefix.into()),
            ..Default::default()
        };
        assert_eq!(find(action("tool."), 10), [40, 10]);
        assert_eq!(find(action("tool_"), 10), [30]);
        assert_eq!(find(action("%"), 10), Vec::<i64>::new());
        // 翻页。
        let first = store.read(|c| list(c, &Filter::default(), 2)).unwrap();
        assert_eq!(first.len(), 2);
        let older = Filter {
            before: Some(first[1].id),
            ..Default::default()
        };
        assert_eq!(find(older, 10), [20, 10]);
        // 读出来和写进去的一样。
        let mut expected = entry(40, "tool.shell", Some("r2"), Some("t2"));
        expected.id = first[0].id;
        assert_eq!(first[0], expected);

        assert_eq!(store.write(|tx| prune(tx, 25)).unwrap(), 2);
        assert_eq!(find(Filter::default(), 10), [40, 30]);
    }
}
