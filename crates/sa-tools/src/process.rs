//! 连同子进程一起结束一个命令。`kill_on_drop` 只管直接启动的那个进程。
//!
//! 用法：启动前调 [`own_group`]，启动后用 [`TreeGuard`] 守着进程号；
//! 要提前结束时调 [`kill_tree`]，命令收了尾就把守卫里的进程号清掉。

use std::process::Stdio;
use tokio::process::Command;

/// 让命令自己一个进程组：结束时能连同它启动的子进程一起杀掉。Windows 上按进程树杀，不用准备。
pub fn own_group(command: &mut Command) {
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(not(unix))]
    let _ = command;
}

/// 调用还没收尾就被丢弃时（比如人点了停止，上层不再等这个工具），把进程树杀掉。
///
/// `kill_on_drop` 只管直接启动的那个进程，它启动的子进程会留下来。
pub struct TreeGuard(pub Option<u32>);

impl Drop for TreeGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0.take() {
            // 在这里等它杀完：调用方（比如房间的写锁）在守卫之后才放手，下一个命令开始时这一个的进程都已经没了。
            // 杀进程很快，短暂占住这个线程可以接受。
            let _ = kill_command(pid).status();
        }
    }
}

/// 结束进程 `pid` 和它启动的所有进程的命令。
fn kill_command(pid: u32) -> std::process::Command {
    let mut killer = if cfg!(windows) {
        let mut c = std::process::Command::new("taskkill");
        c.args(["/T", "/F", "/PID", &pid.to_string()]);
        c
    } else {
        // 负号表示整个进程组。
        let mut c = std::process::Command::new("kill");
        c.args(["-KILL", "--", &format!("-{pid}")]);
        c
    };
    killer
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    killer
}

/// 结束这个进程和它启动的所有进程。
pub async fn kill_tree(pid: Option<u32>) {
    if let Some(pid) = pid {
        let _ = Command::from(kill_command(pid)).status().await;
    }
}
