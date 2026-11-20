#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("数据库错误：{0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("数据库里的数据无法解析：{0}")]
    Corrupt(String),
    #[error("数据库结构版本 {found} 比本程序支持的 {supported} 新，请升级程序")]
    SchemaTooNew { found: i64, supported: i64 },
}

impl StoreError {
    /// 是否由唯一约束冲突引起（例如重名）。
    pub fn is_unique_violation(&self) -> bool {
        matches!(
            self,
            Self::Sqlite(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation
                    && (e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                        || e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY)
        )
    }
}
