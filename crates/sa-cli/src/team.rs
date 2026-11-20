//! `sa bot` 和 `sa room`：在终端里管理 Bot 和房间成员。
//! 直接改数据目录里的状态库，所以和 `sa serve` 不能同时运行；服务开着时请在界面里管理。

use crate::app::open_state;
use crate::paths::DataDir;
use anyhow::{bail, Context, Result};
use sa_engine::{BotPatch, Engine};
use sa_protocol::{Bot, ModelTier, Room};
use std::io::Write;
use std::process::ExitCode;

#[derive(clap::Subcommand)]
pub enum BotCommand {
    /// 列出所有 Bot
    List,
    /// 新建一个 Bot
    Add {
        /// 名字，用来 @ 它，不能有空格
        name: String,
        /// 模型档位：cheap、mid、strong
        #[arg(long, default_value = "mid", value_parser = parse_tier)]
        tier: ModelTier,
        /// 分工和人设。第一行写它负责什么，路由和别的 Bot 靠这一行决定找不找它
        #[arg(long, default_value = "")]
        persona: String,
        /// 建好之后拉进这个房间（名字或 ID）
        #[arg(long, value_name = "房间")]
        room: Option<String>,
    },
    /// 修改一个 Bot，只改给出的项
    Set {
        /// Bot 的名字或 ID
        bot: String,
        /// 新名字
        #[arg(long)]
        name: Option<String>,
        #[arg(long, value_parser = parse_tier)]
        tier: Option<ModelTier>,
        #[arg(long)]
        persona: Option<String>,
        /// 启用
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        /// 停用：不会被点到，也不接话
        #[arg(long)]
        disable: bool,
    },
    /// 删除一个 Bot。它发过的消息会留着
    Rm {
        /// Bot 的名字或 ID
        bot: String,
    },
}

#[derive(clap::Subcommand)]
pub enum RoomCommand {
    /// 列出所有房间和里面的 Bot
    List,
    /// 新建一个房间，默认的入口 Bot 会在里面
    Add { name: String },
    /// 把一个 Bot 拉进房间
    Invite { room: String, bot: String },
    /// 把一个 Bot 请出房间
    Kick { room: String, bot: String },
    /// 换入口 Bot：没有点名、也挑不出合适的 Bot 时由它接话
    Host { room: String, bot: String },
}

pub enum Command {
    Bot(BotCommand),
    Room(RoomCommand),
}

fn parse_tier(text: &str) -> Result<ModelTier, String> {
    ModelTier::parse(text).ok_or_else(|| format!("档位只能是 cheap、mid、strong，不能是「{text}」"))
}

pub fn run(data_dir: &DataDir, command: Command) -> Result<ExitCode> {
    let (engine, _lock) = open_state(data_dir)?;
    let mut out = std::io::stdout();
    match command {
        Command::Bot(command) => bot(&engine, command, &mut out)?,
        Command::Room(command) => room(&engine, command, &mut out)?,
    }
    Ok(ExitCode::SUCCESS)
}

fn bot(engine: &Engine, command: BotCommand, out: &mut impl Write) -> Result<()> {
    match command {
        BotCommand::List => {
            for bot in engine.list_bots()? {
                let role = bot.persona.lines().next().unwrap_or("");
                let off = if bot.enabled { "" } else { "（已停用）" };
                writeln!(
                    out,
                    "{}{off}　{}　{}　{role}",
                    bot.name,
                    bot.tier.as_str(),
                    bot.id
                )?;
            }
        }
        BotCommand::Add {
            name,
            tier,
            persona,
            room,
        } => {
            // 先确认房间找得到，免得建了 Bot 才发现房间名写错。
            let room = room.map(|wanted| find_room(engine, &wanted)).transpose()?;
            let bot = engine.create_bot(&name, tier, &persona)?;
            writeln!(out, "已新建 {}（{}）", bot.name, bot.id)?;
            if let Some(room) = room {
                engine.add_room_member(&room.id, &bot.id)?;
                writeln!(out, "已拉进房间「{}」", room.name)?;
            }
        }
        BotCommand::Set {
            bot,
            name,
            tier,
            persona,
            enable,
            disable,
        } => {
            let enabled = match (enable, disable) {
                (true, _) => Some(true),
                (_, true) => Some(false),
                _ => None,
            };
            if name.is_none() && tier.is_none() && persona.is_none() && enabled.is_none() {
                bail!("没有要改的内容。可以用 --name、--tier、--persona、--enable、--disable");
            }
            let target = find_bot(engine, &bot)?;
            let patch = BotPatch {
                name,
                tier,
                persona,
                enabled,
            };
            let updated = engine.update_bot(&target.id, patch)?;
            let off = if updated.enabled { "" } else { "，已停用" };
            writeln!(
                out,
                "已更新 {}（{}{off}）",
                updated.name,
                updated.tier.as_str()
            )?;
        }
        BotCommand::Rm { bot } => {
            let target = find_bot(engine, &bot)?;
            engine.delete_bot(&target.id)?;
            writeln!(out, "已删除 {}", target.name)?;
        }
    }
    Ok(())
}

