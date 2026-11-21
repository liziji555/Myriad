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

    /// 建数据目录（连同缺的上级）。新建出来的目录只有当前用户能进：里面有令牌、数据库和会话记录。
    /// 已经存在的目录不动它的权限。
    pub fn create(&self) -> std::io::Result<()> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&self.root)
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

    /// 任务的独立工作目录，一个任务一个子目录。
    pub fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    /// 可选的系统提示词。有这个文件就用它，没有就用内置的。
    /// 技能放在这里：`<名字>/SKILL.md` 或者 `<名字>.md`。
    pub fn skills(&self) -> PathBuf {
        self.root.join("skills")
    }

    pub fn chat_prompt(&self) -> PathBuf {
        self.root.join("prompts").join("chat.md")
    }

    pub fn token(&self) -> PathBuf {
        self.root.join("token")
    }
}
