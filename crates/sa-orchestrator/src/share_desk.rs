//! 在共享目录那台电脑上跑 harness。
//!
//! 服务交给 `sa share` 三件事：开独立目录、在里面跑 harness、带回或丢掉改动。
//! `sa share` 在自己这边做（独立目录放在它的数据目录里），harness 的每一步边做边报回房间。

use crate::{harness, workdir};
use async_trait::async_trait;
use sa_engine::Workdir;
use sa_kernel::{Observer, StepEnd, TurnEnd};
use sa_llm::Usage;
use sa_protocol::{DeskChange, Harness, TaskId};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

/// 开（或者接着用）独立目录。
pub const OPEN: &str = "_desk_open";
/// 在独立目录里跑一次 harness。
pub const RUN: &str = "_harness";
/// 带回或丢掉独立目录里的改动，收掉目录。
pub const CLOSE: &str = "_desk_close";

/// 这几件只有加了 `--commands` 的共享才做：harness 自己会执行命令。
pub fn needs_commands(tool: &str) -> bool {
    matches!(tool, "shell" | OPEN | RUN)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Open {
    pub bot: String,
    /// 上次开的。还在的话接着用。
    pub existing: Option<Workdir>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Run {
    pub workdir: Workdir,
    pub harness: Harness,
    pub command: String,
    pub prompt: String,
    pub session: Option<String>,
    /// 跑完自动提交时的说明。
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "end", rename_all = "snake_case")]
pub enum Ended {
    Completed,
    Cancelled,
    Failed { reason: String },
}

impl From<TurnEnd> for Ended {
    fn from(end: TurnEnd) -> Self {
        match end {
            TurnEnd::Completed => Self::Completed,
            TurnEnd::Cancelled => Self::Cancelled,
            TurnEnd::Failed(reason) => Self::Failed { reason },
            other => Self::Failed {
                reason: format!("{other:?}"),
            },
        }
    }
}

impl From<Ended> for TurnEnd {
    fn from(end: Ended) -> Self {
        match end {
            Ended::Completed => Self::Completed,
            Ended::Cancelled => Self::Cancelled,
            Ended::Failed { reason } => Self::Failed(reason),
        }
    }
}

/// 跑完的结果。
#[derive(Debug, Serialize, Deserialize)]
pub struct Ran {
    pub ended: Ended,
    pub session: Option<String>,
    pub text: String,
    /// 从开工到现在改了哪些文件；没记下来时是原因。
    pub changes: Result<Vec<DeskChange>, String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Close {
    pub workdir: Workdir,
    pub accept: bool,
}

/// harness 做到一半报回来的一件事。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Progress {
    Text {
        text: String,
    },
    Started {
        id: String,
        tool: String,
        title: String,
    },
    Finished {
        id: String,
        ok: bool,
        output: String,
    },
}

/// 把 harness 的进展交给 `sa share`，由它转给服务。
pub struct Forward(pub UnboundedSender<Progress>);

#[async_trait]
impl Observer for Forward {
    async fn text_delta(&self, text: &str) {
        let _ = self.0.send(Progress::Text { text: text.into() });
    }

    async fn tool_started(&self, call_id: &str, tool: &str, title: &str, _arguments: &str) {
        let _ = self.0.send(Progress::Started {
            id: call_id.into(),
            tool: tool.into(),
            title: title.into(),
        });
    }

    async fn tool_finished(&self, call_id: &str, end: StepEnd, output: &str) {
        let _ = self.0.send(Progress::Finished {
            id: call_id.into(),
            ok: end == StepEnd::Ok,
            output: output.into(),
        });
    }

    async fn usage(&self, _model: &str, _usage: Usage) {}
}

/// 服务这边：把那台电脑报回来的进展放进房间。
pub(crate) async fn show(progress: &str, observer: &dyn Observer) {
    let Ok(progress) = serde_json::from_str::<Progress>(progress) else {
        return;
    };
    match progress {
        Progress::Text { text } => observer.text_delta(&text).await,
        Progress::Started { id, tool, title } => {
            observer.tool_started(&id, &tool, &title, "").await
        }
        Progress::Finished { id, ok, output } => {
            let end = if ok { StepEnd::Ok } else { StepEnd::Failed };
            observer.tool_finished(&id, end, &output).await
        }
    }
}

/// `sa share` 这边：共享目录开出来的独立目录都放在 `dir` 里。
pub struct Desks {
    root: PathBuf,
    dir: PathBuf,
}

impl Desks {
    /// `root` 是共享的目录（已经规范化过），`dir` 是放独立目录的地方。
    pub fn new(root: &Path, dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            root: root.to_path_buf(),
            dir: dir.canonicalize()?,
        })
    }

    fn inside(&self, path: &str) -> bool {
        Path::new(path)
            .canonicalize()
            .is_ok_and(|real| real.starts_with(&self.dir))
    }

    /// 服务交来的独立目录确实是这边从共享目录开出来的。
    fn check(&self, workdir: &Workdir) -> Result<(), String> {
        let source = Path::new(&workdir.source).canonicalize().ok();
        if source.as_deref() != Some(self.root.as_path()) {
            return Err(format!(
                "这个独立目录是从 {} 开出来的，现在共享的是 {}。先在成员面板里丢掉它的旧改动，再让它干活",
                workdir.source,
                self.root.display()
            ));
        }
        if !self.inside(&workdir.path) || !self.inside(&workdir.root) {
            return Err("这个独立目录不是这边开的".into());
        }
        Ok(())
    }

    /// 上次开的还在就接着用，不在了就重新开一个。
    pub async fn open(&self, open: Open) -> Result<Workdir, String> {
        if let Some(existing) = open.existing {
            if Path::new(&existing.path).is_dir() {
                self.check(&existing)?;
                return Ok(existing);
            }
        }
        let bot: String = open
            .bot
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .collect();
        let name = format!("desk-{bot}-{}", sa_engine::now_ms());
        workdir::provision(&self.root, &self.dir, &TaskId::from(name)).await
    }

    /// 在独立目录里跑一次 harness，跑完把改动提交掉、列出来。
    pub async fn run(&self, job: Run, observer: &dyn Observer, cancel: &CancellationToken) -> Ran {
        if let Err(reason) = self.check(&job.workdir) {
            return Ran {
                ended: Ended::Failed { reason },
                session: job.session,
                text: String::new(),
                changes: Ok(vec![]),
            };
        }
        let launch = harness::Launch {
            harness: job.harness,
            command: &job.command,
            dir: Path::new(&job.workdir.path),
            prompt: &job.prompt,
            session: job.session.as_deref(),
        };
        let finished = harness::run(&launch, observer, cancel).await;
        let changes = workdir::commit_and_diff(&job.workdir, &job.summary)
            .await
            .map(|files| {
                files
                    .into_iter()
                    .map(|(path, label)| DeskChange { path, label })
                    .collect()
            });
        Ran {
            ended: finished.end.into(),
            session: finished.session,
            text: finished.text,
            changes,
        }
    }

    /// 带回（`accept`）或丢掉改动，然后删掉独立目录。带不回去时什么都不动。
    pub async fn close(&self, close: Close) -> Result<(), String> {
        let dir = &close.workdir;
        if close.accept {
            if !Path::new(&dir.path).is_dir() {
                return Err("它的独立目录已经不在了，没有改动可以带回".into());
            }
            self.check(dir)?;
            workdir::bring_back(dir).await?;
        } else if Path::new(&dir.root).exists() {
            self.check(dir)?;
        }
        if dir.kind == workdir::KIND_COPY
            && Path::new(&dir.git_dir).exists()
            && !self.inside(&dir.git_dir)
        {
            return Err("这个独立目录不是这边开的".into());
        }
        workdir::remove(&TaskId::from("desk".to_string()), dir).await;
        Ok(())
    }
}
