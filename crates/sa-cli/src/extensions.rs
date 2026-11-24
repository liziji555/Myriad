//! `sa skill` 和 `sa mcp`：看有哪些技能，试外部 MCP 服务能不能连上。
//!
//! 这两个命令只读配置和技能目录，不动数据库，`sa serve` 开着的时候也能用。

use crate::config::Config;
use crate::paths::DataDir;
use anyhow::{bail, Result};
use sa_tools::mcp::{run_demo_server, McpServer};
use sa_tools::SkillSet;
use std::io::Write;
use std::process::ExitCode;

#[derive(clap::Subcommand)]
pub enum SkillCommand {
    /// 列出技能目录里的技能
    List,
}

#[derive(clap::Subcommand)]
pub enum McpCommand {
    /// 列出配置里的 MCP 服务
    List,
    /// 连一下试试，列出它提供的工具。不写名字就试全部启用着的
    Test { name: Option<String> },
    /// 一个最小的 MCP 服务，试用和测试用：echo、add 等几个工具
    #[command(hide = true)]
    DemoServer,
}

pub fn skill(data_dir: &DataDir, command: SkillCommand) -> Result<ExitCode> {
    let SkillCommand::List = command;
    list_skills(data_dir, &mut std::io::stdout())?;
    Ok(ExitCode::SUCCESS)
}

fn list_skills(data_dir: &DataDir, out: &mut impl Write) -> Result<()> {
    let dir = data_dir.skills();
    let set = SkillSet::load(&dir);
    if set.skills.is_empty() {
        writeln!(
            out,
            "还没有技能。在 {} 里放 <名字>/SKILL.md 或者 <名字>.md，下一轮对话就能用。",
            dir.display()
        )?;
    }
    for skill in &set.skills {
        let who = if skill.bots.is_empty() {
            "所有 Bot".to_string()
        } else {
            skill.bots.join("、")
        };
        writeln!(out, "{}  {}  （{who}）", skill.name, skill.description)?;
    }
    for problem in &set.problems {
        writeln!(out, "! 没认出来：{problem}")?;
    }
    Ok(())
}

pub fn mcp(data_dir: &DataDir, command: McpCommand) -> Result<ExitCode> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    if let McpCommand::DemoServer = command {
        runtime.block_on(run_demo_server(tokio::io::stdin(), tokio::io::stdout()))?;
        return Ok(ExitCode::SUCCESS);
    }
    let config = Config::load_or_default(&data_dir.config())?;
    let mut out = std::io::stdout();
    match command {
        McpCommand::List => {
            if config.mcp.is_empty() {
                writeln!(
                    out,
                    "还没有配置 MCP 服务。在 {} 里加一节 [mcp.<名字>]，里面有写法的例子。",
                    data_dir.config().display()
                )?;
            }
            for (name, server) in &config.mcp {
                let state = if server.enabled { "启用" } else { "停用" };
                let command = match &server.url {
                    Some(url) => url.clone(),
                    None => [server.command.clone()]
                        .into_iter()
                        .chain(server.args.iter().cloned())
                        .collect::<Vec<_>>()
                        .join(" "),
                };
                writeln!(out, "[{state}]  {name}  {command}")?;
            }
            Ok(ExitCode::SUCCESS)
        }
        McpCommand::Test { name } => {
            let cwd = std::env::current_dir()?;
            let env = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
            let mut specs = config.mcp_specs(&cwd, env);
            if let Some(name) = &name {
                if !config.mcp.contains_key(name) {
                    bail!("配置里没有叫「{name}」的 MCP 服务。用 sa mcp list 看看有哪些。");
                }
                specs.retain(|spec| &spec.name == name);
                if specs.is_empty() {
                    bail!("「{name}」在配置里是停用的（enabled = false）。");
                }
            }
            if specs.is_empty() {
                writeln!(out, "没有启用着的 MCP 服务。")?;
            }
            let mut failed = false;
            for spec in specs {
                let name = spec.name.clone();
                let server = McpServer::new(spec);
                match runtime.block_on(server.connect()) {
                    Ok(tools) => {
                        writeln!(out, "✓ {name}：连上了，{} 个工具", tools.len())?;
                        for tool in tools {
                            let about: String = tool.description.chars().take(60).collect();
                            writeln!(out, "    {}  {}", tool.name, about.replace('\n', " "))?;
                        }
                    }
                    Err(error) => {
                        failed = true;
                        writeln!(out, "✗ {name}：{error}")?;
                    }
                }
                runtime.block_on(server.close());
            }
            Ok(if failed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            })
        }
        McpCommand::DemoServer => unreachable!("上面已经处理了"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skills_are_listed_with_who_may_use_them_and_what_went_wrong() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = DataDir::resolve(Some(dir.path().to_path_buf())).unwrap();
        let mut out = vec![];
        list_skills(&data_dir, &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("还没有技能"));

        let skills = data_dir.skills();
        std::fs::create_dir_all(skills.join("release")).unwrap();
        std::fs::write(
            skills.join("release/SKILL.md"),
            "---\nname: 发版\ndescription: 发版前的检查\nbots: Coder\n---\n正文",
        )
        .unwrap();
        std::fs::write(skills.join("review.md"), "# 代码评审\n").unwrap();
        std::fs::write(skills.join("bad name.md"), "正文").unwrap();
        let mut out = vec![];
        list_skills(&data_dir, &mut out).unwrap();
        let shown = String::from_utf8(out).unwrap();
        assert!(shown.contains("review  代码评审  （所有 Bot）"), "{shown}");
        assert!(shown.contains("发版  发版前的检查  （Coder）"), "{shown}");
        assert!(shown.contains("! 没认出来：bad name.md"), "{shown}");
    }
}
