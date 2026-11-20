mod config;
mod doctor;
mod paths;
mod serve;

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
    /// 检查配置、密钥、数据库、端口是否正常
    Doctor,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = paths::DataDir::resolve(cli.data_dir).and_then(|data_dir| match cli.command {
        Command::Serve(args) => serve::run(&data_dir, args).map(|()| ExitCode::SUCCESS),
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
