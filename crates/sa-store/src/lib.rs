//! SQLite 持久层。
//!
//! 这里只有「怎么存、怎么取」，不做业务判断；业务规则在 `sa-engine`。
//! 各表的函数都接收 `&Connection`，这样上层可以把多次写入放进同一个事务。

mod error;
mod migrate;

pub mod audit;
pub mod bots;
pub mod effects;
pub mod events;
pub mod external;
pub mod messages;
pub mod notices;
pub mod rooms;
pub mod routines;
pub mod rules;
pub mod sessions;
pub mod tasks;
pub mod usage;

pub use error::StoreError;
pub use migrate::LATEST_SCHEMA_VERSION;
pub use rusqlite::{Connection, Transaction};

use rusqlite::{OpenFlags, TransactionBehavior};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

pub type Result<T> = std::result::Result<T, StoreError>;

/// 一个数据库连接。SQLite 同一时刻只允许一个写入者，所以用互斥锁串行化。
pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// 打开（不存在则创建）数据库文件，并迁移到最新结构。
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init(conn)
    }

    /// 内存数据库，测试用。
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate::run(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        f(&self.lock())
    }

    /// 在一个事务里执行 `f`：返回 `Ok` 才提交，否则全部回滚。
    pub fn write<T>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        // 持锁期间 panic 时事务已随 Transaction 的析构回滚，连接仍可用。
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 只读体检结果，`sa doctor` 用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inspection {
    pub schema_version: i64,
    /// `PRAGMA integrity_check` 报告的问题；为空表示通过。
    pub integrity_problems: Vec<String>,
}

/// 以只读方式检查一个已有的数据库文件，不创建、不迁移。
pub fn inspect(path: &Path) -> Result<Inspection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let schema_version = migrate::current_version(&conn)?;
    let mut stmt = conn.prepare("PRAGMA integrity_check")?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let integrity_problems = rows.into_iter().filter(|r| r != "ok").collect();
    Ok(Inspection {
        schema_version,
        integrity_problems,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_rolls_back_on_error() {
        let store = Store::open_in_memory().unwrap();
        let result: Result<()> = store.write(|tx| {
            events::append(
                tx,
                None,
                sa_protocol::EventKind::BotCreated,
                &serde_json::json!({}),
                1,
            )?;
            Err(StoreError::Corrupt("boom".into()))
        });
        assert!(result.is_err());
        assert_eq!(store.read(events::head).unwrap(), 0);
    }

    #[test]
    fn open_creates_file_and_inspect_reads_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        drop(Store::open(&path).unwrap());
        let report = inspect(&path).unwrap();
        assert_eq!(report.schema_version, LATEST_SCHEMA_VERSION);
        assert!(report.integrity_problems.is_empty());
    }

    #[test]
    fn inspect_does_not_create_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.db");
        assert!(inspect(&path).is_err());
        assert!(!path.exists());
    }
}
