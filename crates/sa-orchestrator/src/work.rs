//! 任务的工作层：把「等着开工」的任务一个个跑起来。
//!
//! 每个任务有自己的工作会话和独立目录（见 [`crate::workdir`]）。负责的 Bot 用强档模型在里面干活，
//! 结束时用 `report` 交结果：做完了等人验收、缺一个回答、卡住了，或者这件事不用做。
//! 同时最多跑 [`MAX_PARALLEL_RUNS`] 个任务，多的排队。

use crate::room::{db, RoomObserver};
use crate::{render_work_prompt, workdir, Orchestrator, OrchestratorError, Shared};
use async_trait::async_trait;
use sa_engine::{EngineError, NewTask, RunOutcome, Workdir};
use sa_kernel::{run_turn, Inbox, Transcript, TurnConfig, TurnEnd, UserInput};
use sa_llm::RetryPolicy;
use sa_protocol::{
    ArtifactKind, Bot, CreateTaskRequest, MessageKind, MessageState, ModelTier, Report,
    ReportStatus, RoomId, RunState, Sender, Task, TaskId, TaskState,
};
use sa_tools::{Fence, Tool, ToolCtx, ToolError, ToolResult};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// 同时最多跑这么多个任务。
pub const MAX_PARALLEL_RUNS: usize = 3;
/// 任务说明里带上房间里最近这么多条对话。
const BRIEF_MESSAGES: u32 = 10;
/// 交付物里最多列这么多个文件。
const MAX_FILE_ARTIFACTS: usize = 30;
const STOPPED_NOTE: &str = "被停下了。补充说明后可以让它继续。";

#[derive(Default)]
pub(crate) struct Works {
    wake: Arc<Notify>,
    scheduler: Mutex<Option<JoinHandle<()>>>,
    /// 正在跑的任务，和停下它用的开关。
    running: Mutex<HashMap<TaskId, (CancellationToken, JoinHandle<()>)>>,
    /// 人点了「停止」的任务。用来区分「被人停下」和「服务在关闭」。
    stopped: Mutex<HashSet<TaskId>>,
    /// 验收一次只做一个：把改动带回工作目录不能并发。
    accepting: tokio::sync::Mutex<()>,
}

/// 有任务可能要开工了：叫醒调度（没有就先启动它）。
pub(crate) fn kick(shared: &Arc<Shared>) {
    if shared.shutdown.is_cancelled() {
        return;
    }
    let mut scheduler = shared.works.scheduler.lock().unwrap();
    if scheduler.is_none() {
        *scheduler = Some(tokio::spawn(schedule(shared.clone())));
    }
    shared.works.wake.notify_one();
}

async fn schedule(shared: Arc<Shared>) {
    loop {
        tokio::select! {
            _ = shared.works.wake.notified() => {}
            _ = shared.shutdown.cancelled() => return,
        }
        let engine = shared.engine.clone();
        let open = match db(move || engine.tasks_in_state(TaskState::Open)).await {
            Ok(open) => open,
            Err(error) => {
                tracing::error!("查不到等着开工的任务：{error}");
                continue;
            }
        };
        // 在同一把锁里「看有没有空位」和「占位」：任务结束时也要拿这把锁才能把自己去掉。
        let mut running = shared.works.running.lock().unwrap();
        for task in open {
            if running.len() >= MAX_PARALLEL_RUNS || shared.shutdown.is_cancelled() {
                break;
            }
            if running.contains_key(&task.id) {
                continue;
            }
            let cancel = shared.shutdown.child_token();
            let (shared, id, token) = (shared.clone(), task.id.clone(), cancel.clone());
            let key = id.clone();
            let handle = tokio::spawn(async move {
                run_task(&shared, task, &token).await;
                shared.works.running.lock().unwrap().remove(&id);
                shared.works.stopped.lock().unwrap().remove(&id);
                // 空出了一个位置：看看有没有排着队的。
                shared.works.wake.notify_one();
            });
            running.insert(key, (cancel, handle));
        }
    }
}

