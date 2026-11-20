//! `sa routine`：在终端里管理定时任务。到点之后由 `sa serve` 来做。

use crate::app::open_state;
use crate::paths::DataDir;
use crate::team::{find_bot, find_room};
use anyhow::{bail, Result};
use chrono::{Local, TimeZone};
use sa_engine::{Engine, RoutinePatch};
use sa_protocol::{Routine, RoutineResult};
use std::io::Write;
use std::process::ExitCode;

#[derive(clap::Subcommand)]
pub enum RoutineCommand {
    /// 列出所有定时任务
    List,
    /// 新建一个定时任务
    Add {
        /// 房间的名字或 ID
        #[arg(long)]
        room: String,
        /// 由哪个 Bot 做（名字或 ID）。不写就是房间的入口 Bot
        #[arg(long)]
        bot: Option<String>,
        /// 名字
        #[arg(long)]
        title: String,
        /// 什么时候做：五段式 cron（分 时 日 月 周），按本机时间。每天早上九点是 "0 9 * * *"
        #[arg(long)]
        schedule: String,
        /// 到点时让 Bot 做什么
        prompt: String,
    },
    /// 删掉一个定时任务
    Remove { id: String },
    /// 启用
    Enable { id: String },
    /// 停用
    Disable { id: String },
}

pub fn run(data_dir: &DataDir, command: RoutineCommand) -> Result<ExitCode> {
    let (engine, _lock) = open_state(data_dir)?;
    routine(&engine, command, &mut std::io::stdout())?;
    Ok(ExitCode::SUCCESS)
}

fn routine(engine: &Engine, command: RoutineCommand, out: &mut impl Write) -> Result<()> {
    match command {
        RoutineCommand::List => {
            let routines = engine.list_routines()?;
            if routines.is_empty() {
                writeln!(out, "还没有定时任务。用 sa routine add 新建一个。")?;
            }
            for item in routines {
                show(engine, &item, out)?;
            }
        }
        RoutineCommand::Add {
            room,
            bot,
            title,
            schedule,
            prompt,
        } => {
            let room = find_room(engine, &room)?;
            let bot = bot.map(|bot| find_bot(engine, &bot)).transpose()?;
            let bot_id = bot.as_ref().map(|bot| &bot.id);
            let item = engine.create_routine(&room.id, bot_id, &title, &prompt, &schedule)?;
            writeln!(
                out,
                "建好了。到点之后由 sa serve 来做（服务没开着就不做）。"
            )?;
            show(engine, &item, out)?;
        }
        RoutineCommand::Remove { id } => {
            let item = find(engine, &id)?;
            engine.delete_routine(&item.id)?;
            writeln!(out, "删掉了「{}」。", item.title)?;
        }
        RoutineCommand::Enable { id } => show(engine, &switch(engine, &id, true)?, out)?,
        RoutineCommand::Disable { id } => show(engine, &switch(engine, &id, false)?, out)?,
    }
    Ok(())
}

fn switch(engine: &Engine, id: &str, enabled: bool) -> Result<Routine> {
    let item = find(engine, id)?;
    let patch = RoutinePatch {
        enabled: Some(enabled),
        ..Default::default()
    };
    Ok(engine.update_routine(&item.id, patch)?)
}

/// 按 ID 或者名字找。
fn find(engine: &Engine, wanted: &str) -> Result<Routine> {
    let mut found: Vec<Routine> = engine
        .list_routines()?
        .into_iter()
        .filter(|item| item.id.as_str() == wanted || item.title == wanted)
        .collect();
    match found.len() {
        0 => bail!("没有叫「{wanted}」的定时任务。用 sa routine list 看有哪些"),
        1 => Ok(found.remove(0)),
        _ => bail!("有好几个定时任务叫「{wanted}」，请用 ID"),
    }
}

fn show(engine: &Engine, item: &Routine, out: &mut impl Write) -> Result<()> {
    let room = engine.get_room(&item.room_id)?.name;
    let bot = engine
        .list_bots()?
        .into_iter()
        .find(|bot| bot.id == item.bot_id)
        .map_or_else(|| "（已删除）".to_string(), |bot| bot.name);
    let state = if item.enabled { "启用" } else { "停用" };
    writeln!(
        out,
        "{}  [{state}]  {}  {}  {room} / {bot}",
        item.id,
        item.schedule,
        crate::chat::printable(&item.title)
    )?;
    if let Some(next) = item.next_at {
        writeln!(out, "  下一次：{}", clock(next))?;
    }
    if let (Some(at), Some(result)) = (item.last_at, item.last_result) {
        let result = match result {
            RoutineResult::Spoke => "说了话",
            RoutineResult::Silent => "没什么可说",
            RoutineResult::Failed => "没做成",
            RoutineResult::Skipped => "跳过了",
        };
        let note = crate::chat::printable(&item.last_note);
        let note = if note.is_empty() {
            String::new()
        } else {
            format!("（{note}）")
        };
        writeln!(out, "  上一次：{} {result}{note}", clock(at))?;
    }
    Ok(())
}

fn clock(at: i64) -> String {
    Local
        .timestamp_millis_opt(at)
        .single()
        .map(|time| time.format("%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use sa_store::Store;

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        command: RoutineCommand,
    }

    fn sa(engine: &Engine, args: &[&str]) -> Result<String> {
        let args = std::iter::once("sa").chain(args.iter().copied());
        let mut out = Vec::new();
        routine(engine, Cli::try_parse_from(args)?.command, &mut out)?;
        Ok(String::from_utf8(out).unwrap())
    }

    #[test]
    fn routines_are_managed_from_the_terminal() {
        let engine = Engine::new(Store::open_in_memory().unwrap()).unwrap();
        engine.ensure_defaults().unwrap();
        let room = engine.list_rooms().unwrap().remove(0).name;
        assert!(sa(&engine, &["list"]).unwrap().contains("还没有"));

        let add = [
            "add",
            "--room",
            &room,
            "--title",
            "早报",
            "--schedule",
            "0 9 * * *",
            "看看新消息",
        ];
        let shown = sa(&engine, &add).unwrap();
        assert!(shown.contains("[启用]  0 9 * * *  早报"), "{shown}");
        assert!(shown.contains("下一次："), "{shown}");

        let shown = sa(&engine, &["disable", "早报"]).unwrap();
        assert!(
            shown.contains("[停用]") && !shown.contains("下一次"),
            "{shown}"
        );
        let shown = sa(&engine, &["enable", "早报"]).unwrap();
        assert!(
            shown.contains("[启用]") && shown.contains("下一次"),
            "{shown}"
        );

        // 写错的时间、不存在的名字都说明原因。
        let bad = [
            "add",
            "--room",
            &room,
            "--title",
            "晚报",
            "--schedule",
            "九点",
            "总结",
        ];
        assert!(sa(&engine, &bad).unwrap_err().to_string().contains("五段"));
        let error = sa(&engine, &["remove", "晚报"]).unwrap_err().to_string();
        assert!(error.contains("没有叫「晚报」的定时任务"), "{error}");

        assert!(sa(&engine, &["remove", "早报"])
            .unwrap()
            .contains("删掉了「早报」"));
        assert!(engine.list_routines().unwrap().is_empty());
    }
}
