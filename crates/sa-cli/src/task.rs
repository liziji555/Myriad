//! `sa task`：在终端里看任务、开任务、验收。
//! 和 `sa chat` 一样直接用数据目录，所以不能和 `sa serve` 同时运行；服务开着时请在界面里操作。

use crate::app::{init_logging, App, RunArgs};
use crate::chat::pick_room;
use crate::paths::DataDir;
use crate::team::find_bot;
use anyhow::{bail, Result};
use sa_engine::state_label;
use sa_protocol::{
    ApprovalDecision, ArtifactKind, CreateTaskRequest, MessageKind, RunState, StepState, Task,
    TaskState,
};
use std::collections::HashSet;
use std::io::Write;
use std::process::ExitCode;
use std::time::Duration;

#[derive(clap::Args)]
pub struct TaskArgs {
    #[command(subcommand)]
    command: TaskCommand,
    #[command(flatten)]
    run: RunArgs,
}

#[derive(clap::Subcommand)]
pub enum TaskCommand {
    /// 列出房间里的任务
    List {
        /// 房间的名字或 ID，默认是第一个房间
        #[arg(long, value_name = "房间")]
        room: Option<String>,
    },
    /// 看一个任务的详情、执行历史和交付物
    Show {
        /// 任务 ID（可以只写开头几位）
        task: String,
    },
    /// 开一个任务，并等它做到需要你处理的地方
    Add {
        /// 一句话的标题
        title: String,
        /// 要做什么
        #[arg(long)]
        scope: String,
        /// 怎样算做完
        #[arg(long)]
        acceptance: Option<String>,
        /// 负责的 Bot（名字或 ID），默认是房间的入口 Bot
        #[arg(long, value_name = "BOT")]
        owner: Option<String>,
        #[arg(long, value_name = "房间")]
        room: Option<String>,
    },
    /// 让等回答、卡住的任务继续
    Resume {
        task: String,
        /// 给负责的 Bot 的话（回答它的问题、补充说明）
        #[arg(long)]
        note: Option<String>,
        /// 换一个负责人
        #[arg(long, value_name = "BOT")]
        owner: Option<String>,
    },
    /// 验收通过：把任务的改动带回工作目录
    Accept { task: String },
    /// 验收不通过：带着意见打回重做
    Reject {
        task: String,
        /// 哪里不行
        #[arg(long)]
        note: String,
    },
    /// 取消任务，它的改动不保留
    Cancel {
        task: String,
        #[arg(long)]
        note: Option<String>,
    },
}

pub fn run(data_dir: &DataDir, args: TaskArgs) -> Result<ExitCode> {
    init_logging(tracing::Level::WARN);
    let app = App::open(data_dir, &args.run)?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            let mut out = std::io::stdout();
            let result = execute(&app, args.command, &mut out).await;
            app.orchestrator.shutdown().await;
            result
        })
}

async fn execute(app: &App, command: TaskCommand, out: &mut impl Write) -> Result<ExitCode> {
    let task = match command {
        TaskCommand::List { room } => {
            let room = pick_room(app, room.as_deref())?;
            let tasks = app.engine.list_tasks(&room.id)?;
            if tasks.is_empty() {
                writeln!(out, "房间「{}」里还没有任务。", room.name)?;
            }
            for task in tasks {
                writeln!(
                    out,
                    "{}  [{}]  {}",
                    task.id,
                    state_label(task.state),
                    task.title
                )?;
            }
            return Ok(ExitCode::SUCCESS);
        }
        TaskCommand::Show { task } => {
            let task = find_task(app, &task)?;
            show(app, &task, out)?;
            return Ok(ExitCode::SUCCESS);
        }
        TaskCommand::Add {
            title,
            scope,
            acceptance,
            owner,
            room,
        } => {
            let room = pick_room(app, room.as_deref())?;
            let owner_bot_id = match owner {
                Some(name) => Some(find_bot(&app.engine, &name)?.id),
                None => room.host_bot_id.clone(),
            };
            let request = CreateTaskRequest {
                title,
                scope,
                acceptance,
                owner_bot_id,
            };
            let task = app.orchestrator.create_task(&room.id, request).await?;
            writeln!(out, "任务 {} 已经开了。", task.id)?;
            wait(app, task, out).await?
        }
        TaskCommand::Resume { task, note, owner } => {
            let task = find_task(app, &task)?;
            let owner = match owner {
                Some(name) => Some(find_bot(&app.engine, &name)?.id),
                None => None,
            };
            let task = app
                .orchestrator
                .resume_task(&task.id, note.unwrap_or_default(), owner)
                .await?;
            wait(app, task, out).await?
        }
        TaskCommand::Reject { task, note } => {
            let task = find_task(app, &task)?;
            let task = app.orchestrator.reject_task(&task.id, note).await?;
            wait(app, task, out).await?
        }
        TaskCommand::Accept { task } => {
            let task = find_task(app, &task)?;
            app.orchestrator.accept_task(&task.id).await?
        }
        TaskCommand::Cancel { task, note } => {
            let task = find_task(app, &task)?;
            app.orchestrator
                .cancel_task(&task.id, note.unwrap_or_default())
                .await?
        }
    };
    show(app, &task, out)?;
    Ok(match task.state {
        TaskState::Blocked => ExitCode::FAILURE,
        _ => ExitCode::SUCCESS,
    })
}

