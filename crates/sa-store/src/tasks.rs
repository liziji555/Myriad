//! 任务、运行、交付物。

use crate::{Result, StoreError};
use rusqlite::{params, Connection, OptionalExtension, Row};
use sa_protocol::{
    Artifact, ArtifactId, ArtifactKind, BotId, MessageId, Report, RoomId, Run, RunId, RunState,
    Sender, Task, TaskId, TaskState,
};

/// 任务的独立工作目录。这些只有后端用，不给界面。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workdir {
    /// Bot 在哪个目录里干活。
    pub path: String,
    /// `worktree` 或 `copy`。
    pub kind: String,
    pub branch: Option<String>,
    /// 开工时工作目录所在的提交。
    pub base_ref: String,
    /// 任务目录的根。工作目录是仓库里的一层时，`path` 是它里面对应的那一层。
    pub root: String,
    /// 任务目录的 git 目录。不在 Bot 能写的范围里。
    pub git_dir: String,
    /// 任务是从哪个工作目录开出来的；验收时改动带回这里。
    pub source: String,
}

const COLUMNS: &str = "id, room_id, title, scope, acceptance, owner_bot_id, state, note, created_by_bot_id, workdir, branch, rev, created_at, updated_at";

pub fn insert(conn: &Connection, task: &Task) -> Result<()> {
    let created_by = match &task.created_by {
        Sender::User => None,
        Sender::Bot { bot_id } => Some(bot_id.as_str()),
    };
    conn.execute(
        "INSERT INTO tasks (id, room_id, title, scope, acceptance, owner_bot_id, state, note, created_by_bot_id, rev, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            task.id.as_str(),
            task.room_id.as_str(),
            task.title,
            task.scope,
            task.acceptance,
            task.owner_bot_id.as_ref().map(BotId::as_str),
            task.state.as_str(),
            task.note,
            created_by,
            task.rev,
            task.created_at,
            task.updated_at
        ],
    )?;
    Ok(())
}

/// 用 `task` 的可变部分（标题、范围、验收标准、负责人、状态、说明、版本）覆盖已有的那一行。
pub fn update(conn: &Connection, task: &Task) -> Result<()> {
    let changed = conn.execute(
        "UPDATE tasks SET title = ?2, scope = ?3, acceptance = ?4, owner_bot_id = ?5, state = ?6, note = ?7, rev = ?8, updated_at = ?9 WHERE id = ?1",
        params![
            task.id.as_str(),
            task.title,
            task.scope,
            task.acceptance,
            task.owner_bot_id.as_ref().map(BotId::as_str),
            task.state.as_str(),
            task.note,
            task.rev,
            task.updated_at
        ],
    )?;
    if changed == 0 {
        return Err(StoreError::Corrupt(format!("任务 {} 不存在", task.id)));
    }
    Ok(())
}

pub fn get(conn: &Connection, id: &TaskId) -> Result<Option<Task>> {
    let task = conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM tasks WHERE id = ?1"),
            [id.as_str()],
            read_task,
        )
        .optional()?
        .transpose()?;
    match task {
        Some(mut task) => {
            task.artifacts = list_artifacts(conn, &task.id)?;
            Ok(Some(task))
        }
        None => Ok(None),
    }
}

/// 房间里的全部任务，按创建时间升序。
pub fn list_in_room(conn: &Connection, room_id: &RoomId) -> Result<Vec<Task>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM tasks WHERE room_id = ?1 ORDER BY created_at, id"
    ))?;
    let rows = stmt.query_map([room_id.as_str()], read_task)?;
    with_artifacts(conn, rows.map(|row| row?).collect::<Result<Vec<_>>>()?)
}

/// 所有房间里处在某个状态的任务，最早变成这个状态的在前。
pub fn list_in_state(conn: &Connection, state: TaskState) -> Result<Vec<Task>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM tasks WHERE state = ?1 ORDER BY updated_at, id"
    ))?;
    let rows = stmt.query_map([state.as_str()], read_task)?;
    with_artifacts(conn, rows.map(|row| row?).collect::<Result<Vec<_>>>()?)
}

