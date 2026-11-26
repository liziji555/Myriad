//! 后台进程管理工具（`shell_bg` / `shell_output` / `shell_kill`）：
//! 让 Bot 能启动开发服务器、文件监听或长耗时编译任务，随时拉取日志尾部并在不需要时清理整棵进程树。

use crate::process::Tree;
use crate::shell::hidden_env;
use crate::{arg, parse_input, Permission, Tool, ToolCtx, ToolError, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::oneshot;

const MAX_CONCURRENT_JOBS: usize = 16;
const MAX_BUFFER_BYTES: usize = 128 * 1024;
const STARTUP_PEEK: Duration = Duration::from_millis(500);

#[derive(Default)]
struct JobBuffer {
    bytes: Vec<u8>,
    dropped: usize,
    exited: Option<Option<i32>>,
}

impl JobBuffer {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() > MAX_BUFFER_BYTES {
            let excess = self.bytes.len() - MAX_BUFFER_BYTES;
            self.bytes.drain(..excess);
            self.dropped += excess;
        }
    }

    fn tail_text(&self, max_chars: usize) -> String {
        let full = String::from_utf8_lossy(&self.bytes);
        let total = full.chars().count();
        if total <= max_chars {
            full.into_owned()
        } else {
            let tail: String = full.chars().skip(total - max_chars).collect();
            format!("…（前面已省略，仅展示最后 {max_chars} 字）\n{tail}")
        }
    }
}

struct BgJob {
    id: String,
    scope: String,
    command: String,
    buffer: Arc<Mutex<JobBuffer>>,
    killer: Option<oneshot::Sender<()>>,
}

#[derive(Default)]
pub struct BgShellManager {
    next: AtomicU64,
    jobs: Mutex<HashMap<String, BgJob>>,
}

impl BgShellManager {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 结束所有仍在运行的后台进程（服务退出时自动调用）。
    pub fn kill_all(&self) {
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        for job in jobs.values_mut() {
            if let Some(tx) = job.killer.take() {
                let _ = tx.send(());
            }
        }
        jobs.clear();
    }

    /// 结束指定作用域（例如已验收或取消的任务 `task:<id>`）下启动的全部后台进程，释放端口与槽位。
    pub fn kill_scope(&self, scope: &str) {
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        let ids: Vec<String> = jobs
            .values()
            .filter(|j| j.scope == scope)
            .map(|j| j.id.clone())
            .collect();
        for id in ids {
            if let Some(mut job) = jobs.remove(&id) {
                if let Some(tx) = job.killer.take() {
                    let _ = tx.send(());
                }
            }
        }
    }

    fn running_count(jobs: &HashMap<String, BgJob>) -> usize {
        jobs.values()
            .filter(|j| {
                j.buffer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .exited
                    .is_none()
            })
            .count()
    }
}

impl Drop for BgShellManager {
    fn drop(&mut self) {
        self.kill_all();
    }
}

pub struct BgShellStartTool {
    pub manager: Arc<BgShellManager>,
    pub scope: String,
}

#[derive(Deserialize)]
struct StartInput {
    command: String,
}

