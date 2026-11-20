//! 任务：一件被派出去的工作，和它的每一次运行。
//!
//! 状态怎么走见 [`TaskState::can_become`]。「改状态」「记运行」「在房间里放卡片」都在同一个事务里。

use crate::messages::add_message;
use crate::{now_ms, to_json, Engine, EngineError, Result};
use sa_protocol::{
    Artifact, ArtifactId, ArtifactKind, BotId, EventKind, Message, MessageId, MessageKind,
    MessageState, Report, RoomId, Run, RunId, RunState, Sender, SessionId, Task, TaskId, TaskState,
};
use sa_store::tasks::{self, Workdir};
use sa_store::{bots, events, rooms, sessions, Transaction};

pub const MAX_TASK_TITLE_CHARS: usize = 80;
pub const MAX_TASK_TEXT_CHARS: usize = 8_000;
/// 一个房间里最多有这么多没做完的任务。
pub const MAX_ACTIVE_TASKS: usize = 10;

#[derive(Debug, Clone)]
pub struct NewTask {
    pub room_id: RoomId,
    pub title: String,
    pub scope: String,
    pub acceptance: String,
    /// 派给谁。没有就先放着（草稿）。
    pub owner_bot_id: Option<BotId>,
    pub created_by: Sender,
}

/// 一次运行怎么收的场。
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub state: RunState,
    pub report: Option<Report>,
    pub error: Option<String>,
    /// 任务接下来是什么状态。`None` 表示不动它（比如已经被人取消了）。
    pub task_state: Option<TaskState>,
    /// 对任务新状态的一句说明。
    pub note: String,
    /// 这次运行之后任务的文件和分支类交付物（种类、是什么、一句说明）。`None` 表示不动原来的。
    pub artifacts: Option<Vec<(ArtifactKind, String, String)>>,
}

impl Engine {
    /// 开一个任务，并在房间里放一张任务卡。有负责人就直接等着开工，没有就是草稿。
    pub fn create_task(&self, new: NewTask) -> Result<Task> {
        let title = new.title.trim().to_string();
        let scope = new.scope.trim().to_string();
        let acceptance = new.acceptance.trim().to_string();
        if title.is_empty() || scope.is_empty() {
            return Err(EngineError::Invalid(
                "任务要有标题，也要写清楚做什么".into(),
            ));
        }
        if title.chars().count() > MAX_TASK_TITLE_CHARS {
            return Err(EngineError::Invalid(format!(
                "任务标题不能超过 {MAX_TASK_TITLE_CHARS} 个字"
            )));
        }
        if scope.chars().count() > MAX_TASK_TEXT_CHARS
            || acceptance.chars().count() > MAX_TASK_TEXT_CHARS
        {
            return Err(EngineError::Invalid(format!(
                "任务说明和验收标准都不能超过 {MAX_TASK_TEXT_CHARS} 个字"
            )));
        }
        let now = now_ms();
        let (task, seq) = self.store.write(|tx| {
            let Some(room) = rooms::get(tx, &new.room_id)? else {
                return Ok(Err(EngineError::NotFound("没有这个房间".into())));
            };
            if let Some(owner) = &new.owner_bot_id {
                if let Err(error) = check_owner(tx, &room.member_bot_ids, owner) {
                    return Ok(Err(error));
                }
            }
            let active = tasks::list_in_room(tx, &room.id)?
                .iter()
                .filter(|task| !task.state.is_terminal() && task.state != TaskState::Review)
                .count();
            if active >= MAX_ACTIVE_TASKS {
                return Ok(Err(EngineError::Invalid(format!(
                    "这个房间里已经有 {MAX_ACTIVE_TASKS} 个没做完的任务，先处理掉一些再开新的"
                ))));
            }
            let task = Task {
                id: TaskId::new(),
                room_id: room.id.clone(),
                title,
                scope,
                acceptance,
                state: if new.owner_bot_id.is_some() {
                    TaskState::Open
                } else {
                    TaskState::Draft
                },
                owner_bot_id: new.owner_bot_id,
                note: String::new(),
                created_by: new.created_by,
                workdir: None,
                branch: None,
                artifacts: vec![],
                rev: 0,
                created_at: now,
                updated_at: now,
            };
            tasks::insert(tx, &task)?;
            events::append(
                tx,
                Some(&task.room_id),
                EventKind::TaskCreated,
                &to_json(&task),
                now,
            )?;
            let card = card(
                &task,
                task.created_by.clone(),
                MessageKind::Task,
                None,
                format!("开了任务「{}」：{}", task.title, task.scope),
                MessageState::Complete,
                now,
            );
            let (_, seq) = add_message(tx, card)?;
            Ok(Ok((task, seq)))
        })??;
        self.publish(seq);
        Ok(task)
    }