/// 服务关闭：等正在跑的任务收尾（它们会被标成「中断」，下次启动接着做）。
pub(crate) async fn shutdown(shared: &Arc<Shared>) {
    let scheduler = shared.works.scheduler.lock().unwrap().take();
    if let Some(scheduler) = scheduler {
        let _ = scheduler.await;
    }
    let handles: Vec<JoinHandle<()>> = {
        let mut running = shared.works.running.lock().unwrap();
        running.drain().map(|(_, (_, handle))| handle).collect()
    };
    for handle in handles {
        let _ = handle.await;
    }
}

/// 任务没法开工：标成「卡住了」并写明原因。
async fn block(shared: &Arc<Shared>, task_id: &TaskId, reason: String) {
    let (engine, id) = (shared.engine.clone(), task_id.clone());
    if let Err(error) = db(move || engine.move_task(&id, TaskState::Blocked, &reason)).await {
        tracing::warn!(task = %task_id, "任务没能标成卡住：{error}");
    }
}

/// 准备好任务的独立目录：已经有了就接着用，没有就新建。
async fn ensure_workdir(shared: &Arc<Shared>, task_id: &TaskId) -> Result<Workdir, String> {
    let (engine, id) = (shared.engine.clone(), task_id.clone());
    let existing = db(move || engine.task_workdir(&id))
        .await
        .map_err(|e| e.to_string())?;
    if let Some(workdir) = existing {
        if Path::new(&workdir.path).is_dir() {
            return Ok(workdir);
        }
        return Err(format!(
            "任务的目录 {} 不见了，没法接着做。可以取消这个任务重新开一个。",
            workdir.path
        ));
    }
    let settings = &shared.settings;
    let workdir = workdir::provision(settings.fence.root(), &settings.work_dir, task_id).await?;
    let (engine, id, saved) = (shared.engine.clone(), task_id.clone(), workdir.clone());
    db(move || engine.set_task_workdir(&id, Some(&saved)).map(drop))
        .await
        .map_err(|e| e.to_string())?;
    Ok(workdir)
}