#[async_trait]
impl Tool for BgShellStartTool {
    fn name(&self) -> &'static str {
        "shell_bg"
    }

    fn description(&self) -> String {
        "在工作目录里以后台任务的方式启动一条长耗时命令（例如开发服务器、watcher、大型编译），\
         立刻返回 job_id 而不会阻塞对话。随后可用 shell_output 查看输出和状态，用 shell_kill 结束整棵进程树。\
         每条命令都要经过用户批准。"
            .into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "要在后台执行的命令" }
            },
            "required": ["command"]
        })
    }

    fn title(&self, input: &Value) -> String {
        let cmd: String = arg(input, "command").chars().take(100).collect();
        format!("后台启动 {cmd}")
    }

    async fn check_permission(&self, input: &Value, _ctx: &ToolCtx) -> Permission {
        Permission::Ask(format!("要在后台启动命令：{}", arg(input, "command")))
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolResult {
        let input: StartInput = parse_input(input)?;
        let cmd_text = input.command.trim().to_string();
        if cmd_text.is_empty() {
            return Err(ToolError::new("命令不能为空。"));
        }
        {
            let jobs = self.manager.jobs.lock().unwrap_or_else(|e| e.into_inner());
            if BgShellManager::running_count(&jobs) >= MAX_CONCURRENT_JOBS {
                return Err(ToolError(format!(
                    "同时运行中的后台命令已达上限（{MAX_CONCURRENT_JOBS} 个），请先用 shell_kill 结束不需要的进程。"
                )));
            }
        }

        let mut command = if cfg!(windows) {
            let mut c = Command::new("powershell");
            c.args(["-NoProfile", "-NonInteractive", "-Command", &cmd_text]);
            c
        } else {
            let mut c = Command::new("bash");
            c.args(["-c", &cmd_text]);
            c
        };
        for name in hidden_env() {
            command.env_remove(name);
        }
        command
            .current_dir(ctx.fence.root())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut tree =
            Tree::spawn(command).map_err(|e| ToolError(format!("后台命令启动不了：{e}")))?;
        let stdout = tree.stdout().expect("bg stdout piped");
        let stderr = tree.stderr().expect("bg stderr piped");
        let id = format!(
            "bg-{}",
            self.manager.next.fetch_add(1, Ordering::Relaxed) + 1
        );
        let buffer = Arc::new(Mutex::new(JobBuffer::default()));
        let (kill_tx, kill_rx) = oneshot::channel::<()>();

        let buf_out = buffer.clone();
        tokio::spawn(drain_stream(stdout, buf_out));
        let buf_err = buffer.clone();
        tokio::spawn(drain_stream(stderr, buf_err));
        let buf_wait = buffer.clone();
        tokio::spawn(async move {
            let exited = tokio::select! {
                status = tree.wait() => {
                    tree.kill();
                    status.ok().and_then(|s| s.code())
                }
                _ = kill_rx => {
                    tree.kill();
                    let _ = tree.wait().await;
                    None
                }
            };
            let mut guard = buf_wait.lock().unwrap_or_else(|e| e.into_inner());
            guard.exited = Some(exited);
        });

        self.manager
            .jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                id.clone(),
                BgJob {
                    id: id.clone(),
                    scope: self.scope.clone(),
                    command: cmd_text.clone(),
                    buffer: buffer.clone(),
                    killer: Some(kill_tx),
                },
            );

        // 稍微等一下，顺便把启动横幅或秒崩报错带给模型。
        tokio::time::sleep(STARTUP_PEEK).await;
        let guard = buffer.lock().unwrap_or_else(|e| e.into_inner());
        let status = match guard.exited {
            None => "运行中".to_string(),
            Some(Some(code)) => format!("已退出（退出码 {code}）"),
            Some(None) => "已结束".to_string(),
        };
        let preview = guard.tail_text(2_000);
        if preview.trim().is_empty() {
            Ok(format!(
                "后台任务已启动：{id}（{status}）。命令：{cmd_text}"
            ))
        } else {
            Ok(format!(
                "后台任务已启动：{id}（{status}）。命令：{cmd_text}\n\n初始输出：\n{preview}"
            ))
        }
    }
}

pub struct BgShellOutputTool {
    pub manager: Arc<BgShellManager>,
    pub scope: String,
}

#[derive(Deserialize)]
struct OutputInput {
    #[serde(default)]
    job_id: Option<String>,
    #[serde(default)]
    tail_chars: Option<usize>,
}

