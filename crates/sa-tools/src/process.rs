//! 连同子进程一起结束一个命令。`kill_on_drop` 只管直接启动的那个进程。
//!
//! 用 [`Tree::spawn`] 启动：Unix 上命令自己一个进程组，Windows 上放进一个作业对象，
//! 它启动的子进程、孙进程都跟着进去。要提前结束时调 [`Tree::kill`]，命令收了尾就调 [`Tree::disarm`]；
//! 两样都没做就被丢弃时（比如人点了停止，上层不再等这个工具），整棵进程树一起结束。

use process_wrap::tokio::{ChildWrapper, CommandWrap};
use std::process::{ExitStatus, Output};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};

/// 一个命令和它启动的所有进程。
#[derive(Debug)]
pub struct Tree {
    child: Box<dyn ChildWrapper>,
    armed: bool,
}

impl Tree {
    pub fn spawn(command: Command) -> std::io::Result<Self> {
        let mut command = CommandWrap::from(command);
        #[cfg(unix)]
        command.wrap(process_wrap::tokio::ProcessGroup::leader());
        // 作业对象：Windows 上父进程退出后按进程树找不到它的孙进程，放进同一个作业里就一个都跑不掉。
        #[cfg(windows)]
        command.wrap(process_wrap::tokio::JobObject);
        Ok(Self {
            child: command.spawn()?,
            armed: true,
        })
    }

    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    pub fn stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin().take()
    }

    pub fn stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout().take()
    }

    pub fn stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr().take()
    }

    /// 等直接启动的那个进程退出。它开出来的后台进程还在不在不管：要收干净就再调 [`Tree::kill`]。
    pub async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        self.child.inner_mut().wait().await
    }

    /// 读完输出、等它退出。
    pub async fn output(&mut self) -> std::io::Result<Output> {
        async fn read_all(stream: Option<impl AsyncRead + Unpin>) -> std::io::Result<Vec<u8>> {
            let mut bytes = Vec::new();
            if let Some(mut stream) = stream {
                stream.read_to_end(&mut bytes).await?;
            }
            Ok(bytes)
        }
        let (stdout, stderr) = (self.stdout(), self.stderr());
        let (status, stdout, stderr) =
            tokio::try_join!(self.wait(), read_all(stdout), read_all(stderr))?;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }

    /// 结束这个进程和它启动的所有进程，已经退出的也一样。
    pub fn kill(&mut self) {
        // 进程都已经没了时会报错：要的就是它们没了。
        let _ = self.child.start_kill();
        self.armed = false;
    }

    /// 命令收了尾：丢弃时不再结束它留下的后台进程。
    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        // 当场结束：调用方（比如房间的写锁）在它之后才放手，下一个命令开始时这一个的进程都已经没了。
        if self.armed {
            let _ = self.child.start_kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::time::Duration;

    /// 命令自己退出了，它开的后台进程还占着输出：结束整棵树后输出马上读到头。
    #[tokio::test]
    async fn kill_after_exit_takes_the_background_process_too() {
        let mut command = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", "start /B ping -n 60 127.0.0.1"]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", "sleep 60 &"]);
            c
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut tree = Tree::spawn(command).unwrap();
        let mut stdout = tree.stdout().unwrap();
        let status = tokio::time::timeout(Duration::from_secs(20), tree.wait())
            .await
            .expect("命令没有启动起来")
            .unwrap();
        assert!(status.success());

        tree.kill();
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(20), stdout.read_to_end(&mut rest))
            .await
            .expect("后台进程还占着输出")
            .unwrap();
    }
}