fn with_artifacts(conn: &Connection, mut tasks: Vec<Task>) -> Result<Vec<Task>> {
    for task in &mut tasks {
        task.artifacts = list_artifacts(conn, &task.id)?;
    }
    Ok(tasks)
}

fn read_task(row: &Row<'_>) -> rusqlite::Result<Result<Task>> {
    let id: String = row.get(0)?;
    let state: String = row.get(6)?;
    let Some(state) = TaskState::parse(&state) else {
        return Ok(Err(StoreError::Corrupt(format!(
            "任务 {id} 的状态未知：{state}"
        ))));
    };
    let created_by = match row.get::<_, Option<String>>(8)? {
        None => Sender::User,
        Some(bot_id) => Sender::Bot {
            bot_id: BotId::from(bot_id),
        },
    };
    Ok(Ok(Task {
        id: TaskId::from(id),
        room_id: RoomId::from(row.get::<_, String>(1)?),
        title: row.get(2)?,
        scope: row.get(3)?,
        acceptance: row.get(4)?,
        owner_bot_id: row.get::<_, Option<String>>(5)?.map(BotId::from),
        state,
        note: row.get(7)?,
        created_by,
        workdir: row.get(9)?,
        branch: row.get(10)?,
        artifacts: vec![],
        rev: row.get(11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
    }))
}

pub fn workdir(conn: &Connection, id: &TaskId) -> Result<Option<Workdir>> {
    let row = conn
        .query_row(
            "SELECT workdir, work_kind, branch, base_ref, work_tree, git_dir, source_dir FROM tasks WHERE id = ?1",
            [id.as_str()],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            },
        )
        .optional()?;
    Ok(match row {
        Some((
            Some(path),
            Some(kind),
            branch,
            Some(base_ref),
            Some(root),
            Some(git_dir),
            Some(source),
        )) => Some(Workdir {
            path,
            kind,
            branch,
            base_ref,
            root,
            git_dir,
            source,
        }),
        _ => None,
    })
}

/// 记下任务是在哪个工作目录里开出来的。
pub fn set_source(conn: &Connection, id: &TaskId, source: &str) -> Result<()> {
    conn.execute(
        "UPDATE tasks SET source_dir = ?2 WHERE id = ?1",
        params![id.as_str(), source],
    )?;
    Ok(())
}

