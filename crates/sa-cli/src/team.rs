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
    /// 把 Bot 导出成一份模板（TOML），可以拿到别的机器上导入。里面没有密钥和聊天记录
    Export {
        /// 要导出的 Bot（名字或 ID）。不写就是全部
        bots: Vec<String>,
        /// 导出这个房间的团队：成员、谁是入口、房间叫什么
        #[arg(long, value_name = "房间", conflicts_with = "bots")]
        room: Option<String>,
        /// 写到这个文件里。不写就打印出来
        #[arg(short, long, value_name = "文件")]
        output: Option<std::path::PathBuf>,
    },
    /// 导入一份模板：新建里面的 Bot（已经有同名的跳过），模板带了房间就新建那个房间
    Import {
        /// 模板文件
        file: std::path::PathBuf,
        /// 已经有同名的 Bot 时，照模板改它的档位、分工、是否启用（默认不动它）
        #[arg(long)]
        update: bool,
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
        BotCommand::Export { bots, room, output } => {
            let team = match room {
                Some(room) => engine.export_room(&find_room(engine, &room)?.id)?,
                None => {
                    let ids = bots
                        .iter()
                        .map(|bot| find_bot(engine, bot).map(|bot| bot.id));
                    engine.export_bots(&ids.collect::<Result<Vec<_>>>()?)?
                }
            };
            let text = sa_engine::template_text(&team);
            match output {
                Some(path) => {
                    std::fs::write(&path, text)
                        .with_context(|| format!("写不了 {}", path.display()))?;
                    writeln!(
                        out,
                        "已导出 {} 个 Bot 到 {}",
                        team.bots.len(),
                        path.display()
                    )?;
                }
                None => out.write_all(text.as_bytes())?,
            }
        }
        BotCommand::Import { file, update } => {
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("读不了 {}", file.display()))?;
            let team = sa_engine::parse_template(&text)
                .with_context(|| format!("读不懂 {}", file.display()))?;
            let report = engine.import_team(&team, update)?;
            for bot in &report.created {
                writeln!(out, "已新建 {}（{}）", bot.name, bot.tier.as_str())?;
            }
            for bot in &report.updated {
                writeln!(out, "已照模板改了 {}（{}）", bot.name, bot.tier.as_str())?;
            }
            for name in &report.skipped {
                writeln!(out, "已经有 {name}，没有动它")?;
            }
            if !update && !report.skipped.is_empty() {
                // 模板带了房间时再导一次会多出一个同名的房间，所以不劝人重来。
                let how = if report.room.is_some() {
                    "想让已有的同名 Bot 也照模板改，用 sa bot set 改它们；\
                     再导一次会多建一个同名的房间。下次导入时可以直接加 --update。"
                } else {
                    "想让已有的同名 Bot 也照模板改，加上 --update 再导一次。"
                };
                writeln!(out, "{how}")?;
            }
            if let Some(room) = &report.room {
                writeln!(out, "已新建房间「{}」（{}）", room.name, room.id)?;
            }
            if report.created.is_empty() && report.updated.is_empty() && report.room.is_none() {
                writeln!(out, "没有新建任何东西。")?;
            }
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
pub(crate) fn find_bot(engine: &Engine, wanted: &str) -> Result<Bot> {
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
    fn a_team_is_exported_to_a_file_and_imported_somewhere_else() {
        let here = engine();
        sa(&here, "room add 修bug").unwrap();
        here.create_bot(
            "Coder",
            ModelTier::Strong,
            "写代码\n改完跑测试，\"引号\" 和 # 井号都留着",
        )
        .unwrap();
        sa(&here, "bot add Reviewer --tier cheap --persona 看改动").unwrap();
        sa(&here, "room invite 修bug Coder").unwrap();
        sa(&here, "room invite 修bug Reviewer").unwrap();
        sa(&here, "room host 修bug Reviewer").unwrap();
        // 新房间自动带上的「助手」请出去：模板里只该有房间里的成员。
        sa(&here, "room kick 修bug 助手").unwrap();
        sa(&here, "bot set Coder --disable").unwrap();

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("team.toml");
        let out = sa(
            &here,
            &format!("bot export --room 修bug -o {}", file.display()),
        )
        .unwrap();
        assert!(out.contains("已导出"), "{out}");
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            text.contains("version = 1") && text.contains("[[bots]]"),
            "{text}"
        );
        assert!(
            text.contains("[room]") && text.contains(r#"host = "Reviewer""#),
            "{text}"
        );
        // 不写文件就打印出来；可以只挑几个 Bot，这时不带房间。
        let printed = sa(&here, "bot export Coder").unwrap();
        assert!(printed.contains(r#"name = "Coder""#) && !printed.contains("[room]"));
        assert!(!printed.contains("Reviewer"), "{printed}");
        assert!(sa(&here, "bot export Coder --room 修bug").is_err());
        assert!(sa(&here, "bot export 没有这个").is_err());

        let there = Engine::new(Store::open_in_memory().unwrap()).unwrap();
        let out = sa(&there, &format!("bot import {}", file.display())).unwrap();
        assert!(
            out.contains("已新建 Coder") && out.contains("已新建房间「修bug」"),
            "{out}"
        );
        let bots = there.list_bots().unwrap();
        let coder = bots.iter().find(|bot| bot.name == "Coder").unwrap();
        assert_eq!(
            coder.persona,
            "写代码\n改完跑测试，\"引号\" 和 # 井号都留着"
        );
        assert_eq!((coder.tier, coder.enabled), (ModelTier::Strong, false));
        let listed = sa(&there, "room list").unwrap();
        assert!(
            listed.contains("Reviewer（入口）") && listed.contains("Coder"),
            "{listed}"
        );
        // 再导一次不会多出 Bot。
        let out = sa(&there, &format!("bot import {}", file.display())).unwrap();
        // 这份模板带着房间：不劝人「再导一次」，那样会多出一个同名的房间。
        assert!(
            out.contains("已经有 Coder，没有动它") && out.contains("sa bot set"),
            "{out}"
        );
        assert!(!out.contains("再导一次。"), "{out}");
        // 不带房间的模板可以放心重来。
        let bots_only = dir.path().join("bots.toml");
        sa(
            &here,
            &format!("bot export Coder -o {}", bots_only.display()),
        )
        .unwrap();
        let out = sa(&there, &format!("bot import {}", bots_only.display())).unwrap();
        assert!(out.contains("加上 --update 再导一次。"), "{out}");
        assert_eq!(there.list_bots().unwrap().len(), 2);
        // 已有的同名 Bot 默认不动；加上 --update 才照模板改。
        sa(&there, "bot set Coder --tier cheap --enable").unwrap();
        sa(&there, &format!("bot import {}", file.display())).unwrap();
        let coder = find_bot(&there, "Coder").unwrap();
        assert_eq!((coder.tier, coder.enabled), (ModelTier::Cheap, true));
        let out = sa(&there, &format!("bot import {} --update", file.display())).unwrap();
        assert!(out.contains("已照模板改了 Coder（strong）"), "{out}");
        assert!(!out.contains("--update"), "{out}");
        let coder = find_bot(&there, "Coder").unwrap();
        assert_eq!((coder.tier, coder.enabled), (ModelTier::Strong, false));
        assert_eq!(there.list_bots().unwrap().len(), 2);

        // 不是模板的文件、没有的文件：说清楚，不动数据。
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "这不是 TOML = = =").unwrap();
        let error = sa(&there, &format!("bot import {}", bad.display())).unwrap_err();
        assert!(
            format!("{error:#}").contains("不是一份能认的模板"),
            "{error:#}"
        );
        std::fs::write(&bad, "version = 9\n").unwrap();
        let error = sa(&there, &format!("bot import {}", bad.display())).unwrap_err();
        assert!(format!("{error:#}").contains("版本"), "{error:#}");
        assert!(sa(&there, "bot import /没有/这个文件.toml").is_err());
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
