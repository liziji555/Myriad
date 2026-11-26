//! `sa` 命令行。桌面外壳（`desktop/`）也用这里的启动过程，所以是一个库加一个很薄的可执行文件。

mod app;
mod audit;
mod chat;
mod config;
mod doctor;
mod extensions;
mod link;
mod models;
mod paths;
mod remote;
mod routine;
mod serve;
mod service;
mod share;
mod task;
mod team;
mod update;
mod usage;

pub use app::{init_logging, RunArgs};
pub use paths::DataDir;
pub use remote::{answer_askpass, LoginDenied, SshTarget, Tunnel, TunnelStop};
pub use serve::serve_until;
pub use update::{check_desktop_update, download_desktop_installer, DesktopUpdateInfo};

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "sa", version, about = "Myriad：房间里的 AI 团队")]
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
    /// 打印这台机器上 Myriad 的连接地址（带令牌），给 sa connect 和桌面版用
    Url(remote::UrlArgs),
    /// 通过 SSH 连另一台机器上的 Myriad：开一条隧道，在本机浏览器里打开
    Connect(remote::ConnectArgs),
    /// 把这台电脑上的一个目录共享给 Myriad 服务（比如云端服务器）：那边的 Bot 能看、能搜、能改里面的文件
    Share(share::ShareArgs),
    /// 在 Linux 服务器上让 sa serve 常驻（systemd 用户服务）：开机自启，退出登录也不停
    Service {
        #[command(subcommand)]
        command: service::ServiceCommand,
    },
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
    /// 检查并更新到 GitHub 最新 Release（自动校验 SHA-256 并重启 systemd 服务）
    Update(update::UpdateArgs),
}

/// `sa` 的入口：解析命令行，做完返回退出码。
pub fn run() -> ExitCode {
    answer_askpass();
    let cli = Cli::parse();
    // 没指定数据目录的 `sa url`：服务用的是别的数据目录时，跟着服务走。
    let served = match (&cli.data_dir, &cli.command) {
        (None, Command::Url(_)) => remote::served_data_dir(),
        _ => None,
    };
    let data_dir = cli.data_dir.or(served);
    let result = paths::DataDir::resolve(data_dir).and_then(|data_dir| match cli.command {
        Command::Serve(args) => serve::run(&data_dir, args).map(|()| ExitCode::SUCCESS),
        Command::Url(args) => remote::url(&data_dir, args).map(|()| ExitCode::SUCCESS),
        Command::Connect(args) => remote::connect(args).map(|()| ExitCode::SUCCESS),
        Command::Share(args) => share::run(&data_dir, args).map(|()| ExitCode::SUCCESS),
        Command::Service { command } => {
            service::run(&data_dir, command).map(|()| ExitCode::SUCCESS)
        }
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
        Command::Update(args) => update::run(args).map(|()| ExitCode::SUCCESS),
    });
    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("错误：{err:#}");
            ExitCode::FAILURE
        }
    }
}
