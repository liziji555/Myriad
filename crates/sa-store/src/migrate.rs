use crate::{Result, StoreError};
use rusqlite::Connection;

/// 按顺序排列的迁移脚本。已发布的脚本不能修改，只能在末尾追加。
const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_messages.sql"),
    include_str!("../migrations/0003_permission_rules.sql"),
    include_str!("../migrations/0004_multi_bot.sql"),
    include_str!("../migrations/0005_tasks.sql"),
    include_str!("../migrations/0006_effects.sql"),
    include_str!("../migrations/0007_budget_notices.sql"),
    include_str!("../migrations/0008_usage_task.sql"),
    include_str!("../migrations/0009_routines.sql"),
    include_str!("../migrations/0010_external.sql"),
    include_str!("../migrations/0011_audit.sql"),
    include_str!("../migrations/0012_queue.sql"),
    include_str!("../migrations/0013_discussion.sql"),
    include_str!("../migrations/0014_reply_to.sql"),
];

pub const LATEST_SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

pub(crate) fn current_version(conn: &Connection) -> Result<i64> {
    Ok(conn.pragma_query_value(None, "user_version", |row| row.get(0))?)
}

pub(crate) fn run(conn: &mut Connection) -> Result<()> {
    let current = current_version(conn)?;
    if current > LATEST_SCHEMA_VERSION {
        return Err(StoreError::SchemaTooNew {
            found: current,
            supported: LATEST_SCHEMA_VERSION,
        });
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", index as i64 + 1)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_fresh_database_to_latest() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), LATEST_SCHEMA_VERSION);
    }

    #[test]
    fn running_twice_is_a_no_op() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn).unwrap();
        run(&mut conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), LATEST_SCHEMA_VERSION);
    }

    #[test]
    fn refuses_database_from_a_newer_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", LATEST_SCHEMA_VERSION + 1)
            .unwrap();
        assert!(matches!(
            run(&mut conn),
            Err(StoreError::SchemaTooNew { .. })
        ));
    }
}
