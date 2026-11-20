use crate::{arg, parse_input, Permission, Tool, ToolCtx, ToolError, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 600;
/// 输出最多留这么多字节，超出的丢掉中间。
const MAX_OUTPUT_BYTES: usize = 60_000;

pub struct ShellTool;

#[derive(Deserialize)]
struct Input {
    command: String,
    timeout_secs: Option<u64>,
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &'static str {
        "shell"
    }

    fn description(&self) -> String {
        let shell = if cfg!(windows) { "PowerShell" } else { "bash" };
        format!(
            "在工作目录里用 {shell} 执行一条命令，返回退出码和输出（标准输出和错误输出合在一起）。\
             默认最多跑 {DEFAULT_TIMEOUT_SECS} 秒，可以用 timeout_secs 调整，上限 {MAX_TIMEOUT_SECS} 秒。\
             每条命令都要用户批准后才执行。不要用它读文件或搜索，那些有专门的工具。"
        )
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "要执行的命令" },
                "timeout_secs": { "type": "integer", "minimum": 1, "description": "最多跑多少秒" }
            },
            "required": ["command"]
        })
    }

    fn title(&self, input: &Value) -> String {
        let command: String = arg(input, "command").chars().take(120).collect();
        format!("执行 {command}")
    }

    async fn check_permission(&self, input: &Value, _ctx: &ToolCtx) -> Permission {
        Permission::Ask(format!("要执行命令：{}", arg(input, "command")))
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolResult {
        let input: Input = parse_input(input)?;
        if input.command.trim().is_empty() {
            return Err(ToolError::new("命令是空的。"));
        }
        let limit = Duration::from_secs(
            input
                .timeout_secs
                .unwrap_or(DEFAULT_TIMEOUT_SECS)
                .clamp(1, MAX_TIMEOUT_SECS),
        );

        let mut command = if cfg!(windows) {
            let mut c = Command::new("powershell");
            c.args(["-NoProfile", "-NonInteractive", "-Command", &input.command]);
            c
        } else {
            let mut c = Command::new("bash");
            c.args(["-c", &input.command]);
            c
        };
        command
            .current_dir(ctx.fence.root())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // 自己一个进程组：停止时连同它启动的子进程一起杀掉。
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|e| ToolError(format!("命令启动不了：{e}")))?;
        let pid = child.id();
        let stdout = child.stdout.take().expect("上面设置了管道");
        let stderr = child.stderr.take().expect("上面设置了管道");
        let output = async {
            let (out, err) = tokio::join!(read_capped(stdout), read_capped(stderr));
            (out, err)
        };

        enum End {
            Exited(std::io::Result<std::process::ExitStatus>),
            TimedOut,
            Stopped,
        }
        let run = async {
            let (streams, status) = tokio::join!(output, child.wait());
            (streams, status)
        };
        tokio::pin!(run);
        let (end, streams) = tokio::select! {
            (streams, status) = &mut run => (End::Exited(status), Some(streams)),
            _ = tokio::time::sleep(limit) => (End::TimedOut, None),
            _ = ctx.cancel.cancelled() => (End::Stopped, None),
        };
        if streams.is_none() {
            kill_tree(pid).await;
        }
        match end {
            End::Stopped => Err(ToolError::new("已停止，命令和它启动的进程都已结束。")),
            End::TimedOut => Err(ToolError(format!(
                "命令跑了 {} 秒还没结束，已经连同它启动的进程一起结束掉。需要更久的话用 timeout_secs。",
                limit.as_secs()
            ))),
            End::Exited(Err(e)) => Err(ToolError(format!("等不到命令结束：{e}"))),
            End::Exited(Ok(status)) => {
                let (out, err) = streams.expect("正常结束时一定有输出");
                let mut text = String::from_utf8_lossy(&out).into_owned();
                if !err.is_empty() {
                    if !text.is_empty() && !text.ends_with('\n') {
                        text.push('\n');
                    }
                    text.push_str(&String::from_utf8_lossy(&err));
                }
                let text = text.trim_end();
                let code = status
                    .code()
                    .map_or("被信号终止".to_string(), |code| format!("退出码 {code}"));
                Ok(if text.is_empty() {
                    format!("{code}（没有输出）")
                } else {
                    format!("{code}\n{text}")
                })
            }
        }
    }
}