    pub fn get_task(&self, id: &TaskId) -> Result<Task> {
        self.store
            .read(|c| tasks::get(c, id))?
            .ok_or_else(|| EngineError::NotFound("没有这个任务".into()))
    }

    /// 房间里的全部任务，按创建时间升序。
    pub fn list_tasks(&self, room_id: &RoomId) -> Result<Vec<Task>> {
        self.store
            .read(|c| {
                if rooms::get(c, room_id)?.is_none() {
                    return Ok(None);
                }
                Ok(Some(tasks::list_in_room(c, room_id)?))
            })?
            .ok_or_else(|| EngineError::NotFound("没有这个房间".into()))
    }

    /// 所有房间里处在某个状态的任务，等得最久的在前。
    pub fn tasks_in_state(&self, state: TaskState) -> Result<Vec<Task>> {
        Ok(self.store.read(|c| tasks::list_in_state(c, state))?)
    }

    /// 任务的执行历史，按先后。
    pub fn task_runs(&self, id: &TaskId) -> Result<Vec<Run>> {
        self.get_task(id)?;
        Ok(self.store.read(|c| tasks::list_runs(c, id))?)
    }

    /// 把任务挪到另一个状态。`note` 是对新状态的一句说明。不允许的走法会被拒绝。
    pub fn move_task(&self, id: &TaskId, to: TaskState, note: &str) -> Result<Task> {
        self.change_task(id, |_, task| {
            check_move(task, to)?;
            task.state = to;
            task.note = note.trim().to_string();
            Ok(())
        })
    }

    /// 人让任务继续：草稿、等回答、卡住、等验收（打回）的任务回到「等着开工」。
    /// `note` 是给负责的 Bot 的话，它下一步就能读到；任务正在做时只带话、不改状态。
    /// `owner` 给了就顺便换负责人（正在做的不能换）。
    pub fn resume_task(&self, id: &TaskId, note: &str, owner: Option<&BotId>) -> Result<Task> {
        let note = note.trim();
        if note.chars().count() > MAX_TASK_TEXT_CHARS {
            return Err(EngineError::Invalid(format!(
                "补充的话不能超过 {MAX_TASK_TEXT_CHARS} 个字"
            )));
        }
        self.change_task(id, |tx, task| {
            if task.state == TaskState::Working {
                if owner.is_some() {
                    return Err(EngineError::Invalid("任务正在做，先停下再换负责人".into()));
                }
                if note.is_empty() {
                    return Err(EngineError::Invalid(
                        "任务正在做；要补充什么请写出来".into(),
                    ));
                }
            } else {
                check_move(task, TaskState::Open)?;
                if let Some(owner) = owner {
                    let room = rooms::get(tx, &task.room_id)?
                        .ok_or_else(|| EngineError::NotFound("没有这个房间".into()))?;
                    check_owner(tx, &room.member_bot_ids, owner)?;
                    task.owner_bot_id = Some(owner.clone());
                }
                if task.owner_bot_id.is_none() {
                    return Err(EngineError::Invalid(
                        "这个任务还没有负责人，先指定一个 Bot".into(),
                    ));
                }
                task.state = TaskState::Open;
                task.note = note.to_string();
            }
            if !note.is_empty() {
                tasks::push_input(tx, &task.id, note)?;
            }
            Ok(())
        })
    }

    /// 取走人给这个任务的、工作会话还没读到的话。没有时是空字符串。
    pub fn take_task_input(&self, id: &TaskId) -> Result<String> {
        Ok(self.store.write(|tx| tasks::take_input(tx, id))?)
    }

    pub fn task_workdir(&self, id: &TaskId) -> Result<Option<Workdir>> {
        Ok(self.store.read(|c| tasks::workdir(c, id))?)
    }

    /// 记下（或清掉）任务的独立工作目录。
    pub fn set_task_workdir(&self, id: &TaskId, workdir: Option<&Workdir>) -> Result<Task> {
        self.change_task(id, |tx, task| {
            tasks::set_workdir(tx, &task.id, workdir)?;
            task.workdir = workdir.map(|w| w.path.clone());
            task.branch = workdir.and_then(|w| w.branch.clone());
            Ok(())
        })
    }

    /// 给任务加一样交付物（比如开出来的 PR）。
    pub fn add_artifact(
        &self,
        id: &TaskId,
        run_id: &RunId,
        kind: ArtifactKind,
        reference: &str,
        summary: &str,
    ) -> Result<Task> {
        self.change_task(id, |tx, task| {
            let artifact = new_artifact(task, run_id, kind, reference, summary);
            tasks::insert_artifact(tx, &artifact)?;
            task.artifacts.push(artifact);
            Ok(())
        })
    }