/// 跑任务的一次运行，从开工到收尾。
async fn run_task(shared: &Arc<Shared>, task: Task, cancel: &CancellationToken) {
    let engine = shared.engine.clone();
    let (room_id, owner) = (task.room_id.clone(), task.owner_bot_id.clone());
    let bot = db(move || {
        let room = engine.get_room(&room_id)?;
        let Some(owner) = owner else { return Ok(None) };
        Ok(engine
            .list_bots()?
            .into_iter()
            .find(|bot| bot.id == owner && bot.enabled && room.member_bot_ids.contains(&bot.id)))
    })
    .await;
    let bot = match bot {
        Ok(Some(bot)) => bot,
        Ok(None) => {
            let reason = "负责这个任务的 Bot 不在房间里或者停用了。换一个负责人再继续。";
            return block(shared, &task.id, reason.into()).await;
        }
        Err(error) => return block(shared, &task.id, error.to_string()).await,
    };
    let workdir = match ensure_workdir(shared, &task.id).await {
        Ok(workdir) => workdir,
        Err(reason) => return block(shared, &task.id, reason).await,
    };
    let fence = match Fence::new(Path::new(&workdir.path), &[]) {
        Ok(fence) => fence,
        Err(error) => {
            return block(shared, &task.id, format!("任务目录打不开：{error}")).await;
        }
    };

    let (engine, id, bot_id) = (shared.engine.clone(), task.id.clone(), bot.id.clone());
    let (run, draft, session_id) = match db(move || engine.start_run(&id, &bot_id)).await {
        Ok(started) => started,
        Err(error) => {
            // 多半是开工前被人取消了：刚准备好的目录不留。
            tracing::info!(task = %task.id, "任务没有开工：{error}");
            let (engine, id) = (shared.engine.clone(), task.id.clone());
            let ended = db(move || engine.get_task(&id)).await;
            if ended.is_ok_and(|task| task.state.is_terminal()) {
                cleanup(shared, &task.id).await;
            }
            return;
        }
    };

    let observer = RoomObserver {
        shared: shared.clone(),
        room_id: task.room_id.clone(),
        bot: bot.clone(),
        session_id: session_id.clone(),
        message_id: draft.id.clone(),
        fence: fence.clone(),
        steps: Mutex::new(vec![]),
        // 工作会话里没有「不说话」这回事，字直接显示。
        held: Mutex::new(None),
    };
    let reported: Arc<Mutex<Option<Report>>> = Arc::default();
    let path = shared
        .settings
        .sessions_dir
        .join(format!("{session_id}.jsonl"));
    let end = match (
        Transcript::open(&path),
        shared.models.resolve(ModelTier::Strong),
    ) {
        (Err(error), _) => TurnEnd::Failed(format!("任务的会话记录打不开：{error}")),
        (_, Err(reason)) => TurnEnd::Failed(reason),
        (Ok(mut transcript), Ok(model)) => {
            let config = TurnConfig {
                model: model.model.clone(),
                system_prompt: render_work_prompt(&bot, &fence),
                max_steps: shared.settings.max_steps,
                max_tokens: None,
                thinking: model.thinking,
                session_id: Some(session_id.to_string()),
                context_window: Some(model.context_window.unwrap_or_else(|| {
                    model.provider.capabilities(&model.model).context_window as usize
                })),
                spill_dir: shared
                    .settings
                    .sessions_dir
                    .parent()
                    .map(|d| d.join("spill")),
                retry: RetryPolicy::default(),
            };
            let mut ctx = ToolCtx::new(fence.clone());
            ctx.allow_private_network = shared.settings.allow_private_network;
            let mut tools = shared.tools.clone();
            tools.register(ReportTool {
                reported: reported.clone(),
            });
            let mut inbox = WorkInbox {
                shared: shared.clone(),
                task: task.clone(),
                fresh: transcript.entries().is_empty(),
                first: true,
            };
            let outcome = run_turn(
                &mut transcript,
                model.provider.as_ref(),
                &tools,
                &ctx,
                &config,
                &mut inbox,
                &observer,
                cancel,
            )
            .await;
            observer.settle_steps().await;
            outcome.end
        }
    };

    let stopped = shared.works.stopped.lock().unwrap().contains(&task.id);
    let (message_state, error) = match &end {
        TurnEnd::Completed => (MessageState::Complete, None),
        TurnEnd::Cancelled if stopped || !shared.shutdown.is_cancelled() => {
            (MessageState::Cancelled, None)
        }
        TurnEnd::Cancelled => (MessageState::Interrupted, None),
        TurnEnd::Truncated => (
            MessageState::Failed,
            Some("回复太长，到了模型单次输出的上限，后面被截断了。".to_string()),
        ),
        TurnEnd::MaxSteps => (
            MessageState::Failed,
            Some(format!(
                "连续调用了 {} 次模型还没做完，先停在这里。可以让它继续，或者把任务拆小一点。",
                shared.settings.max_steps
            )),
        ),
        TurnEnd::ContextLimit => (
            MessageState::Failed,
            Some("任务的对话太长，已经放不进模型的上下文窗口。".to_string()),
        ),
        TurnEnd::Failed(reason) => (MessageState::Failed, Some(reason.clone())),
    };
    let (engine, id, failure) = (shared.engine.clone(), draft.id.clone(), error.clone());
    let said = match db(move || engine.finish_message(&id, message_state, failure)).await {
        Ok(message) => message.content,
        Err(error) => {
            tracing::error!(task = %task.id, "运行过程没写进去：{error}");
            String::new()
        }
    };

    // 不管怎么结束的，目录里的改动都记下来：下一次运行、验收都从这里接着。
    let artifacts = match workdir::commit_and_diff(&workdir, &task.title).await {
        Ok(files) => {
            let mut artifacts = vec![];
            if let Some(branch) = &workdir.branch {
                artifacts.push((ArtifactKind::Branch, branch.clone(), "任务分支".to_string()));
            }
            let extra = files.len().saturating_sub(MAX_FILE_ARTIFACTS);
            artifacts.extend(
                files
                    .into_iter()
                    .take(MAX_FILE_ARTIFACTS)
                    .map(|(path, label)| (ArtifactKind::File, path, label)),
            );
            if extra > 0 {
                tracing::info!(task = %task.id, "还有 {extra} 个改动的文件没有列进交付物");
            }
            Some(artifacts)
        }
        Err(error) => {
            tracing::warn!(task = %task.id, "改动没能记下来：{error}");
            None
        }
    };

    let outcome = match message_state {
        MessageState::Complete => {
            // 没有调用 report 就结束了：把它最后说的话当结果，交给人验收。
            let report = reported.lock().unwrap().take().unwrap_or_else(|| Report {
                status: ReportStatus::Done,
                summary: if said.trim().is_empty() {
                    "（没有写结果说明）".into()
                } else {
                    said.trim().to_string()
                },
            });
            let (task_state, note) = match report.status {
                ReportStatus::Done => (TaskState::Review, String::new()),
                ReportStatus::NeedsInput => (TaskState::Waiting, report.summary.clone()),
                ReportStatus::Blocked => (TaskState::Blocked, report.summary.clone()),
                ReportStatus::Dropped => (TaskState::Dropped, report.summary.clone()),
            };
            RunOutcome {
                state: RunState::Succeeded,
                report: Some(report),
                error: None,
                task_state: Some(task_state),
                note,
                artifacts,
            }
        }
        MessageState::Interrupted => RunOutcome {
            state: RunState::Interrupted,
            report: None,
            error: None,
            task_state: Some(TaskState::Open),
            note: "上一次运行被中断了，会接着做".into(),
            artifacts,
        },
        MessageState::Cancelled => RunOutcome {
            state: RunState::Cancelled,
            report: None,
            error: None,
            task_state: Some(TaskState::Blocked),
            note: STOPPED_NOTE.into(),
            artifacts,
        },
        _ => RunOutcome {
            state: RunState::Failed,
            report: None,
            error: error.clone(),
            task_state: Some(TaskState::Blocked),
            note: error.unwrap_or_default(),
            artifacts,
        },
    };
    let (engine, run_id) = (shared.engine.clone(), run.id.clone());
    match db(move || engine.finish_run(&run_id, outcome)).await {
        // 任务到此为止（不用做了，或者中途被取消）：独立目录不留。
        Ok(task) if matches!(task.state, TaskState::Dropped | TaskState::Cancelled) => {
            cleanup(shared, &task.id).await;
        }
        Ok(_) => {}
        Err(error) => tracing::error!(task = %task.id, "运行结果没记上：{error}"),
    }
}

