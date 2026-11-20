//! `sa audit`：在终端里看审计日志——谁在什么时候做了一件要留底的事。
//! 和 `sa chat` 一样直接用数据目录，所以不能和 `sa serve` 同时运行；服务开着时请在界面的「审计」里看。

use crate::app::{init_logging, open_state};
use crate::paths::DataDir;
use crate::team::find_room;
use anyhow::{bail, Result};
use chrono::{Local, TimeZone};
use clap::Args;
use sa_engine::{AuditFilter, Engine};
use sa_protocol::{AuditActor, AuditEntry, TaskId};
use std::io::Write;
use std::process::ExitCode;

#[derive(Args)]
pub struct AuditArgs {
    /// 只看这个房间的（房间名或 ID）
    #[arg(long, value_name = "房间")]
    room: Option<String>,
    /// 只看这个任务的（任务 ID，写开头几位就行）
    #[arg(long, value_name = "任务")]
    task: Option<String>,
    /// 只看这一类动作，写动作名的开头，例如 approval. 或 tool.shell
    #[arg(long, value_name = "动作")]
    action: Option<String>,
    /// 最多看几条（新的在前）
    #[arg(long, default_value_t = 50, value_name = "条数")]
    limit: u32,
    /// 一行一条 JSON，方便交给别的程序
    #[arg(long)]
    json: bool,
}

pub fn run(data_dir: &DataDir, args: AuditArgs) -> Result<ExitCode> {
    init_logging(tracing::Level::WARN);
    let (engine, _lock) = open_state(data_dir)?;
    let filter = AuditFilter {
        room_id: match &args.room {
            Some(wanted) => Some(find_room(&engine, wanted)?.id),
            None => None,
        },
        task_id: match &args.task {
            Some(wanted) => Some(find_task(&engine, wanted)?),
            None => None,
        },
        action: args.action.clone(),
        before: None,
    };
    let page = engine.list_audit(&filter, args.limit)?;
    let mut out = std::io::stdout();
    if args.json {
        for entry in &page.entries {
            writeln!(out, "{}", serde_json::to_string(entry)?)?;
        }
        return Ok(ExitCode::SUCCESS);
    }
    if page.entries.is_empty() {
        writeln!(out, "没有符合条件的审计记录。")?;
        return Ok(ExitCode::SUCCESS);
    }
    for entry in &page.entries {
        print(entry, &when(entry.at), &mut out)?;
    }
    if page.has_more {
        writeln!(out, "更早的还有；用 --limit 多看几条。")?;
    }
    Ok(ExitCode::SUCCESS)
}

/// 任务 ID 可以只写开头几位。任务删不掉，所以审计里出现过的任务都找得到。
fn find_task(engine: &Engine, wanted: &str) -> Result<TaskId> {
    let mut matches = vec![];
    for room in engine.list_rooms()? {
        for task in engine.list_tasks(&room.id)? {
            if task.id.as_str() == wanted {
                return Ok(task.id);
            }
            if task.id.as_str().starts_with(wanted) {
                matches.push(task.id);
            }
        }
    }
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => bail!("没有 ID 以「{wanted}」开头的任务"),
        many => bail!("有 {many} 个任务的 ID 都以「{wanted}」开头，请多写几位"),
    }
}

fn when(at: i64) -> String {
    match Local.timestamp_millis_opt(at).single() {
        Some(time) => time.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => at.to_string(),
    }
}

fn who(entry: &AuditEntry) -> String {
    match entry.actor {
        AuditActor::User => "你".to_string(),
        AuditActor::System => "程序".to_string(),
        AuditActor::Bot | AuditActor::Outside if entry.actor_name.is_empty() => {
            if entry.actor == AuditActor::Bot {
                "Bot"
            } else {
                "外面"
            }
            .to_string()
        }
        AuditActor::Bot | AuditActor::Outside => entry.actor_name.clone(),
    }
}

pub fn print(entry: &AuditEntry, when: &str, out: &mut impl Write) -> Result<()> {
    writeln!(
        out,
        "{when}  {}  [{}]  {}",
        who(entry),
        entry.action,
        entry.summary
    )?;
    if let Some(task) = &entry.task_id {
        writeln!(out, "    任务：{task}")?;
    }
    if let Some(detail) = &entry.detail {
        for line in detail.lines() {
            writeln!(out, "    {line}")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sa_engine::Audit;
    use sa_store::Store;

    #[test]
    fn entries_are_printed_with_who_what_and_the_details() {
        let engine = Engine::new(Store::open_in_memory().unwrap()).unwrap();
        engine.ensure_defaults().unwrap();
        let room = engine.list_rooms().unwrap().remove(0);
        engine.audit(
            Audit::bot("Coder", "tool.shell", "执行 cargo test")
                .room(&room.id)
                .detail("cargo test\n--workspace"),
        );
        engine.audit(Audit::user(
            "approval.allowed",
            "Coder 要「执行 cargo test」：允许一次",
        ));
        engine.audit(Audit::system("server.started", "sa serve 启动了"));
        let page = engine.list_audit(&AuditFilter::default(), 3).unwrap();
        let mut out = vec![];
        for entry in &page.entries {
            print(entry, "2026-01-02 03:04:05", &mut out).unwrap();
        }
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "2026-01-02 03:04:05  程序  [server.started]  sa serve 启动了\n\
             2026-01-02 03:04:05  你  [approval.allowed]  Coder 要「执行 cargo test」：允许一次\n\
             2026-01-02 03:04:05  Coder  [tool.shell]  执行 cargo test\n    cargo test\n    --workspace\n"
        );
        // 任务 ID 对不上时说清楚。
        let error = find_task(&engine, "task_nope").unwrap_err().to_string();
        assert!(
            error.contains("没有 ID 以「task_nope」开头的任务"),
            "{error}"
        );
    }
}
