use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// 数据目录里各个文件的位置。
#[derive(Debug, Clone)]
pub struct DataDir {
    root: PathBuf,
}

impl DataDir {
    /// 用户指定的目录优先；否则用系统的应用数据目录（Windows 下是 `%APPDATA%\superagent`）。
    pub fn resolve(explicit: Option<PathBuf>) -> Result<Self> {
        let root = match explicit {
            Some(path) => path,
            None => dirs::data_dir()
                .context("找不到系统的应用数据目录，请用 --data-dir 指定")?
                .join("superagent"),
        };
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    pub fn database(&self) -> PathBuf {
        self.root.join("state.db")
    }

    pub fn token(&self) -> PathBuf {
        self.root.join("token")
    }
}