/// 删掉任务的独立目录。
async fn cleanup(shared: &Arc<Shared>, task_id: &TaskId) {
    let (engine, id) = (shared.engine.clone(), task_id.clone());
    if let Ok(Some(dir)) = db(move || engine.task_workdir(&id)).await {
        let settings = &shared.settings;
        workdir::remove(settings.fence.root(), &settings.work_dir, task_id, &dir).await;
    }
}

/// 工作会话的输入：第一次是任务说明，之后是人补充的话。
struct WorkInbox {
    shared: Arc<Shared>,
    task: Task,
    /// 会话记录是空的：这是任务的第一次运行。
    fresh: bool,
    first: bool,
}

#[async_trait]
impl Inbox for WorkInbox {
    async fn drain(&mut self) -> Vec<UserInput> {
        let (engine, id) = (self.shared.engine.clone(), self.task.id.clone());
        let extra = db(move || engine.take_task_input(&id))
            .await
            .unwrap_or_default();
        let first = std::mem::take(&mut self.first);
        let content = if first && self.fresh {
            let mut brief = self.brief().await;
            if !extra.is_empty() {
                brief.push_str(&format!("\n\n【用户的新指示】\n{extra}"));
            }
            brief
        } else if !extra.is_empty() {
            format!("【用户的新指示】\n{extra}")
        } else if first {
            "（这个任务上一次没有做完就停了。先看看现在做到哪一步，再接着做，最后用 report 交结果。）"
                .to_string()
        } else {
            return vec![];
        };
        vec![UserInput {
            content,
            message_id: None,
            message_seq: None,
        }]
    }
}