#[async_trait]
impl Tool for BgShellOutputTool {
    fn name(&self) -> &'static str {
        "shell_output"
    }

    fn description(&self) -> String {
        "查看后台命令（shell_bg 启动）的运行状态和最新输出；不传 job_id 时列出全部后台任务。".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "job_id": { "type": "string", "description": "可选：要查看的后台任务 ID（如 bg-1）" },
                "tail_chars": { "type": "integer", "minimum": 100, "maximum": 32000, "description": "最多返回末尾多少字，默认 6000" }
            }
        })
    }

    fn title(&self, input: &Value) -> String {
        let id = arg(input, "job_id");
        if id.is_empty() {
            "查看后台任务列表".into()
        } else {
            format!("查看后台任务 {id}")
        }
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, _ctx: &ToolCtx) -> ToolResult {
        let input: OutputInput = parse_input(input)?;
        let jobs = self.manager.jobs.lock().unwrap_or_else(|e| e.into_inner());
        let Some(job_id) = input
            .job_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            let mut rows: Vec<String> = jobs
                .values()
                .filter(|j| j.scope == self.scope)
                .map(|j| {
                    let guard = j.buffer.lock().unwrap_or_else(|e| e.into_inner());
                    let state = match guard.exited {
                        None => "运行中".to_string(),
                        Some(Some(c)) => format!("已退出({c})"),
                        Some(None) => "已停止".to_string(),
                    };
                    format!("- {} [{}] {}", j.id, state, j.command)
                })
                .collect();
            if rows.is_empty() {
                return Ok("当前没有后台任务。".into());
            }
            rows.sort();
            return Ok(rows.join("\n"));
        };
        let Some(job) = jobs.get(job_id).filter(|j| j.scope == self.scope) else {
            return Err(ToolError(format!("找不到后台任务 {job_id}。")));
        };
        let guard = job.buffer.lock().unwrap_or_else(|e| e.into_inner());
        let state = match guard.exited {
            None => "运行中".to_string(),
            Some(Some(code)) => format!("已退出（退出码 {code}）"),
            Some(None) => "已停止".to_string(),
        };
        let limit = input.tail_chars.unwrap_or(6_000).clamp(100, 32_000);
        let text = guard.tail_text(limit);
        Ok(format!(
            "任务：{}（{state}）\n命令：{}\n\n{}",
            job.id,
            job.command,
            if text.is_empty() {
                "（暂无输出）"
            } else {
                &text
            }
        ))
    }
}

pub struct BgShellKillTool {
    pub manager: Arc<BgShellManager>,
    pub scope: String,
}

#[derive(Deserialize)]
struct KillInput {
    job_id: String,
}

#[async_trait]
impl Tool for BgShellKillTool {
    fn name(&self) -> &'static str {
        "shell_kill"
    }

    fn description(&self) -> String {
        "结束由 shell_bg 启动的后台任务及其全部子进程。".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "job_id": { "type": "string", "description": "要停止的后台任务 ID（如 bg-1）" }
            },
            "required": ["job_id"]
        })
    }

    fn title(&self, input: &Value) -> String {
        format!("停止后台任务 {}", arg(input, "job_id"))
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        // 清理已批准启动的后台进程不需要二次审批。
        true
    }

    async fn call(&self, input: Value, _ctx: &ToolCtx) -> ToolResult {
        let input: KillInput = parse_input(input)?;
        let id = input.job_id.trim();
        let mut jobs = self.manager.jobs.lock().unwrap_or_else(|e| e.into_inner());
        if !jobs.get(id).is_some_and(|j| j.scope == self.scope) {
            return Err(ToolError(format!("找不到后台任务 {id}。")));
        }
        let job = jobs
            .remove(id)
            .ok_or_else(|| ToolError(format!("找不到后台任务 {id}。")))?;
        drop(jobs);
        if let Some(tx) = job.killer {
            let _ = tx.send(());
        }
        Ok(format!("已停止后台任务 {}（{}）。", job.id, job.command))
    }
}

async fn drain_stream(mut reader: impl AsyncRead + Unpin, buffer: Arc<Mutex<JobBuffer>>) {
    let mut chunk = [0u8; 4096];
    while let Ok(n) = reader.read(&mut chunk).await {
        if n == 0 {
            break;
        }
        let mut guard = buffer.lock().unwrap_or_else(|e| e.into_inner());
        guard.push(&chunk[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Fence;

    #[tokio::test]
    async fn starts_reads_and_kills_background_shell_job() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolCtx::new(Fence::new(dir.path(), &[]).unwrap());
        let manager = BgShellManager::new();
        let start = BgShellStartTool {
            manager: manager.clone(),
            scope: "room-1".into(),
        };
        let out = BgShellOutputTool {
            manager: manager.clone(),
            scope: "room-1".into(),
        };
        let kill = BgShellKillTool {
            manager: manager.clone(),
            scope: "room-1".into(),
        };

        let cmd = if cfg!(windows) {
            "Write-Output 'BG_READY'; Start-Sleep -Seconds 30"
        } else {
            "echo BG_READY; sleep 30"
        };
        let started = start.call(json!({"command": cmd}), &ctx).await.unwrap();
        assert!(started.contains("bg-1"), "{started}");

        let status = out.call(json!({"job_id": "bg-1"}), &ctx).await.unwrap();
        assert!(status.contains("BG_READY"), "{status}");

        let killed = kill.call(json!({"job_id": "bg-1"}), &ctx).await.unwrap();
        assert!(killed.contains("已停止后台任务 bg-1"), "{killed}");
    }
}