/// 按 ID 或 ID 的开头找任务。
fn find_task(app: &App, wanted: &str) -> Result<Task> {
    let mut matches = vec![];
    for room in app.engine.list_rooms()? {
        for task in app.engine.list_tasks(&room.id)? {
            if task.id.as_str() == wanted {
                return Ok(task);
            }
            if task.id.as_str().starts_with(wanted) {
                matches.push(task);
            }
        }
    }
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => bail!("没有 ID 以「{wanted}」开头的任务。用 sa task list 看有哪些"),
        _ => bail!(
            "有 {} 个任务的 ID 以「{wanted}」开头，请多写几位",
            matches.len()
        ),
    }
}

/// 等任务做到需要人处理的地方（等验收、等回答、卡住）或者结束。中间有命令要批准就在终端里问。
async fn wait(app: &App, task: Task, out: &mut impl Write) -> Result<Task> {
    let mut asked = HashSet::new();
    loop {
        let now = app.engine.get_task(&task.id)?;
        let running = app
            .engine
            .task_runs(&task.id)?
            .iter()
            .any(|run| run.state == RunState::Running);
        if !running && !matches!(now.state, TaskState::Open | TaskState::Working) {
            return Ok(now);
        }
        let page = app.engine.list_messages(&now.room_id, None, 50)?;
        let waiting = page
            .messages
            .iter()
            .filter(|m| m.kind == MessageKind::Run && m.task_id.as_ref() == Some(&task.id))
            .flat_map(|m| m.steps.iter())
            .find(|step| step.state == StepState::Waiting);
        if let Some((step, approval)) = waiting.and_then(|s| Some((s, s.approval.clone()?))) {
            if asked.insert(approval.id.clone()) {
                write!(
                    out,
                    "任务要{}（{}）。允许吗？[y/N] ",
                    step.title, approval.reason
                )?;
                out.flush()?;
                let answer = tokio::task::spawn_blocking(|| {
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line).map(|_| line)
                })
                .await??;
                let decision = match answer.trim().to_lowercase().as_str() {
                    "y" | "yes" | "是" => ApprovalDecision::Once,
                    _ => ApprovalDecision::Deny,
                };
                app.orchestrator.decide(&approval.id, decision);
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn show(app: &App, task: &Task, out: &mut impl Write) -> Result<()> {
    writeln!(
        out,
        "{}  [{}]  {}",
        task.id,
        state_label(task.state),
        task.title
    )?;
    if !task.note.is_empty() {
        writeln!(out, "  说明：{}", task.note)?;
    }
    writeln!(out, "  要做什么：{}", task.scope)?;
    if !task.acceptance.is_empty() {
        writeln!(out, "  验收标准：{}", task.acceptance)?;
    }
    for run in app.engine.task_runs(&task.id)? {
        let state = match run.state {
            RunState::Running => "进行中",
            RunState::Succeeded => "结束",
            RunState::Failed => "出错",
            RunState::Cancelled => "被停下",
            RunState::Interrupted => "中断",
        };
        let detail = match (&run.report, &run.error) {
            (Some(report), _) => report.summary.clone(),
            (None, Some(error)) => error.clone(),
            (None, None) => String::new(),
        };
        writeln!(out, "  第 {} 次运行（{state}）：{detail}", run.attempt)?;
    }
    for artifact in &task.artifacts {
        let kind = match artifact.kind {
            ArtifactKind::Pr => "PR",
            ArtifactKind::Branch => "分支",
            ArtifactKind::File => "文件",
            ArtifactKind::Report => "报告",
        };
        writeln!(
            out,
            "  {kind}：{}（{}）",
            artifact.reference, artifact.summary
        )?;
    }
    match task.state {
        TaskState::Review => writeln!(
            out,
            "  下一步：sa task accept {0} 验收通过，或者 sa task reject {0} --note \"哪里不行\"",
            task.id
        )?,
        TaskState::Waiting | TaskState::Blocked => writeln!(
            out,
            "  下一步：sa task resume {} --note \"你的回答或补充\"",
            task.id
        )?,
        _ => {}
    }
    Ok(())
}