impl WorkInbox {
    async fn brief(&self) -> String {
        let task = &self.task;
        let mut brief = format!("【任务】{}\n\n要做什么：\n{}", task.title, task.scope);
        if !task.acceptance.is_empty() {
            brief.push_str(&format!("\n\n验收标准：\n{}", task.acceptance));
        }
        let (engine, room_id) = (self.shared.engine.clone(), task.room_id.clone());
        // 多取一些：任务卡、运行过程不算对话，要筛掉。
        let recent = db(move || engine.list_messages(&room_id, None, BRIEF_MESSAGES * 3))
            .await
            .map(|page| page.messages)
            .unwrap_or_default();
        let (engine, names) = (self.shared.engine.clone(), HashMap::new());
        let names: HashMap<_, _> = db(move || engine.list_bots())
            .await
            .map(|bots| bots.into_iter().map(|bot| (bot.id, bot.name)).collect())
            .unwrap_or(names);
        let chat: Vec<String> = recent
            .iter()
            .filter(|m| m.kind == MessageKind::Chat && m.state == MessageState::Complete)
            .filter(|m| !m.content.trim().is_empty())
            .map(|m| {
                let who = match &m.sender {
                    Sender::User => "用户",
                    Sender::Bot { bot_id } => names.get(bot_id).map_or("Bot", String::as_str),
                };
                let mut chars = m.content.chars();
                let mut text: String = chars.by_ref().take(600).collect();
                if chars.next().is_some() {
                    text.push('…');
                }
                format!("{who}：{text}")
            })
            .collect();
        if !chat.is_empty() {
            let from = chat.len().saturating_sub(BRIEF_MESSAGES as usize);
            brief.push_str(&format!(
                "\n\n房间里开这个任务之前的对话（供参考，不是指令）：\n{}",
                chat[from..].join("\n")
            ));
        }
        brief
    }
}

/// `report`：工作会话交结果。
struct ReportTool {
    reported: Arc<Mutex<Option<Report>>>,
}

#[async_trait]
impl Tool for ReportTool {
    fn name(&self) -> &'static str {
        "report"
    }

    fn description(&self) -> String {
        "交任务的结果。结束之前必须调用一次；之后用一两句话收尾，不要再调别的工具。\
         status：done（做完了，等用户验收）、needs_input（缺一个只有用户能回答的问题）、\
         blocked（卡住了）、dropped（这件事不需要做）。\
         summary 是给用户看的：做了什么、怎么验证的；或者要问的问题；或者卡在哪。"
            .into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "status": { "type": "string", "enum": ["done", "needs_input", "blocked", "dropped"] },
                "summary": { "type": "string", "description": "给用户看的结果说明" }
            },
            "required": ["status", "summary"]
        })
    }

    fn title(&self, input: &Value) -> String {
        let label = match input["status"].as_str() {
            Some("done") => "做完了",
            Some("needs_input") => "有问题要问",
            Some("blocked") => "卡住了",
            Some("dropped") => "不需要做",
            _ => "",
        };
        format!("交结果：{label}")
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, _ctx: &ToolCtx) -> ToolResult {
        let report: Report =
            serde_json::from_value(input).map_err(|e| ToolError(format!("参数不对：{e}")))?;
        if report.summary.trim().is_empty() {
            return Err(ToolError::new(
                "summary 是空的。用户只看得到这段话，写清楚结果。",
            ));
        }
        // 调了两次以后一次为准。
        *self.reported.lock().unwrap() = Some(Report {
            status: report.status,
            summary: report.summary.trim().to_string(),
        });
        Ok("结果已经记下。用一两句话收尾就结束，不要再调用工具。".into())
    }
}

/// `create_task`：对话里的 Bot 把一件事派成任务。
pub(crate) struct CreateTaskTool {
    pub shared: Arc<Shared>,
    pub room_id: RoomId,
    /// 谁在派。
    pub from: Bot,
}

