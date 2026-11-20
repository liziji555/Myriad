//! `sa link`：在终端里管理房间和外面的连接。Telegram 聊天要在界面里用配对码来连。

use crate::app::open_state;
use crate::paths::DataDir;
use crate::team::find_room;
use anyhow::{bail, Result};
use sa_engine::Engine;
use sa_protocol::{Link, Source};
use std::io::Write;
use std::process::ExitCode;

#[derive(clap::Subcommand)]
pub enum LinkCommand {
    /// 列出所有连接
    List,
    /// 把一个 GitHub 仓库连到房间：这个仓库的 issue、评论、CI 结果进这个房间
    Github {
        /// 房间的名字或 ID
        room: String,
        /// 仓库，写成 owner/name
        repo: String,
    },
    /// 断开一个连接（ID、仓库名或 Telegram 聊天的名字）
    Rm { id: String },
}

pub fn run(data_dir: &DataDir, command: LinkCommand) -> Result<ExitCode> {
    let (engine, _lock) = open_state(data_dir)?;
    link(&engine, command, &mut std::io::stdout())?;
    Ok(ExitCode::SUCCESS)
}

fn link(engine: &Engine, command: LinkCommand, out: &mut impl Write) -> Result<()> {
    match command {
        LinkCommand::List => {
            let links = engine.list_links()?;
            if links.is_empty() {
                writeln!(
                    out,
                    "还没有连接。用 sa link github 连一个仓库；Telegram 聊天在界面的「连接」页里用配对码来连。"
                )?;
            }
            for item in links {
                show(engine, &item, out)?;
            }
        }
        LinkCommand::Github { room, repo } => {
            let room = find_room(engine, &room)?;
            let item = engine.link_repo(&room.id, &repo)?;
            writeln!(out, "连好了。")?;
            show(engine, &item, out)?;
        }
        LinkCommand::Rm { id } => {
            let links = engine.list_links()?;
            let mut matches: Vec<Link> = links
                .into_iter()
                .filter(|item| {
                    item.id.as_str() == id
                        || item.title == id
                        || item.external.eq_ignore_ascii_case(&id)
                })
                .collect();
            let item = match matches.len() {
                1 => matches.remove(0),
                0 => bail!("没有「{id}」这个连接。用 sa link list 看看有哪些。"),
                n => bail!("有 {n} 个连接都对得上「{id}」，请改用连接的 ID"),
            };
            engine.delete_link(&item.id)?;
            writeln!(out, "断开了「{}」。", item.title)?;
        }
    }
    Ok(())
}

fn show(engine: &Engine, item: &Link, out: &mut impl Write) -> Result<()> {
    let room = engine.get_room(&item.room_id)?;
    let source = match item.source {
        Source::Github => "GitHub  ",
        Source::Telegram => "Telegram",
    };
    writeln!(
        out,
        "{source}  {}  → 房间「{}」  {}",
        item.title, room.name, item.id
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use sa_store::Store;

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        command: LinkCommand,
    }

    fn sa(engine: &Engine, args: &[&str]) -> Result<String> {
        let args = std::iter::once("sa").chain(args.iter().copied());
        let mut out = Vec::new();
        link(engine, Cli::try_parse_from(args)?.command, &mut out)?;
        Ok(String::from_utf8(out).unwrap())
    }

    #[test]
    fn links_are_managed_from_the_terminal() {
        let engine = Engine::new(Store::open_in_memory().unwrap()).unwrap();
        engine.ensure_defaults().unwrap();
        let room = engine.list_rooms().unwrap().remove(0);
        assert!(sa(&engine, &["list"]).unwrap().contains("还没有连接"));

        let shown = sa(&engine, &["github", &room.name, "Me/App"]).unwrap();
        assert!(shown.contains("GitHub"), "{shown}");
        assert!(
            shown.contains(&format!("Me/App  → 房间「{}」", room.name)),
            "{shown}"
        );
        let error = sa(&engine, &["github", &room.name, "app"]).unwrap_err();
        assert!(error.to_string().contains("owner/name"), "{error}");
        let error = sa(&engine, &["github", "没有的房间", "me/app"]).unwrap_err();
        assert!(error.to_string().contains("没有叫"), "{error}");

        let pairing = engine.create_pairing(&room.id).unwrap();
        engine.redeem_pairing(&pairing.code, "42", "张三").unwrap();
        let listed = sa(&engine, &["list"]).unwrap();
        assert_eq!(listed.lines().count(), 2, "{listed}");
        assert!(listed.contains("Telegram  张三"), "{listed}");

        assert!(sa(&engine, &["rm", "me/app"])
            .unwrap()
            .contains("断开了「Me/App」"));
        assert!(sa(&engine, &["rm", "张三"])
            .unwrap()
            .contains("断开了「张三」"));
        assert!(sa(&engine, &["rm", "张三"]).is_err());
        assert!(engine.list_links().unwrap().is_empty());
    }
}
