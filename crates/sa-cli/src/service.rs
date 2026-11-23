//! `sa service`：在 Linux 服务器上把 `sa serve` 装成 systemd 用户服务。
//! 开机自启、崩了自动拉起，退出 SSH 登录也不停。

use crate::paths::DataDir;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

const UNIT: &str = "superagent.service";

#[derive(clap::Subcommand)]
pub enum ServiceCommand {
    /// 装好并启动（已经装过的话照新参数重装）
    Install {
        /// Bot 能读写的工作目录（默认当前目录）
        #[arg(long, value_name = "目录")]
        workdir: Option<PathBuf>,
        /// 用不联网的假模型（用来试流程）
        #[arg(long)]
        mock: bool,
    },
    /// 停下并卸掉
    Uninstall,
}

pub fn run(data_dir: &DataDir, command: ServiceCommand) -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!("sa service 只支持 Linux（systemd）。别的系统上请用系统自己的方式让 sa serve 常驻");
    }
    let unit_path = unit_dir()?.join(UNIT);
    match command {
        ServiceCommand::Install { workdir, mock } => {
            let workdir = match workdir {
                Some(dir) => dir,
                None => std::env::current_dir().context("拿不到当前目录，请用 --workdir 指定")?,
            };
            let workdir = workdir
                .canonicalize()
                .with_context(|| format!("工作目录 {} 打不开", workdir.display()))?;
            data_dir
                .create()
                .with_context(|| format!("无法创建数据目录 {}", data_dir.root().display()))?;
            let data_root = data_dir.root().canonicalize()?;
            let exe = std::env::current_exe().context("找不到 sa 自己在哪")?;
            let unit = render_unit(&exe, &data_root, &workdir, mock);
            std::fs::create_dir_all(unit_path.parent().unwrap())?;
            std::fs::write(&unit_path, unit)
                .with_context(|| format!("写不进 {}", unit_path.display()))?;
            systemctl(&["daemon-reload"])?;
            systemctl(&["enable", UNIT])?;
            systemctl(&["restart", UNIT])?;
            linger();
            println!("已装好并启动：{}", unit_path.display());
            println!("工作目录：{}", workdir.display());
            println!(
                "模型密钥写进 {}（每行一个 名字=值），改了之后 systemctl --user restart superagent。",
                env_file(&data_root).display()
            );
            println!("看日志：journalctl --user -u superagent -f");
            println!("连接地址：sa url；在自己电脑上用 sa connect 或桌面版连过来。");
        }
        ServiceCommand::Uninstall => {
            if !unit_path.exists() {
                println!("没有装过。");
                return Ok(());
            }
            systemctl(&["disable", "--now", UNIT])?;
            std::fs::remove_file(&unit_path)?;
            systemctl(&["daemon-reload"])?;
            println!("已停下并卸掉。数据目录没动。");
        }
    }
    Ok(())
}

fn unit_dir() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("找不到配置目录（~/.config）")?
        .join("systemd")
        .join("user"))
}

fn env_file(data_root: &Path) -> PathBuf {
    data_root.join("env")
}

fn render_unit(exe: &Path, data_root: &Path, workdir: &Path, mock: bool) -> String {
    let mut exec = vec![
        quote(&exe.to_string_lossy()),
        "--data-dir".into(),
        quote(&data_root.to_string_lossy()),
        "serve".into(),
        "--workdir".into(),
        quote(&workdir.to_string_lossy()),
    ];
    if mock {
        exec.push("--mock".into());
    }
    format!(
        "# 由 sa service install 生成。重新运行它会覆盖这个文件。\n\
         [Unit]\n\
         Description=苦乐（sa serve）\n\
         Wants=network-online.target\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={}\n\
         EnvironmentFile=-{}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exec.join(" "),
        env_file(data_root).to_string_lossy().replace('%', "%%"),
    )
}

/// systemd 的引号：整段用双引号包起来，`\` 和 `"` 转义，`%`、`$` 写两遍免得被展开。
fn quote(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn systemctl(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("找不到 systemctl：这台机器没有 systemd")?;
    if !status.success() {
        bail!("systemctl --user {} 失败", args.join(" "));
    }
    Ok(())
}

/// 让用户服务在没登录时也跑着。不成功只提醒，不算装失败。
fn linger() {
    let user = std::env::var("USER").unwrap_or_default();
    let ok = Command::new("loginctl")
        .args(["enable-linger", &user])
        .status()
        .is_ok_and(|status| status.success());
    if !ok {
        eprintln!(
            "提示：没能打开 linger，退出 SSH 登录后服务可能会停。请运行 sudo loginctl enable-linger {user}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // systemd 只在 Linux 上有；Windows 上拼出来的路径是反斜杠。
    #[cfg(unix)]
    #[test]
    fn the_unit_runs_sa_serve_with_quoted_paths() {
        let unit = render_unit(
            Path::new("/opt/sa/bin/sa"),
            Path::new("/home/me/.local/share/superagent"),
            Path::new("/srv/我的 项目 50%$HOME"),
            true,
        );
        assert!(unit.contains(
            "ExecStart=\"/opt/sa/bin/sa\" --data-dir \"/home/me/.local/share/superagent\" \
             serve --workdir \"/srv/我的 项目 50%%$$HOME\" --mock\n"
        ));
        assert!(unit.contains("EnvironmentFile=-/home/me/.local/share/superagent/env\n"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn quotes_and_backslashes_are_escaped() {
        assert_eq!(quote(r#"a"b\c"#), r#""a\"b\\c""#);
    }
}