fn room(engine: &Engine, command: RoomCommand, out: &mut impl Write) -> Result<()> {
    match command {
        RoomCommand::List => {
            let bots = engine.list_bots()?;
            let mut rooms = engine.list_rooms()?;
            rooms.sort_by_key(|room| (room.created_at, room.id.to_string()));
            for room in rooms {
                let members: Vec<String> = room
                    .member_bot_ids
                    .iter()
                    .filter_map(|id| bots.iter().find(|bot| &bot.id == id))
                    .map(|bot| {
                        let host = if Some(&bot.id) == room.host_bot_id.as_ref() {
                            "（入口）"
                        } else {
                            ""
                        };
                        format!("{}{host}", bot.name)
                    })
                    .collect();
                let members = if members.is_empty() {
                    "没有 Bot".to_string()
                } else {
                    members.join("、")
                };
                writeln!(out, "{}　{}　{members}", room.name, room.id)?;
            }
        }
        RoomCommand::Add { name } => {
            let room = engine.create_room(&name)?;
            writeln!(out, "已新建房间「{}」（{}）", room.name, room.id)?;
        }
        RoomCommand::Invite { room, bot } => {
            let (room, bot) = (find_room(engine, &room)?, find_bot(engine, &bot)?);
            engine.add_room_member(&room.id, &bot.id)?;
            writeln!(out, "{} 已在房间「{}」里", bot.name, room.name)?;
        }
        RoomCommand::Kick { room, bot } => {
            let (room, bot) = (find_room(engine, &room)?, find_bot(engine, &bot)?);
            let after = engine.remove_room_member(&room.id, &bot.id)?;
            writeln!(out, "已把 {} 请出房间「{}」", bot.name, room.name)?;
            if room.host_bot_id.as_ref() == Some(&bot.id) {
                let host = after
                    .host_bot_id
                    .and_then(|id| engine.get_bot(&id).ok())
                    .map_or("没有了".to_string(), |bot| {
                        format!("换成了 {}", bot.name)
                    });
                writeln!(out, "它原来是入口 Bot，现在入口 Bot {host}")?;
            }
        }
        RoomCommand::Host { room, bot } => {
            let (room, bot) = (find_room(engine, &room)?, find_bot(engine, &bot)?);
            engine.update_room(&room.id, None, Some(&bot.id))?;
            writeln!(out, "房间「{}」的入口 Bot 现在是 {}", room.name, bot.name)?;
        }
    }
    Ok(())
}

/// 按名字（不分大小写）或 ID 找 Bot。
fn find_bot(engine: &Engine, wanted: &str) -> Result<Bot> {
    let bots = engine.list_bots()?;
    let wanted = wanted.strip_prefix('@').unwrap_or(wanted);
    let names: Vec<_> = bots.iter().map(|bot| bot.name.clone()).collect();
    bots.into_iter()
        .find(|bot| bot.id.as_str() == wanted || bot.name.to_lowercase() == wanted.to_lowercase())
        .with_context(|| format!("没有叫「{wanted}」的 Bot。现有的 Bot：{}", names.join("、")))
}

