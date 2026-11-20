mod app;
mod audit;
mod chat;
mod config;
mod doctor;
mod extensions;
mod link;
mod models;
mod paths;
mod routine;
mod serve;
mod task;
mod team;
mod usage;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "sa", version, about = "SuperAgent：房间里的 AI 团队")]
struct Cli {
    /// 数据目录（配置、数据库、会话记录都在这里）
    #[arg(long, global = true, env = "SA_DATA_DIR", value_name = "目录")]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 启动本机服务，在浏览器里打开界面
    Serve(serve::ServeArgs),
    /// 在终端里和 Bot 对话（和 sa serve 用同一份数据，不能同时运行）
    Chat(chat::ChatArgs),
    /// 管理 Bot：列出、新建、修改、删除
    Bot {
        #[command(subcommand)]
        command: team::BotCommand,
    },
    /// 管理房间：列出、新建、拉 Bot 进来、请出去、换入口 Bot
    Room {
        #[command(subcommand)]
        command: team::RoomCommand,
    },
    /// 定时任务：列出、新建、删除、启用、停用（不能和 sa serve 同时运行；到点之后由 sa serve 来做）
    Routine {
        #[command(subcommand)]
        command: routine::RoutineCommand,
    },
    /// 和外面的连接：列出、把 GitHub 仓库连到房间、断开（不能和 sa serve 同时运行）
    Link {
        #[command(subcommand)]
        command: link::LinkCommand,
    },
    /// 技能：列出技能目录里的技能
    Skill {
        #[command(subcommand)]
        command: extensions::SkillCommand,
    },
    /// 外部 MCP 服务：列出配置里的、连一下试试
    Mcp {
        #[command(subcommand)]
        command: extensions::McpCommand,
    },
    /// 任务：列出、开任务、继续、验收、打回、取消（不能和 sa serve 同时运行）
    Task(task::TaskArgs),
    /// 看今天和最近几天花了多少（不能和 sa serve 同时运行）
    Usage,
    /// 审计日志：谁在什么时候批准了什么、执行了什么、改了什么（不能和 sa serve 同时运行）
    Audit(audit::AuditArgs),
    /// 检查配置、密钥、数据库、端口是否正常
    Doctor,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = paths::DataDir::resolve(cli.data_dir).and_then(|data_dir| match cli.command {
        Command::Serve(args) => serve::run(&data_dir, args).map(|()| ExitCode::SUCCESS),
        Command::Chat(args) => chat::run(&data_dir, args),
        Command::Bot { command } => team::run(&data_dir, team::Command::Bot(command)),
        Command::Room { command } => team::run(&data_dir, team::Command::Room(command)),
        Command::Routine { command } => routine::run(&data_dir, command),
        Command::Link { command } => link::run(&data_dir, command),
        Command::Skill { command } => extensions::skill(&data_dir, command),
        Command::Mcp { command } => extensions::mcp(&data_dir, command),
        Command::Task(args) => task::run(&data_dir, args),
        Command::Usage => usage::run(&data_dir),
        Command::Audit(args) => audit::run(&data_dir, args),
        Command::Doctor => Ok(doctor::run(&data_dir)),
    });
    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("错误：{err:#}");
            ExitCode::FAILURE
        }
    }
}