/// 任务是在哪个工作目录里开出来的。老数据里没有记的是 `None`。
pub fn source(conn: &Connection, id: &TaskId) -> Result<Option<String>> {
    let source: Option<Option<String>> = conn
        .query_row(
            "SELECT source_dir FROM tasks WHERE id = ?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(source.flatten().filter(|source| !source.is_empty()))
}

/// 人上一次让任务继续时它已经花了多少。
pub fn spend_floor(conn: &Connection, id: &TaskId) -> Result<i64> {
    Ok(conn
        .query_row(
            "SELECT spend_floor FROM tasks WHERE id = ?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

pub fn set_spend_floor(conn: &Connection, id: &TaskId, floor: i64) -> Result<()> {
    conn.execute(
        "UPDATE tasks SET spend_floor = ?2 WHERE id = ?1",
        params![id.as_str(), floor],
    )?;
    Ok(())
}

/// 记下（或清掉）任务的工作目录。清掉时「在哪个工作目录里开的」留着。
pub fn set_workdir(conn: &Connection, id: &TaskId, workdir: Option<&Workdir>) -> Result<()> {
    conn.execute(
        "UPDATE tasks SET workdir = ?2, work_kind = ?3, branch = ?4, base_ref = ?5, work_tree = ?6, git_dir = ?7, source_dir = COALESCE(?8, source_dir) WHERE id = ?1",
        params![
            id.as_str(),
            workdir.map(|w| w.path.as_str()),
            workdir.map(|w| w.kind.as_str()),
            workdir.and_then(|w| w.branch.as_deref()),
            workdir.map(|w| w.base_ref.as_str()),
            workdir.map(|w| w.root.as_str()),
            workdir.map(|w| w.git_dir.as_str()),
            workdir.map(|w| w.source.as_str())
        ],
    )?;
    Ok(())
}

/// 在「还没读到的话」后面再加一段。
pub fn push_input(conn: &Connection, id: &TaskId, text: &str) -> Result<()> {
    conn.execute(
        "UPDATE tasks SET pending_input = CASE WHEN pending_input = '' THEN ?2 ELSE pending_input || char(10) || char(10) || ?2 END WHERE id = ?1",
        params![id.as_str(), text],
    )?;
    Ok(())
}

/// 取走「还没读到的话」。没有时返回空字符串。
pub fn take_input(conn: &Connection, id: &TaskId) -> Result<String> {
    let text: Option<String> = conn
        .query_row(
            "SELECT pending_input FROM tasks WHERE id = ?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    let text = text.unwrap_or_default();
    if !text.is_empty() {
        conn.execute(
            "UPDATE tasks SET pending_input = '' WHERE id = ?1",
            [id.as_str()],
        )?;
    }
    Ok(text)
}

const RUN_COLUMNS: &str =
    "id, task_id, room_id, bot_id, message_id, state, attempt, started_at, ended_at, report_json, error";

pub fn insert_run(conn: &Connection, run: &Run) -> Result<()> {
    conn.execute(
        "INSERT INTO runs (id, task_id, room_id, bot_id, message_id, state, attempt, started_at, ended_at, report_json, error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            run.id.as_str(),
            run.task_id.as_str(),
            run.room_id.as_str(),
            run.bot_id.as_str(),
            run.message_id.as_str(),
            run.state.as_str(),
            run.attempt,
            run.started_at,
            run.ended_at,
            report_to_json(run.report.as_ref()),
            run.error
        ],
    )?;
    Ok(())
}

/// 用 `run` 的可变部分（状态、结束时间、结果、错误）覆盖已有的那一行。
pub fn update_run(conn: &Connection, run: &Run) -> Result<()> {
    let changed = conn.execute(
        "UPDATE runs SET state = ?2, ended_at = ?3, report_json = ?4, error = ?5 WHERE id = ?1",
        params![
            run.id.as_str(),
            run.state.as_str(),
            run.ended_at,
            report_to_json(run.report.as_ref()),
            run.error
        ],
    )?;
    if changed == 0 {
        return Err(StoreError::Corrupt(format!("运行 {} 不存在", run.id)));
    }
    Ok(())
}

pub fn get_run(conn: &Connection, id: &RunId) -> Result<Option<Run>> {
    conn.query_row(
        &format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = ?1"),
        [id.as_str()],
        read_run,
    )
    .optional()?
    .transpose()
}

/// 任务的全部运行，按开始时间升序。
pub fn list_runs(conn: &Connection, task_id: &TaskId) -> Result<Vec<Run>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {RUN_COLUMNS} FROM runs WHERE task_id = ?1 ORDER BY attempt"
    ))?;
    let rows = stmt.query_map([task_id.as_str()], read_run)?;
    rows.map(|row| row?).collect()
}

/// 所有还在进行的运行。
pub fn list_running(conn: &Connection) -> Result<Vec<Run>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {RUN_COLUMNS} FROM runs WHERE state = 'running' ORDER BY started_at"
    ))?;
    let rows = stmt.query_map([], read_run)?;
    rows.map(|row| row?).collect()
}

