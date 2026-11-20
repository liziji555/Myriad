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

    /// `sa serve` 运行期间持有的锁，保证一个数据目录同时只有一个服务在写。
    pub fn lock(&self) -> PathBuf {
        self.root.join("serve.lock")
    }

    /// 会话记录，一个会话一个 `.jsonl` 文件。
    pub fn sessions(&self) -> PathBuf {
        self.root.join("sessions")
    }

    /// 可选的系统提示词。有这个文件就用它，没有就用内置的。
    pub fn chat_prompt(&self) -> PathBuf {
        self.root.join("prompts").join("chat.md")
    }

    pub fn token(&self) -> PathBuf {
        self.root.join("token")
    }
}