#[derive(Deserialize)]
struct CreateTaskInput {
    title: String,
    scope: String,
    #[serde(default)]
    acceptance: Option<String>,
    #[serde(default)]
    owner: Option<String>,
}

#[async_trait]
impl Tool for CreateTaskTool {
    fn name(&self) -> &'static str {
        "create_task"
    }

    fn description(&self) -> String {
        "把一件要花些功夫的事（修 bug、加功能、整理文件、写报告）开成任务。任务在独立的目录里做，不影响用户手上的文件，\
         做完由用户验收。这个工具只是把任务派出去，不会等它做完：调用之后告诉用户任务开了就结束这一轮。\
         scope 要写清楚做什么、相关的背景和文件，干活的会话看不到这里的对话全文。\
         owner 是负责的 Bot 的名字，不填就是你自己。"
            .into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "一句话的标题" },
                "scope": { "type": "string", "description": "要做什么，以及需要的背景" },
                "acceptance": { "type": "string", "description": "怎样算做完（可选）" },
                "owner": { "type": "string", "description": "负责的 Bot 的名字（可选，默认是你自己）" }
            },
            "required": ["title", "scope"]
        })
    }

    fn title(&self, input: &Value) -> String {
        format!("开任务：{}", input["title"].as_str().unwrap_or("").trim())
    }

    /// 只是登记一个任务，不碰用户的文件；任务里要执行命令时另有审批。
    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, _ctx: &ToolCtx) -> ToolResult {
        let input: CreateTaskInput =
            serde_json::from_value(input).map_err(|e| ToolError(format!("参数不对：{e}")))?;
        let wanted = input
            .owner
            .as_deref()
            .map(|name| name.trim().trim_start_matches('@').to_lowercase())
            .filter(|name| !name.is_empty());
        let (engine, room_id, from) = (
            self.shared.engine.clone(),
            self.room_id.clone(),
            self.from.clone(),
        );
        let created = db(move || {
            let owner = match wanted {
                None => from.clone(),
                Some(wanted) => {
                    let room = engine.get_room(&room_id)?;
                    let mut members = engine.list_bots()?;
                    members.retain(|bot| bot.enabled && room.member_bot_ids.contains(&bot.id));
                    match members.iter().find(|bot| bot.name.to_lowercase() == wanted) {
                        Some(bot) => bot.clone(),
                        None => {
                            let names: Vec<&str> =
                                members.iter().map(|bot| bot.name.as_str()).collect();
                            return Err(EngineError::Invalid(format!(
                                "房间里没有叫这个名字的 Bot。可以负责任务的有：{}。",
                                names.join("、")
                            )));
                        }
                    }
                }
            };
            let task = engine.create_task(NewTask {
                room_id,
                title: input.title,
                scope: input.scope,
                acceptance: input.acceptance.unwrap_or_default(),
                owner_bot_id: Some(owner.id.clone()),
                created_by: Sender::Bot { bot_id: from.id },
            })?;
            Ok((task, owner))
        })
        .await
        .map_err(|e| ToolError(format!("任务没开成：{e}")))?;
        kick(&self.shared);
        let (task, owner) = created;
        Ok(format!(
            "任务「{}」已经开了，由 {} 负责，会在独立的目录里做。不用等它：做完后结果会出现在房间里，由用户验收。现在告诉用户任务已经开了，然后结束这一轮。",
            task.title, owner.name
        ))
    }
}

impl Orchestrator {
    /// 人开一个任务。有负责人就排队开工。
    pub async fn create_task(
        &self,
        room_id: &RoomId,
        request: CreateTaskRequest,
    ) -> Result<Task, OrchestratorError> {
        self.check_running()?;
        let (engine, room_id) = (self.shared.engine.clone(), room_id.clone());
        let task = db(move || {
            engine.create_task(NewTask {
                room_id,
                title: request.title,
                scope: request.scope,
                acceptance: request.acceptance.unwrap_or_default(),
                owner_bot_id: request.owner_bot_id,
                created_by: Sender::User,
            })
        })
        .await?;
        kick(&self.shared);
        Ok(task)
    }