/// 读到结束，但只留开头和结尾各一半，中间超出的部分丢掉，内存不会跟着输出一直涨。
async fn read_capped(mut stream: impl AsyncRead + Unpin) -> Vec<u8> {
    let half = MAX_OUTPUT_BYTES / 2;
    let mut head = Vec::new();
    let mut tail = std::collections::VecDeque::new();
    let mut dropped = 0usize;
    let mut buffer = [0u8; 8192];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                for &byte in &buffer[..n] {
                    if head.len() < half {
                        head.push(byte);
                    } else {
                        if tail.len() == half {
                            tail.pop_front();
                            dropped += 1;
                        }
                        tail.push_back(byte);
                    }
                }
            }
        }
    }
    if dropped > 0 {
        head.extend_from_slice(format!("\n…（中间省略了 {dropped} 字节的输出）…\n").as_bytes());
    }
    head.extend(tail);
    head
}

/// 结束这个进程和它启动的所有进程。
async fn kill_tree(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    let mut killer = if cfg!(windows) {
        let mut c = Command::new("taskkill");
        c.args(["/T", "/F", "/PID", &pid.to_string()]);
        c
    } else {
        // 负号表示整个进程组。
        let mut c = Command::new("kill");
        c.args(["-KILL", "--", &format!("-{pid}")]);
        c
    };
    let _ = killer
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Fence;

    fn rig() -> (tempfile::TempDir, ToolCtx) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolCtx::new(Fence::new(dir.path(), &[]).unwrap());
        (dir, ctx)
    }

    #[tokio::test]
    async fn always_asks_first_and_shows_the_command() {
        let (_dir, ctx) = rig();
        let input = json!({"command": "echo hi"});
        assert_eq!(
            ShellTool.check_permission(&input, &ctx).await,
            Permission::Ask("要执行命令：echo hi".into())
        );
        assert_eq!(ShellTool.title(&input), "执行 echo hi");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runs_in_the_workdir_and_reports_exit_code_and_both_streams() {
        let (dir, ctx) = rig();
        std::fs::write(dir.path().join("标记.txt"), "").unwrap();
        let run = |command: &str| {
            let ctx = ctx.clone();
            let input = json!({"command": command});
            async move { ShellTool.call(input, &ctx).await }
        };
        assert_eq!(run("ls").await.unwrap(), "退出码 0\n标记.txt");
        assert_eq!(run("true").await.unwrap(), "退出码 0（没有输出）");
        let failed = run("echo 出错了 >&2; echo 正常; exit 3").await.unwrap();
        assert!(failed.starts_with("退出码 3\n"), "{failed}");
        assert!(failed.contains("正常") && failed.contains("出错了"));
        assert!(run("  ").await.unwrap_err().0.contains("空的"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn huge_output_keeps_head_and_tail() {
        let (_dir, ctx) = rig();
        let out = ShellTool
            .call(
                json!({"command": "echo 开头; yes x | head -c 400000; echo 结尾"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.len() < MAX_OUTPUT_BYTES + 200, "{}", out.len());
        assert!(out.contains("开头") && out.trim_end().ends_with("结尾"));
        assert!(out.contains("中间省略了"));
    }

    /// 命令自己又启动了后台进程：超时和停止都要把整棵树结束掉。
    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_and_stop_kill_the_whole_process_tree() {
        let (dir, ctx) = rig();
        let alive = |file: &str| {
            let pid = std::fs::read_to_string(dir.path().join(file)).unwrap();
            std::path::Path::new(&format!("/proc/{}", pid.trim())).exists()
        };
        let spawn_child = |file: &str| format!("sleep 300 & echo $! > {file}; wait");

        let error = ShellTool
            .call(
                json!({"command": spawn_child("a.pid"), "timeout_secs": 1}),
                &ctx,
            )
            .await
            .unwrap_err()
            .0;
        assert!(error.contains("1 秒还没结束"), "{error}");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!alive("a.pid"), "超时后子进程还活着");

        let stopper = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            stopper.cancel();
        });
        let error = ShellTool
            .call(json!({"command": spawn_child("b.pid")}), &ctx)
            .await
            .unwrap_err()
            .0;
        assert!(error.contains("已停止"), "{error}");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!alive("b.pid"), "停止后子进程还活着");
    }
}