/// `since` 之后结束的运行里，`(失败的, 成功和失败加起来的)` 各有几次。被停下和被中断的不算。
pub fn run_counts_since(conn: &Connection, since: i64) -> Result<(i64, i64)> {
    Ok(conn.query_row(
        "SELECT COALESCE(SUM(state = 'failed'), 0), COUNT(*) FROM runs
         WHERE ended_at >= ?1 AND state IN ('succeeded', 'failed')",
        [since],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?)
}

fn report_to_json(report: Option<&Report>) -> Option<String> {
    report.map(|r| serde_json::to_string(r).expect("运行结果总能序列化成 JSON"))
}

fn read_run(row: &Row<'_>) -> rusqlite::Result<Result<Run>> {
    let id: String = row.get(0)?;
    let state: String = row.get(5)?;
    let report_json: Option<String> = row.get(9)?;
    let parsed = (|| {
        let state = RunState::parse(&state)
            .ok_or_else(|| StoreError::Corrupt(format!("运行 {id} 的状态未知：{state}")))?;
        let report = report_json
            .map(|json| serde_json::from_str::<Report>(&json))
            .transpose()
            .map_err(|e| StoreError::Corrupt(format!("运行 {id} 的结果不是合法 JSON：{e}")))?;
        Ok((state, report))
    })();
    let (state, report) = match parsed {
        Ok(parts) => parts,
        Err(e) => return Ok(Err(e)),
    };
    Ok(Ok(Run {
        id: RunId::from(id),
        task_id: TaskId::from(row.get::<_, String>(1)?),
        room_id: RoomId::from(row.get::<_, String>(2)?),
        bot_id: BotId::from(row.get::<_, String>(3)?),
        message_id: MessageId::from(row.get::<_, String>(4)?),
        state,
        attempt: row.get(6)?,
        started_at: row.get(7)?,
        ended_at: row.get(8)?,
        report,
        error: row.get(10)?,
    }))
}

pub fn insert_artifact(conn: &Connection, artifact: &Artifact) -> Result<()> {
    conn.execute(
        "INSERT INTO artifacts (id, task_id, run_id, kind, reference, summary, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            artifact.id.as_str(),
            artifact.task_id.as_str(),
            artifact.run_id.as_str(),
            artifact.kind.as_str(),
            artifact.reference,
            artifact.summary,
            artifact.created_at
        ],
    )?;
    Ok(())
}

/// 删掉任务的某一类交付物。返回删了几条。
pub fn delete_artifacts(conn: &Connection, task_id: &TaskId, kind: ArtifactKind) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM artifacts WHERE task_id = ?1 AND kind = ?2",
        params![task_id.as_str(), kind.as_str()],
    )?)
}

