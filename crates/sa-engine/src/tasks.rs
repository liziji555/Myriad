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
use sa_store::{bots, effects, events, rooms, sessions, Transaction};

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
    /// 任务是在哪个工作目录里开的。任务只在这个工作目录里做、只往这里验收。空表示不限。
    pub workspace: String,
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
            tasks::set_source(tx, &task.id, &new.workspace)?;
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
        let summary = format!("开了任务「{}」", task.title);
        let entry = match &task.created_by {
            Sender::User => crate::Audit::user("task.created", summary),
            Sender::Bot { bot_id } => {
                let name = self.get_bot(bot_id).map(|bot| bot.name).unwrap_or_default();
                crate::Audit::bot(&name, "task.created", summary)
            }
        };
        self.audit(
            entry
                .room(&task.room_id)
                .task(Some(&task.id))
                .detail(&task.scope),
        );
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

    /// 所有还在进行的运行。
    pub fn running_runs(&self) -> Result<Vec<Run>> {
        Ok(self.store.read(tasks::list_running)?)
    }

    /// `since` 之后结束的运行里，`(失败的, 成功和失败加起来的)` 各有几次。
    pub fn run_counts_since(&self, since: sa_protocol::Millis) -> Result<(i64, i64)> {
        Ok(self.store.read(|c| tasks::run_counts_since(c, since))?)
    }

    /// 把任务挪到另一个状态。`note` 是对新状态的一句说明。不允许的走法会被拒绝。
    pub fn move_task(&self, id: &TaskId, to: TaskState, note: &str) -> Result<Task> {
        let task = self.change_task(id, |_, task| {
            check_move(task, to)?;
            task.state = to;
            task.note = note.trim().to_string();
            Ok(())
        })?;
        // 任务每一次换状态都留底：验收、打回、取消、停下、卡住，事后查得到是哪一步、为什么。
        let summary = format!("任务「{}」变成了「{}」", task.title, state_label(to));
        self.audit(
            crate::Audit::system(&format!("task.{}", to.as_str()), summary)
                .room(&task.room_id)
                .task(Some(&task.id))
                .detail(note),
        );
        Ok(task)
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
        let task = self.change_task(id, |tx, task| {
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
                // 人让它继续，就是同意它再花一份任务的额度。
                let spent = sa_store::usage::task_cost(tx, &task.id)?;
                tasks::set_spend_floor(tx, &task.id, spent)?;
            }
            if !note.is_empty() {
                tasks::push_input(tx, &task.id, note)?;
            }
            Ok(())
        })?;
        let summary = format!("让任务「{}」继续做", task.title);
        self.audit(
            crate::Audit::user("task.resumed", summary)
                .room(&task.room_id)
                .task(Some(&task.id))
                .detail(note),
        );
        Ok(task)
    }

    /// 取走人给这个任务的、工作会话还没读到的话。没有时是空字符串。
    pub fn take_task_input(&self, id: &TaskId) -> Result<String> {
        Ok(self.store.write(|tx| tasks::take_input(tx, id))?)
    }

    /// 对外面做过的事（推送分支、开 PR）当时记下的结果。没做过是 `None`。
    pub fn effect(&self, key: &str) -> Result<Option<serde_json::Value>> {
        let found = self.store.read(|c| effects::get(c, key))?;
        // 读不懂的记录当作没有：宁可再做一次，也不拿坏数据当结果。
        Ok(found.and_then(|text| serde_json::from_str(&text).ok()))
    }

    /// 记下一件对外面做完的事。同一件事已经记过就不改。
    pub fn record_effect(
        &self,
        task_id: &TaskId,
        kind: &str,
        key: &str,
        result: &serde_json::Value,
    ) -> Result<()> {
        let (now, text) = (now_ms(), result.to_string());
        self.store
            .write(|tx| effects::put(tx, key, task_id, kind, &text, now))?;
        Ok(())
    }

    /// 任务是在哪个工作目录里开的。没有记就是 `None`。
    pub fn task_source(&self, id: &TaskId) -> Result<Option<String>> {
        Ok(self.store.read(|c| tasks::source(c, id))?)
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
        let name = self.get_bot(bot_id).map(|bot| bot.name).unwrap_or_default();
        self.audit(
            crate::Audit::bot(&name, "task.run_started", "开始做任务（新的一次运行）")
                .room(&run.room_id)
                .task(Some(id)),
        );
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
                // 运行期间 Bot 被删了：交付卡照样要有，只是不再指向它。
                let bot_id = match bots::get(tx, &run.bot_id)? {
                    Some(bot) => bot.id,
                    None => BotId::from(String::new()),
                };
                let delivery = card(
                    &task,
                    Sender::Bot { bot_id },
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
        // 一次运行怎么收的场、任务落到了哪个状态、为什么：任务没做成时先看这一条。
        let summary = format!(
            "任务「{}」的一次运行结束了（{}），任务现在是「{}」",
            task.title,
            outcome.state.as_str(),
            state_label(task.state)
        );
        let why = match &outcome.error {
            Some(error) if !error.trim().is_empty() => error.as_str(),
            _ => outcome.note.as_str(),
        };
        self.audit(
            crate::Audit::system("task.run_finished", summary)
                .room(&task.room_id)
                .task(Some(&task.id))
                .detail(why),
        );
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
        origin: None,
        steps: vec![],
        state,
        error: None,
        rev: 0,
        created_at: now,
        updated_at: now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sa_protocol::ReportStatus;
    use sa_store::Store;

    fn engine() -> (Engine, RoomId, BotId) {
        let engine = Engine::new(Store::open_in_memory().unwrap()).unwrap();
        engine.ensure_defaults().unwrap();
        let room = engine.list_rooms().unwrap().remove(0);
        let bot = room.host_bot_id.clone().unwrap();
        (engine, room.id, bot)
    }

    fn new_task(room_id: &RoomId, owner: Option<&BotId>) -> NewTask {
        NewTask {
            room_id: room_id.clone(),
            title: "修登录 bug".into(),
            scope: "Safari 上登录按钮点了没反应".into(),
            acceptance: "测试通过".into(),
            owner_bot_id: owner.cloned(),
            created_by: Sender::User,
            workspace: String::new(),
        }
    }

    fn done(summary: &str) -> RunOutcome {
        RunOutcome {
            state: RunState::Succeeded,
            report: Some(Report {
                status: ReportStatus::Done,
                summary: summary.into(),
            }),
            error: None,
            task_state: Some(TaskState::Review),
            note: String::new(),
            artifacts: Some(vec![(
                ArtifactKind::File,
                "src/login.rs".into(),
                "修改".into(),
            )]),
        }
    }

    #[test]
    fn a_task_goes_from_open_through_a_run_to_done() {
        let (engine, room_id, bot) = engine();
        let task = engine.create_task(new_task(&room_id, Some(&bot))).unwrap();
        assert_eq!(task.state, TaskState::Open);

        let (run, message, _session) = engine.start_run(&task.id, &bot).unwrap();
        assert_eq!(run.attempt, 1);
        assert_eq!(message.kind, MessageKind::Run);
        assert_eq!(message.state, MessageState::Streaming);
        assert_eq!(engine.get_task(&task.id).unwrap().state, TaskState::Working);
        // 同一个任务不能同时再开一次运行。
        assert!(engine.start_run(&task.id, &bot).is_err());

        let task = engine.finish_run(&run.id, done("修好了")).unwrap();
        assert_eq!(task.state, TaskState::Review);
        assert_eq!(task.artifacts.len(), 1);
        engine
            .finish_message(&message.id, MessageState::Complete, None)
            .unwrap();

        // 时间线上依次是：任务卡、运行过程、交付卡。
        let page = engine.list_messages(&room_id, None, 50).unwrap();
        let kinds: Vec<_> = page.messages.iter().map(|m| m.kind).collect();
        assert_eq!(
            kinds,
            [MessageKind::Task, MessageKind::Run, MessageKind::Delivery]
        );
        assert_eq!(page.messages[2].content, "修好了");
        assert_eq!(page.messages[2].run_id, Some(run.id.clone()));
        assert!(page
            .messages
            .iter()
            .all(|m| m.task_id.as_ref() == Some(&task.id)));
        // 人开的任务卡不算人说的话，不会引出 Bot 的回复。
        assert!(engine.user_messages_after(&room_id, 0).unwrap().is_empty());

        let task = engine.move_task(&task.id, TaskState::Done, "").unwrap();
        assert_eq!(task.state, TaskState::Done);
        assert!(engine.move_task(&task.id, TaskState::Open, "").is_err());
        let runs = engine.task_runs(&task.id).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].state, RunState::Succeeded);
    }

    #[test]
    fn rejecting_reopens_the_task_and_carries_the_note_to_the_next_run() {
        let (engine, room_id, bot) = engine();
        let task = engine.create_task(new_task(&room_id, Some(&bot))).unwrap();
        let (run, _, session) = engine.start_run(&task.id, &bot).unwrap();
        engine.finish_run(&run.id, done("第一版")).unwrap();

        let task = engine
            .resume_task(&task.id, "还要兼容旧版本", None)
            .unwrap();
        assert_eq!(task.state, TaskState::Open);
        assert_eq!(task.note, "还要兼容旧版本");
        let (second, _, again) = engine.start_run(&task.id, &bot).unwrap();
        assert_eq!(second.attempt, 2);
        // 重做时接着同一个工作会话。
        assert_eq!(session, again);
        assert_eq!(engine.take_task_input(&task.id).unwrap(), "还要兼容旧版本");
        assert_eq!(engine.take_task_input(&task.id).unwrap(), "");

        // 正在做的时候人补一句：只带话，不改状态。
        let task = engine.resume_task(&task.id, "顺便更新文档", None).unwrap();
        assert_eq!(task.state, TaskState::Working);
        assert!(engine.resume_task(&task.id, "", None).is_err());
        assert_eq!(engine.take_task_input(&task.id).unwrap(), "顺便更新文档");

        // 重做之后文件类交付物换成新的一份。
        let mut outcome = done("第二版");
        outcome.artifacts = Some(vec![(
            ArtifactKind::File,
            "src/compat.rs".into(),
            "新增".into(),
        )]);
        let task = engine.finish_run(&second.id, outcome).unwrap();
        let files: Vec<_> = task
            .artifacts
            .iter()
            .map(|a| a.reference.as_str())
            .collect();
        assert_eq!(files, ["src/compat.rs"]);
    }

    #[test]
    fn a_draft_needs_an_owner_from_the_room_before_it_can_start() {
        let (engine, room_id, bot) = engine();
        let task = engine.create_task(new_task(&room_id, None)).unwrap();
        assert_eq!(task.state, TaskState::Draft);
        assert!(engine.start_run(&task.id, &bot).is_err());
        assert!(engine.resume_task(&task.id, "", None).is_err());

        let outsider = engine
            .create_bot("外人", sa_protocol::ModelTier::Mid, "")
            .unwrap();
        assert!(engine
            .resume_task(&task.id, "", Some(&outsider.id))
            .is_err());
        assert!(engine
            .create_task(new_task(&room_id, Some(&outsider.id)))
            .is_err());

        let task = engine.resume_task(&task.id, "", Some(&bot)).unwrap();
        assert_eq!(task.state, TaskState::Open);
        assert_eq!(task.owner_bot_id, Some(bot));
    }

    #[test]
    fn a_run_still_ends_when_its_bot_was_deleted_meanwhile() {
        let (engine, room_id, _) = engine();
        let coder = engine
            .create_bot("Coder", sa_protocol::ModelTier::Mid, "")
            .unwrap();
        engine.add_room_member(&room_id, &coder.id).unwrap();
        let task = engine
            .create_task(new_task(&room_id, Some(&coder.id)))
            .unwrap();
        let (run, message, _) = engine.start_run(&task.id, &coder.id).unwrap();
        engine.delete_bot(&coder.id).unwrap();

        let task = engine.finish_run(&run.id, done("做完了")).unwrap();
        assert_eq!(task.state, TaskState::Review);
        assert_eq!(task.owner_bot_id, None);
        engine
            .finish_message(&message.id, MessageState::Complete, None)
            .unwrap();
        let page = engine.list_messages(&room_id, None, 50).unwrap();
        let delivery = page.messages.last().unwrap();
        assert_eq!(delivery.kind, MessageKind::Delivery);
        assert_eq!(delivery.content, "做完了");
    }

    #[test]
    fn a_decision_made_by_a_person_during_the_run_wins() {
        let (engine, room_id, bot) = engine();
        let task = engine.create_task(new_task(&room_id, Some(&bot))).unwrap();
        let (run, _, _) = engine.start_run(&task.id, &bot).unwrap();
        engine
            .move_task(&task.id, TaskState::Cancelled, "不做了")
            .unwrap();

        let mut outcome = done("其实做完了");
        outcome.state = RunState::Cancelled;
        outcome.report = None;
        let task = engine.finish_run(&run.id, outcome).unwrap();
        assert_eq!(task.state, TaskState::Cancelled);
        assert_eq!(task.note, "不做了");
    }

    #[test]
    fn restart_marks_running_runs_interrupted_and_reopens_their_tasks() {
        let (engine, room_id, bot) = engine();
        let task = engine.create_task(new_task(&room_id, Some(&bot))).unwrap();
        let (run, _, _) = engine.start_run(&task.id, &bot).unwrap();

        assert_eq!(engine.recover_runs().unwrap(), 1);
        assert_eq!(engine.recover_runs().unwrap(), 0);
        let task = engine.get_task(&task.id).unwrap();
        assert_eq!(task.state, TaskState::Open);
        let runs = engine.task_runs(&task.id).unwrap();
        assert_eq!(runs[0].id, run.id);
        assert_eq!(runs[0].state, RunState::Interrupted);
        assert!(engine.take_task_input(&task.id).unwrap().contains("中断"));
    }

    #[test]
    fn a_room_holds_a_limited_number_of_unfinished_tasks() {
        let (engine, room_id, bot) = engine();
        for _ in 0..MAX_ACTIVE_TASKS {
            engine.create_task(new_task(&room_id, Some(&bot))).unwrap();
        }
        let error = engine
            .create_task(new_task(&room_id, Some(&bot)))
            .unwrap_err();
        assert!(error.to_string().contains("没做完的任务"), "{error}");
        let mut blank = new_task(&room_id, Some(&bot));
        blank.scope = "  ".into();
        assert!(engine.create_task(blank).is_err());
    }

    /// 对外面做过的事只记第一次的结果；任务删了，记录跟着没。
    #[test]
    fn an_effect_is_recorded_once_and_read_back() {
        let (engine, room, bot) = engine();
        let task = engine.create_task(new_task(&room, Some(&bot))).unwrap();
        let key = format!("open_pr:{}", task.id);
        assert_eq!(engine.effect(&key).unwrap(), None);

        let first = serde_json::json!({ "url": "https://example.test/pr/1" });
        engine
            .record_effect(&task.id, "open_pr", &key, &first)
            .unwrap();
        let second = serde_json::json!({ "url": "https://example.test/pr/2" });
        engine
            .record_effect(&task.id, "open_pr", &key, &second)
            .unwrap();
        assert_eq!(engine.effect(&key).unwrap(), Some(first));

        let missing = TaskId::new();
        assert!(engine
            .record_effect(&missing, "open_pr", "open_pr:none", &second)
            .is_err());
    }
}