/// 按名字或 ID 找房间。
pub fn find_room(engine: &Engine, wanted: &str) -> Result<Room> {
    let rooms = engine.list_rooms()?;
    let names: Vec<_> = rooms.iter().map(|room| room.name.clone()).collect();
    let mut matches: Vec<Room> = rooms
        .into_iter()
        .filter(|room| room.name == wanted || room.id.as_str() == wanted)
        .collect();
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => bail!("没有叫「{wanted}」的房间。现有的房间：{}", names.join("、")),
        _ => bail!("有 {} 个房间都叫「{wanted}」，请改用房间 ID", matches.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use sa_store::Store;

    #[derive(Parser)]
    struct BotCli {
        #[command(subcommand)]
        command: BotCommand,
    }

    #[derive(Parser)]
    struct RoomCli {
        #[command(subcommand)]
        command: RoomCommand,
    }

    fn engine() -> Engine {
        let engine = Engine::new(Store::open_in_memory().unwrap()).unwrap();
        engine.ensure_defaults().unwrap();
        engine
    }

    /// 像在终端里敲命令一样跑一条，返回打印出来的内容。
    fn sa(engine: &Engine, line: &str) -> Result<String> {
        let mut words = line.split_whitespace();
        let group = words.next().unwrap();
        let args = std::iter::once("sa").chain(words);
        let mut out = Vec::new();
        match group {
            "bot" => bot(engine, BotCli::try_parse_from(args)?.command, &mut out)?,
            "room" => room(engine, RoomCli::try_parse_from(args)?.command, &mut out)?,
            other => panic!("不认识的命令 {other}"),
        }
        Ok(String::from_utf8(out).unwrap())
    }

    #[test]
    fn bots_and_rooms_are_managed_from_the_terminal() {
        let engine = engine();
        let room_name = engine.list_rooms().unwrap()[0].name.clone();

        let out = sa(
            &engine,
            &format!("bot add Coder --persona 写代码 --room {room_name}"),
        )
        .unwrap();
        assert!(
            out.contains("已新建 Coder") && out.contains("已拉进房间"),
            "{out}"
        );
        sa(&engine, "bot add Reviewer --tier strong").unwrap();
        let listed = sa(&engine, "bot list").unwrap();
        assert!(
            listed.contains("Coder　mid") && listed.contains("写代码"),
            "{listed}"
        );
        assert!(listed.contains("Reviewer　strong"), "{listed}");

        // 名字不分大小写，带不带 @ 都行。
        sa(&engine, &format!("room invite {room_name} @reviewer")).unwrap();
        sa(&engine, &format!("room host {room_name} coder")).unwrap();
        let rooms = sa(&engine, "room list").unwrap();
        assert!(rooms.contains("助手、Coder（入口）、Reviewer"), "{rooms}");

        let out = sa(&engine, &format!("room kick {room_name} Coder")).unwrap();
        assert!(out.contains("现在入口 Bot 换成了 助手"), "{out}");

        let out = sa(&engine, "bot set Reviewer --name Critic --disable").unwrap();
        assert!(out.contains("已更新 Critic（strong，已停用）"), "{out}");
        assert!(sa(&engine, "bot list")
            .unwrap()
            .contains("Critic（已停用）"));
        sa(&engine, "bot rm critic").unwrap();
        assert!(!sa(&engine, "bot list").unwrap().contains("Critic"));

        sa(&engine, "room add 调研").unwrap();
        assert!(sa(&engine, "room list").unwrap().contains("调研"));
    }

    #[test]
    fn mistakes_are_explained() {
        let engine = engine();
        let error = |line: &str| format!("{:#}", sa(&engine, line).unwrap_err());
        assert!(error("bot set 助手").contains("没有要改的内容"));
        assert!(error("bot rm Nobody").contains("现有的 Bot：助手"));
        assert!(error("bot add X --tier huge").contains("档位只能是"));
        assert!(error("bot add X --room 不存在").contains("没有叫「不存在」的房间"));
        // 房间名写错时不会留下一个建了一半的 Bot。
        assert_eq!(engine.list_bots().unwrap().len(), 1);
        assert!(error("bot set 助手 --enable --disable").contains("cannot be used with"));
    }
}