/// 任务的交付物，按产生的先后。
pub fn list_artifacts(conn: &Connection, task_id: &TaskId) -> Result<Vec<Artifact>> {
    let mut stmt = conn.prepare(
        "SELECT id, task_id, run_id, kind, reference, summary, created_at FROM artifacts WHERE task_id = ?1 ORDER BY created_at, rowid",
    )?;
    let rows = stmt.query_map([task_id.as_str()], |row| {
        let id: String = row.get(0)?;
        let kind: String = row.get(3)?;
        let Some(kind) = ArtifactKind::parse(&kind) else {
            return Ok(Err(StoreError::Corrupt(format!(
                "交付物 {id} 的种类未知：{kind}"
            ))));
        };
        Ok(Ok(Artifact {
            id: ArtifactId::from(id),
            task_id: TaskId::from(row.get::<_, String>(1)?),
            run_id: RunId::from(row.get::<_, String>(2)?),
            kind,
            reference: row.get(4)?,
            summary: row.get(5)?,
            created_at: row.get(6)?,
        }))
    })?;
    rows.map(|row| row?).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{rooms, Store};
    use sa_protocol::{ReportStatus, Room};

    fn seeded() -> (Store, RoomId) {
        let store = Store::open_in_memory().unwrap();
        let room = Room {
            id: RoomId::new(),
            name: "房间".into(),
            host_bot_id: None,
            member_bot_ids: vec![],
            created_at: 1,
            updated_at: 1,
        };
        store.write(|tx| rooms::insert(tx, &room)).unwrap();
        (store, room.id)
    }

    fn task(room_id: &RoomId, title: &str, at: i64) -> Task {
        Task {
            id: TaskId::new(),
            room_id: room_id.clone(),
            title: title.into(),
            scope: "把它修好".into(),
            acceptance: String::new(),
            owner_bot_id: None,
            state: TaskState::Open,
            note: String::new(),
            created_by: Sender::User,
            workdir: None,
            branch: None,
            artifacts: vec![],
            rev: 0,
            created_at: at,
            updated_at: at,
        }
    }

    fn run(task: &Task, attempt: u32) -> Run {
        Run {
            id: RunId::new(),
            task_id: task.id.clone(),
            room_id: task.room_id.clone(),
            bot_id: BotId::from("bot_x".to_string()),
            message_id: MessageId::new(),
            state: RunState::Running,
            attempt,
            started_at: 10,
            ended_at: None,
            report: None,
            error: None,
        }
    }

    #[test]
    fn tasks_round_trip_with_their_artifacts() {
        let (store, room_id) = seeded();
        let mut first = task(&room_id, "第一个", 1);
        let second = task(&room_id, "第二个", 2);
        store
            .write(|tx| {
                insert(tx, &first)?;
                insert(tx, &second)
            })
            .unwrap();

        let artifact = Artifact {
            id: ArtifactId::new(),
            task_id: first.id.clone(),
            run_id: RunId::new(),
            kind: ArtifactKind::File,
            reference: "src/a.rs".into(),
            summary: "修改".into(),
            created_at: 5,
        };
        first.state = TaskState::Review;
        first.note = "等验收".into();
        first.rev = 1;
        store
            .write(|tx| {
                insert_artifact(tx, &artifact)?;
                update(tx, &first)
            })
            .unwrap();

        let listed = store.read(|c| list_in_room(c, &room_id)).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].title, "第一个");
        assert_eq!(listed[0].state, TaskState::Review);
        assert_eq!(listed[0].artifacts, [artifact]);
        assert!(listed[1].artifacts.is_empty());
        let open = store.read(|c| list_in_state(c, TaskState::Open)).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, second.id);

        let removed = store
            .write(|tx| delete_artifacts(tx, &first.id, ArtifactKind::File))
            .unwrap();
        assert_eq!(removed, 1);
        assert!(store
            .read(|c| get(c, &first.id))
            .unwrap()
            .unwrap()
            .artifacts
            .is_empty());
    }

    #[test]
    fn a_task_has_at_most_one_running_run() {
        let (store, room_id) = seeded();
        let task = task(&room_id, "修 bug", 1);
        let mut first = run(&task, 1);
        store
            .write(|tx| {
                insert(tx, &task)?;
                insert_run(tx, &first)
            })
            .unwrap();
        let clash = store.write(|tx| insert_run(tx, &run(&task, 2)));
        assert!(clash.unwrap_err().is_unique_violation());

        first.state = RunState::Succeeded;
        first.ended_at = Some(20);
        first.report = Some(Report {
            status: ReportStatus::Done,
            summary: "修好了".into(),
        });
        store.write(|tx| update_run(tx, &first)).unwrap();
        store.write(|tx| insert_run(tx, &run(&task, 2))).unwrap();

        let runs = store.read(|c| list_runs(c, &task.id)).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0], first);
        assert_eq!(store.read(list_running).unwrap().len(), 1);
    }

    #[test]
    fn pending_input_accumulates_and_is_taken_once() {
        let (store, room_id) = seeded();
        let task = task(&room_id, "修 bug", 1);
        store.write(|tx| insert(tx, &task)).unwrap();
        store
            .write(|tx| {
                push_input(tx, &task.id, "先看日志")?;
                push_input(tx, &task.id, "别动配置")
            })
            .unwrap();
        let taken = store.write(|tx| take_input(tx, &task.id)).unwrap();
        assert_eq!(taken, "先看日志\n\n别动配置");
        assert_eq!(store.write(|tx| take_input(tx, &task.id)).unwrap(), "");
    }

    #[test]
    fn workdir_is_stored_and_cleared() {
        let (store, room_id) = seeded();
        let task = task(&room_id, "修 bug", 1);
        store.write(|tx| insert(tx, &task)).unwrap();
        assert_eq!(store.read(|c| workdir(c, &task.id)).unwrap(), None);
        let dir = Workdir {
            path: "/data/work/task_x".into(),
            kind: "worktree".into(),
            branch: Some("sa/task-x".into()),
            base_ref: "abc123".into(),
            root: "/data/work/task_x".into(),
            git_dir: "/repo/.git/worktrees/task_x".into(),
            source: "/repo".into(),
        };
        store
            .write(|tx| set_workdir(tx, &task.id, Some(&dir)))
            .unwrap();
        assert_eq!(
            store.read(|c| workdir(c, &task.id)).unwrap(),
            Some(dir.clone())
        );
        let stored = store.read(|c| get(c, &task.id)).unwrap().unwrap();
        assert_eq!(stored.workdir.as_deref(), Some(dir.path.as_str()));
        assert_eq!(stored.branch, dir.branch);
        store.write(|tx| set_workdir(tx, &task.id, None)).unwrap();
        assert_eq!(store.read(|c| workdir(c, &task.id)).unwrap(), None);
    }
}