    /// 任务开工：记一次新的运行，任务变成「正在做」，房间里出现一条记录过程的消息（正在写）。
    /// 返回这次运行和任务的工作会话。
    pub fn start_run(&self, id: &TaskId, bot_id: &BotId) -> Result<(Run, Message, SessionId)> {
        let now = now_ms();
        let (run, message, session_id, seq) = self.store.write(|tx| {
            let Some(mut task) = tasks::get(tx, id)? else {
                return Ok(Err(EngineError::NotFound("没有这个任务".into())));
            };
            if let Err(error) = check_move(&task, TaskState::Working) {
                return Ok(Err(error));
            }
            let attempt = tasks::list_runs(tx, id)?.len() as u32 + 1;
            let run_id = RunId::new();
            let message = card(
                &task,
                Sender::Bot {
                    bot_id: bot_id.clone(),
                },
                MessageKind::Run,
                Some(run_id.clone()),
                String::new(),
                MessageState::Streaming,
                now,
            );
            let (message, _) = add_message(tx, message)?;
            let run = Run {
                id: run_id,
                task_id: task.id.clone(),
                room_id: task.room_id.clone(),
                bot_id: bot_id.clone(),
                message_id: message.id.clone(),
                state: RunState::Running,
                attempt,
                started_at: now,
                ended_at: None,
                report: None,
                error: None,
            };
            tasks::insert_run(tx, &run)?;
            events::append(
                tx,
                Some(&run.room_id),
                EventKind::RunUpdated,
                &to_json(&run),
                now,
            )?;
            task.state = TaskState::Working;
            task.note = String::new();
            let seq = save_task(tx, &mut task, now)?;
            let session_id = sessions::ensure_work(tx, &task.room_id, bot_id, &task.id, now)?;
            Ok(Ok((run, message, session_id, seq)))
        })??;
        self.drafts().insert(message.id.clone(), message.clone());
        self.publish(seq);
        Ok((run, message, session_id))
    }

    /// 一次运行结束：记下结果，任务走到下一个状态；有结果时在房间里放一张交付卡。
    ///
    /// 记录过程的那条消息由调用方另外用 [`Engine::finish_message`] 定稿。
    pub fn finish_run(&self, run_id: &RunId, outcome: RunOutcome) -> Result<Task> {
        debug_assert!(outcome.state != RunState::Running);
        let now = now_ms();
        let (task, seq) = self.store.write(|tx| {
            let Some(mut run) = tasks::get_run(tx, run_id)? else {
                return Ok(Err(EngineError::NotFound("没有这次运行".into())));
            };
            let Some(mut task) = tasks::get(tx, &run.task_id)? else {
                return Ok(Err(EngineError::NotFound("没有这个任务".into())));
            };
            run.state = outcome.state;
            run.ended_at = Some(now);
            run.report = outcome.report.clone();
            run.error = outcome.error.clone();
            tasks::update_run(tx, &run)?;
            events::append(
                tx,
                Some(&run.room_id),
                EventKind::RunUpdated,
                &to_json(&run),
                now,
            )?;
            if let Some(artifacts) = &outcome.artifacts {
                tasks::delete_artifacts(tx, &task.id, ArtifactKind::File)?;
                tasks::delete_artifacts(tx, &task.id, ArtifactKind::Branch)?;
                task.artifacts
                    .retain(|a| !matches!(a.kind, ArtifactKind::File | ArtifactKind::Branch));
                for (kind, reference, summary) in artifacts {
                    let artifact = new_artifact(&task, run_id, *kind, reference, summary);
                    tasks::insert_artifact(tx, &artifact)?;
                    task.artifacts.push(artifact);
                }
            }
            if let Some(to) = outcome.task_state {
                // 运行期间任务被人挪走了（比如取消）：以人的决定为准。
                if task.state == TaskState::Working {
                    task.state = to;
                    task.note = outcome.note.trim().to_string();
                }
            }
            let mut seq = save_task(tx, &mut task, now)?;
            if let Some(report) = &outcome.report {
                let delivery = card(
                    &task,
                    Sender::Bot {
                        bot_id: run.bot_id.clone(),
                    },
                    MessageKind::Delivery,
                    Some(run.id.clone()),
                    report.summary.clone(),
                    MessageState::Complete,
                    now,
                );
                seq = add_message(tx, delivery)?.1;
            }
            Ok(Ok((task, seq)))
        })??;
        self.publish(seq);
        Ok(task)
    }

