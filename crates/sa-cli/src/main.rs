mod app;
mod chat;
mod config;
mod doctor;
mod models;
mod paths;
mod serve;
mod task;
mod team;

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
    /// 任务：列出、开任务、继续、验收、打回、取消（不能和 sa serve 同时运行）
    Task(task::TaskArgs),
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
        Command::Task(args) => task::run(&data_dir, args),
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
