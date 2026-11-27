//! 在共享目录那台电脑上跑 harness。
//!
//! 服务交给 `sa share` 三件事：开独立目录、在里面跑 harness、带回或丢掉改动。
//! `sa share` 在自己这边做（独立目录放在它的数据目录里），harness 的每一步边做边报回房间。

use crate::{claude_stream, codex_app_server, harness, workdir};
use async_trait::async_trait;
use sa_engine::Workdir;
use sa_kernel::{ApprovalRequest, Observer, StepEnd, TurnEnd};
use sa_llm::Usage;
use sa_protocol::{DeskChange, Harness, PermissionMode, TaskId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// 开（或者接着用）独立目录。
pub const OPEN: &str = "_desk_open";
/// 在独立目录里跑一次 harness。
pub const RUN: &str = "_harness";
/// 带回或丢掉独立目录里的改动，收掉目录。
pub const CLOSE: &str = "_desk_close";
/// 回收远端工位里常驻的 ACP / Codex / Claude 会话进程（例如用户点「重置会话」时）。
pub const EVICT: &str = "_desk_evict";
/// 服务向运行中的远端工位回传用户审批结果（控制消息，无需回包）。
pub const APPROVAL: &str = "_approval";
/// 服务向运行中的远端工位注入实时用户插话（控制消息，通过 `Progress::SteerAck` 回传确认）。
pub const STEER: &str = "_steer";

/// 这几件只有加了 `--commands` 的共享才做：harness 自己会执行命令，带回时 git 也会跑仓库里的钩子。
pub fn needs_commands(tool: &str) -> bool {
    matches!(tool, "shell" | OPEN | RUN | CLOSE | EVICT)
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
    /// 房间的权限档位。
    #[serde(default)]
    pub permission: PermissionMode,
    /// 是否启用远端双向审批、运行中插话与常驻会话池（仅当服务端与 `sa share` 均支持时开启）。
    #[serde(default)]
    pub interactive: bool,
    /// 常驻会话池的隔离作用域（`room_id:bot_id`）。
    #[serde(default)]
    pub scope: Option<String>,
    /// 服务端配置的审批超时（毫秒），供远端审批等待设置安全兜底超时。
    #[serde(default)]
    pub approval_timeout_ms: Option<u64>,
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
    /// 对应工位的常驻会话池作用域，关闭前先回收进程树。
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Evict {
    pub scope: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalReply {
    pub req_id: String,
    pub allowed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Steer {
    pub steer_id: String,
    pub text: String,
}

/// 远端审批挂起表：`req_id -> oneshot(allowed)`。
pub type PendingApprovals = Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>;

/// harness 做到一半报回来的一件事。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    Usage {
        model: String,
        input_tokens: u64,
        output_tokens: u64,
        cached_tokens: u64,
        #[serde(default)]
        cache_write_tokens: u64,
    },
    /// 远端 harness 发起工具审批请求，等待服务端房间内用户答复后经 `_approval` 回传。
    ApprovalRequest {
        req_id: String,
        call_id: String,
        tool: String,
        title: String,
        input: Value,
        reason: String,
    },
    /// 远端 harness 对服务端 `_steer` 插话请求的确认结果。
    SteerAck {
        steer_id: String,
        accepted: bool,
    },
}

/// 默认远端审批安全兜底超时（当服务端未显式传入 `approval_timeout_ms` 时使用）。
const DEFAULT_REMOTE_APPROVAL_TIMEOUT: Duration = Duration::from_secs(615);
/// 比服务端审批超时多出的缓冲，确保服务端超时与审计优先触发，同时避免服务重启导致远端死等。
const REMOTE_APPROVAL_GRACE: Duration = Duration::from_secs(15);

/// 把 harness 的进展与双向审批请求交给 `sa share`，由它转给服务。
pub struct Forward {
    progress: UnboundedSender<Progress>,
    approvals: Option<PendingApprovals>,
    cancel: CancellationToken,
    approval_timeout: Duration,
}

impl Forward {
    /// 只转发单向进展（兼容旧服务端 / 非交互模式）。
    pub fn new(progress: UnboundedSender<Progress>) -> Self {
        Self {
            progress,
            approvals: None,
            cancel: CancellationToken::new(),
            approval_timeout: DEFAULT_REMOTE_APPROVAL_TIMEOUT,
        }
    }

    /// 启用双向审批透传：审批请求通过 `progress` 上报服务端，并等待对应 `req_id` 的 `_approval` 回传。
    pub fn interactive(
        progress: UnboundedSender<Progress>,
        approvals: PendingApprovals,
        cancel: CancellationToken,
        approval_timeout_ms: Option<u64>,
    ) -> Self {
        let approval_timeout = approval_timeout_ms
            .map(|ms| Duration::from_millis(ms).saturating_add(REMOTE_APPROVAL_GRACE))
            .unwrap_or(DEFAULT_REMOTE_APPROVAL_TIMEOUT);
        Self {
            progress,
            approvals: Some(approvals),
            cancel,
            approval_timeout,
        }
    }
}

struct ApprovalCleanup {
    approvals: PendingApprovals,
    req_id: String,
}

impl Drop for ApprovalCleanup {
    fn drop(&mut self) {
        self.approvals.lock().unwrap().remove(&self.req_id);
    }
}

#[async_trait]
impl Observer for Forward {
    async fn text_delta(&self, text: &str) {
        let _ = self.progress.send(Progress::Text { text: text.into() });
    }

    async fn tool_started(&self, call_id: &str, tool: &str, title: &str, _arguments: &str) {
        let _ = self.progress.send(Progress::Started {
            id: call_id.into(),
            tool: tool.into(),
            title: title.into(),
        });
    }

    async fn tool_finished(&self, call_id: &str, end: StepEnd, output: &str) {
        let _ = self.progress.send(Progress::Finished {
            id: call_id.into(),
            ok: end == StepEnd::Ok,
            output: output.into(),
        });
    }

    async fn request_approval(&self, request: ApprovalRequest<'_>) -> bool {
        let Some(approvals) = &self.approvals else {
            return false;
        };
        if self.cancel.is_cancelled() {
            return false;
        }
        let req_id = uuid::Uuid::new_v4().simple().to_string();
        let (tx, rx) = oneshot::channel();
        approvals.lock().unwrap().insert(req_id.clone(), tx);
        let _cleanup = ApprovalCleanup {
            approvals: Arc::clone(approvals),
            req_id: req_id.clone(),
        };
        if self
            .progress
            .send(Progress::ApprovalRequest {
                req_id,
                call_id: request.call_id.into(),
                tool: request.tool.into(),
                title: request.title.into(),
                input: request.input.clone(),
                reason: request.reason.into(),
            })
            .is_err()
        {
            return false;
        }
        tokio::select! {
            answer = rx => answer.unwrap_or(false),
            _ = self.cancel.cancelled() => false,
            _ = tokio::time::sleep(self.approval_timeout) => false,
        }
    }

    async fn usage(&self, model: &str, usage: Usage) {
        let _ = self.progress.send(Progress::Usage {
            model: model.into(),
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cached_tokens: usage.cached_tokens,
            cache_write_tokens: usage.cache_write_tokens,
        });
    }
}

/// 反序列化一条来自远端 `sa share` 的进展消息。
pub(crate) fn parse_progress(progress: &str) -> Option<Progress> {
    serde_json::from_str::<Progress>(progress).ok()
}

/// 服务这边：把单向进展（文本、步骤、用量）放进房间观察者。
pub(crate) async fn apply_progress(progress: Progress, observer: &dyn Observer) {
    match progress {
        Progress::Text { text } => observer.text_delta(&text).await,
        Progress::Started { id, tool, title } => {
            observer.tool_started(&id, &tool, &title, "").await
        }
        Progress::Finished { id, ok, output } => {
            let end = if ok { StepEnd::Ok } else { StepEnd::Failed };
            observer.tool_finished(&id, end, &output).await
        }
        Progress::Usage {
            model,
            input_tokens,
            output_tokens,
            cached_tokens,
            cache_write_tokens,
        } => {
            observer
                .usage(
                    &model,
                    Usage {
                        input_tokens,
                        output_tokens,
                        cached_tokens,
                        cache_write_tokens,
                    },
                )
                .await
        }
        Progress::ApprovalRequest { .. } | Progress::SteerAck { .. } => {}
    }
}

/// `sa share` 这边：共享目录开出来的独立目录都放在 `dir` 里，同时维护远端常驻会话池。
pub struct Desks {
    root: PathBuf,
    dir: PathBuf,
    acp_pool: harness::AcpPool,
    codex_pool: codex_app_server::CodexPool,
    claude_pool: claude_stream::ClaudePool,
}

impl Desks {
    /// `root` 是共享的目录（已经规范化过），`dir` 是放独立目录的地方。
    pub fn new(root: &Path, dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            root: root.to_path_buf(),
            dir: dir.canonicalize()?,
            acp_pool: harness::AcpPool::default(),
            codex_pool: codex_app_server::CodexPool::default(),
            claude_pool: claude_stream::ClaudePool::default(),
        })
    }

    /// 回收指定工位作用域下的常驻 ACP / Codex / Claude 进程。
    pub fn evict_scope(&self, scope: &str) {
        self.acp_pool.evict_scope(scope);
        self.codex_pool.evict_scope(scope);
        self.claude_pool.evict_scope(scope);
    }

    /// 回收所有远端常驻会话进程（例如 `sa share` 退出时）。
    pub fn evict_all(&self) {
        self.acp_pool.evict_all();
        self.codex_pool.evict_all();
        self.claude_pool.evict_all();
    }

    fn inside(&self, path: &str) -> bool {
        Path::new(path)
            .canonicalize()
            .is_ok_and(|real| real.starts_with(&self.dir))
    }

    /// 服务交来的独立目录确实是这边开的一个：`share-desks` 下面直接的一个 `desk-…`，git 目录也是它自己的。
    fn own(&self, workdir: &Workdir) -> Result<(), String> {
        let root = Path::new(&workdir.root);
        let named = root
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("desk-"));
        let parent = root.parent().and_then(|p| p.canonicalize().ok());
        let git_ok = workdir.kind != workdir::KIND_COPY
            || Path::new(&workdir.git_dir) == Path::new(&format!("{}.git", workdir.root));
        let path_ok = Path::new(&workdir.path).starts_with(root);
        if named && parent.as_deref() == Some(self.dir.as_path()) && git_ok && path_ok {
            Ok(())
        } else {
            Err("这个独立目录不是这边开的".into())
        }
    }

    /// 服务交来的独立目录确实是这边从现在共享的目录开出来的。
    fn check(&self, workdir: &Workdir) -> Result<(), String> {
        self.own(workdir)?;
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
        // 共享目录把这边的数据目录也包了进去：复制一份会把独立目录和令牌一起复制进去。
        if self.dir.starts_with(&self.root) {
            return Err("共享的目录里包含这台电脑上的 Myriad 数据目录，不能在这里给 harness 开独立目录。换一个小一点的目录共享".into());
        }
        let bot: String = open
            .bot
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .collect();
        let name = format!("desk-{bot}-{}", sa_engine::now_ms());
        workdir::provision(&self.root, &self.dir, &TaskId::from(name)).await
    }

    /// 在独立目录里跑一次 harness（支持远端常驻会话池、双向审批与运行中插话），跑完把改动提交掉、列出来。
    pub async fn run_interactive(
        &self,
        job: Run,
        mut steer_rx: Option<UnboundedReceiver<Steer>>,
        progress: &UnboundedSender<Progress>,
        observer: &dyn Observer,
        cancel: &CancellationToken,
    ) -> Ran {
        if let Err(reason) = self.check(&job.workdir) {
            if let Some(rx) = steer_rx.as_mut() {
                while let Ok(steer) = rx.try_recv() {
                    let _ = progress.send(Progress::SteerAck {
                        steer_id: steer.steer_id,
                        accepted: false,
                    });
                }
            }
            return Ran {
                ended: Ended::Failed { reason },
                session: job.session,
                text: String::new(),
                changes: Ok(vec![]),
            };
        }
        let interactive = job.interactive;
        let launch = harness::Launch {
            harness: job.harness,
            command: &job.command,
            dir: Path::new(&job.workdir.path),
            prompt: &job.prompt,
            session: job.session.as_deref(),
            permission: job.permission,
            mcp_servers: &[],
            acp_pool: interactive.then_some(&self.acp_pool),
            codex_pool: interactive.then_some(&self.codex_pool),
            claude_pool: interactive.then_some(&self.claude_pool),
            acp_scope: interactive.then_some(job.scope.as_deref()).flatten(),
            interactive_approvals: interactive,
        };
        let finished = if interactive
            && matches!(job.harness, Harness::Codex | Harness::ClaudeCode)
            && steer_rx.is_some()
        {
            let mut remote_rx = steer_rx.take().expect("checked is_some");
            let (local_tx, local_rx) = tokio::sync::mpsc::unbounded_channel();
            let run_fut = harness::run_interactive(&launch, Some(local_rx), observer, cancel);
            tokio::pin!(run_fut);
            let mut remote_open = true;
            let finished = loop {
                tokio::select! {
                    finished = &mut run_fut => break finished,
                    steer = remote_rx.recv(), if remote_open => {
                        let Some(steer) = steer else {
                            remote_open = false;
                            continue;
                        };
                        let (ack_tx, mut ack_rx) = oneshot::channel();
                        if local_tx
                            .send(harness::SteerRequest {
                                text: steer.text,
                                ack: ack_tx,
                            })
                            .is_err()
                        {
                            let _ = progress.send(Progress::SteerAck {
                                steer_id: steer.steer_id,
                                accepted: false,
                            });
                            continue;
                        }
                        let accepted = tokio::select! {
                            finished = &mut run_fut => {
                                let accepted = ack_rx.try_recv() == Ok(true);
                                let _ = progress.send(Progress::SteerAck {
                                    steer_id: steer.steer_id,
                                    accepted,
                                });
                                break finished;
                            }
                            res = &mut ack_rx => res.unwrap_or(false),
                        };
                        let _ = progress.send(Progress::SteerAck {
                            steer_id: steer.steer_id,
                            accepted,
                        });
                    }
                }
            };
            while let Ok(steer) = remote_rx.try_recv() {
                let _ = progress.send(Progress::SteerAck {
                    steer_id: steer.steer_id,
                    accepted: false,
                });
            }
            finished
        } else {
            let finished = harness::run_interactive(&launch, None, observer, cancel).await;
            if let Some(rx) = steer_rx.as_mut() {
                while let Ok(steer) = rx.try_recv() {
                    let _ = progress.send(Progress::SteerAck {
                        steer_id: steer.steer_id,
                        accepted: false,
                    });
                }
            }
            finished
        };
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
            // 验收前先做只读预检，避免主目录有未完成合并时误杀常驻进程。
            workdir::preflight_accept(dir).await?;
            if let Some(scope) = close.scope.as_deref() {
                self.evict_scope(scope);
            } else {
                self.evict_all();
            }
            workdir::bring_back(dir).await?;
        } else {
            // 丢掉不管它是从哪个目录开的：共享换了目录之后，旧的也要能丢。
            self.own(dir)?;
            if let Some(scope) = close.scope.as_deref() {
                self.evict_scope(scope);
            } else {
                self.evict_all();
            }
        }
        workdir::remove(&TaskId::from("desk".to_string()), dir).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desk(root: &Path) -> Workdir {
        Workdir {
            path: root.display().to_string(),
            kind: workdir::KIND_COPY.into(),
            branch: None,
            base_ref: String::new(),
            root: root.display().to_string(),
            git_dir: format!("{}.git", root.display()),
            source: String::new(),
        }
    }

    #[test]
    fn only_desks_opened_here_can_be_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let share = tmp.path().join("共享");
        std::fs::create_dir_all(&share).unwrap();
        let desks = Desks::new(&share.canonicalize().unwrap(), &tmp.path().join("desks")).unwrap();
        assert!(desks.own(&desk(&desks.dir.join("desk-b1-1"))).is_ok());
        // 放独立目录的地方本身、别处的目录、git 目录指到别处的：都不认。
        assert!(desks.own(&desk(&desks.dir)).is_err());
        assert!(desks.own(&desk(&tmp.path().join("desk-b1-1"))).is_err());
        let mut planted = desk(&desks.dir.join("desk-b1-1"));
        planted.git_dir = desks.dir.join("desk-b2-1.git").display().to_string();
        assert!(desks.own(&planted).is_err());
    }

    #[tokio::test]
    async fn a_share_that_contains_the_desks_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let desks = Desks::new(&root, &root.join("data/work/share-desks")).unwrap();
        let open = Open {
            bot: "b1".into(),
            existing: None,
        };
        assert!(desks.open(open).await.unwrap_err().contains("数据目录"));
    }
}