    /// 让任务继续：草稿、等回答、卡住的任务重新排队。`note` 是给负责的 Bot 的话；
    /// 任务正在做时只把话带到。`owner` 给了就换负责人。
    pub async fn resume_task(
        &self,
        id: &TaskId,
        note: String,
        owner: Option<sa_protocol::BotId>,
    ) -> Result<Task, OrchestratorError> {
        self.check_running()?;
        let (engine, task_id) = (self.shared.engine.clone(), id.clone());
        let task = db(move || {
            if engine.get_task(&task_id)?.state == TaskState::Review {
                return Err(EngineError::Invalid(
                    "任务在等验收：通过，或者打回重做".into(),
                ));
            }
            engine.resume_task(&task_id, &note, owner.as_ref())
        })
        .await?;
        kick(&self.shared);
        Ok(task)
    }

    /// 验收不通过：带着意见打回去重做。
    pub async fn reject_task(&self, id: &TaskId, note: String) -> Result<Task, OrchestratorError> {
        self.check_running()?;
        let (engine, task_id) = (self.shared.engine.clone(), id.clone());
        let task = db(move || {
            if engine.get_task(&task_id)?.state != TaskState::Review {
                return Err(EngineError::Invalid("只有等验收的任务才能打回".into()));
            }
            if note.trim().is_empty() {
                return Err(EngineError::Invalid("打回要写明哪里不行".into()));
            }
            engine.resume_task(&task_id, &note, None)
        })
        .await?;
        kick(&self.shared);
        Ok(task)
    }

    /// 验收通过：把任务的改动带回工作目录，任务完成。带不回去时任务留在「等验收」。
    pub async fn accept_task(&self, id: &TaskId) -> Result<Task, OrchestratorError> {
        self.check_running()?;
        let shared = &self.shared;
        let _one_at_a_time = shared.works.accepting.lock().await;
        let (engine, task_id) = (shared.engine.clone(), id.clone());
        let (task, dir) = db(move || {
            let task = engine.get_task(&task_id)?;
            let dir = engine.task_workdir(&task_id)?;
            Ok((task, dir))
        })
        .await?;
        if task.state != TaskState::Review {
            return Err(EngineError::Invalid("只有等验收的任务才能验收".into()).into());
        }
        if let Some(dir) = &dir {
            workdir::apply_to_workspace(shared.settings.fence.root(), dir)
                .await
                .map_err(OrchestratorError::Conflict)?;
        }
        let (engine, task_id) = (shared.engine.clone(), id.clone());
        let task = db(move || engine.move_task(&task_id, TaskState::Done, "")).await?;
        cleanup(shared, id).await;
        Ok(task)
    }

    /// 停下任务正在进行的运行。任务变成「卡住了」，现场保留，之后可以让它继续。
    /// 返回是否真的有东西被停下。
    pub fn stop_task(&self, id: &TaskId) -> bool {
        let running = self.shared.works.running.lock().unwrap();
        match running.get(id) {
            Some((cancel, _)) => {
                self.shared.works.stopped.lock().unwrap().insert(id.clone());
                cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// 取消任务：不做了。正在进行的运行会被停下，独立目录删掉。
    pub async fn cancel_task(&self, id: &TaskId, note: String) -> Result<Task, OrchestratorError> {
        let (engine, task_id) = (self.shared.engine.clone(), id.clone());
        let task = db(move || engine.move_task(&task_id, TaskState::Cancelled, &note)).await?;
        // 有运行在进行：停下它，目录由它收尾时删。没有：现在就删。
        if !self.stop_task(id) {
            cleanup(&self.shared, id).await;
        }
        Ok(task)
    }

    /// 启动时调用：让上次没做完的任务接着排队。
    pub fn resume_tasks(&self) {
        kick(&self.shared);
    }

    fn check_running(&self) -> Result<(), OrchestratorError> {
        if self.shared.shutdown.is_cancelled() {
            Err(OrchestratorError::ShuttingDown)
        } else {
            Ok(())
        }
    }
}