    /// 启动时调用：上次进程退出时还在跑的运行标成「中断」，对应的任务回到「等着开工」，
    /// 下次开工时接着同一个工作会话做。返回处理了几次运行。
    pub fn recover_runs(&self) -> Result<usize> {
        let now = now_ms();
        let (count, seq) = self.store.write(|tx| {
            let running = tasks::list_running(tx)?;
            let mut last = None;
            for mut run in running.iter().cloned() {
                run.state = RunState::Interrupted;
                run.ended_at = Some(now);
                tasks::update_run(tx, &run)?;
                events::append(
                    tx,
                    Some(&run.room_id),
                    EventKind::RunUpdated,
                    &to_json(&run),
                    now,
                )?;
                if let Some(mut task) = tasks::get(tx, &run.task_id)? {
                    if task.state == TaskState::Working {
                        task.state = TaskState::Open;
                        task.note = "上一次运行被中断了，会接着做".into();
                        tasks::push_input(
                            tx,
                            &task.id,
                            "（服务重启过，上一次运行被中断了。先看看现在做到哪一步，再接着做。）",
                        )?;
                        last = Some(save_task(tx, &mut task, now)?);
                    }
                }
            }
            Ok((running.len(), last))
        })?;
        if let Some(seq) = seq {
            self.publish(seq);
        }
        Ok(count)
    }

    /// 读出任务、改它、存回去并记一条事件，都在一个事务里。
    fn change_task(
        &self,
        id: &TaskId,
        change: impl FnOnce(&Transaction<'_>, &mut Task) -> Result<()>,
    ) -> Result<Task> {
        let now = now_ms();
        let (task, seq) = self.store.write(|tx| {
            let Some(mut task) = tasks::get(tx, id)? else {
                return Ok(Err(EngineError::NotFound("没有这个任务".into())));
            };
            match change(tx, &mut task) {
                Ok(()) => {}
                Err(EngineError::Store(error)) => return Err(error),
                Err(error) => return Ok(Err(error)),
            }
            let seq = save_task(tx, &mut task, now)?;
            Ok(Ok((task, seq)))
        })??;
        self.publish(seq);
        Ok(task)
    }
}

fn save_task(tx: &Transaction<'_>, task: &mut Task, now: i64) -> sa_store::Result<i64> {
    task.rev += 1;
    task.updated_at = now;
    tasks::update(tx, task)?;
    events::append(
        tx,
        Some(&task.room_id),
        EventKind::TaskUpdated,
        &to_json(task),
        now,
    )
}

fn check_move(task: &Task, to: TaskState) -> Result<()> {
    if task.state.can_become(to) {
        Ok(())
    } else {
        Err(EngineError::Invalid(format!(
            "任务现在是「{}」，不能这样操作",
            state_label(task.state)
        )))
    }
}

/// 负责人必须是房间里启用着的 Bot。
fn check_owner(tx: &Transaction<'_>, members: &[BotId], owner: &BotId) -> Result<()> {
    let enabled = bots::get(tx, owner)?.is_some_and(|bot| bot.enabled);
    if enabled && members.contains(owner) {
        Ok(())
    } else {
        Err(EngineError::Invalid(
            "任务只能派给房间里启用着的 Bot".into(),
        ))
    }
}

/// 状态的中文名，给人看的提示里用。
pub fn state_label(state: TaskState) -> &'static str {
    match state {
        TaskState::Draft => "草稿",
        TaskState::Open => "等着开工",
        TaskState::Working => "正在做",
        TaskState::Waiting => "等回答",
        TaskState::Review => "等验收",
        TaskState::Blocked => "卡住了",
        TaskState::Done => "完成",
        TaskState::Dropped => "不需要做",
        TaskState::Cancelled => "已取消",
    }
}

fn new_artifact(
    task: &Task,
    run_id: &RunId,
    kind: ArtifactKind,
    reference: &str,
    summary: &str,
) -> Artifact {
    Artifact {
        id: ArtifactId::new(),
        task_id: task.id.clone(),
        run_id: run_id.clone(),
        kind,
        reference: reference.to_string(),
        summary: summary.to_string(),
        created_at: now_ms(),
    }
}

/// 时间线上和任务有关的一条消息：任务卡、运行过程、交付卡。
fn card(
    task: &Task,
    sender: Sender,
    kind: MessageKind,
    run_id: Option<RunId>,
    content: String,
    state: MessageState,
    now: i64,
) -> Message {
    Message {
        id: MessageId::new(),
        seq: 0,
        room_id: task.room_id.clone(),
        sender,
        content,
        mentions: vec![],
        hop: 0,
        kind,
        task_id: Some(task.id.clone()),
        run_id,
        steps: vec![],
        state,
        error: None,
        rev: 0,
        created_at: now,
        updated_at: now,
    }
}
