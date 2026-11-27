//! Claude Code 双向流式持久会话驱动（`--input-format stream-json --output-format stream-json`）
//! 与运行中实时插话支持。
//!
//! 相比单次冷启动的 `claude -p --output-format stream-json`，双向持久模式通过常驻进程与 NDJSON
//! 控制通道（`control_request` / `control_response` / `--replay-user-messages`）实现：
//! - 持久进程池（`ClaudePool`）：同工位跨轮复用常驻 Claude Code 进程与内存上下文，免去每轮冷启动；
//! - 实时运行中插话：在 Claude Code 干活中途将新消息以 `type: "user"` NDJSON 写入 stdin，
//!   以 `--replay-user-messages` 回显的 `uuid` 作为精确送达确认，且兼容轮次收尾排队续跑场景；
//! - 双向工具权限桥接（`--permission-prompt-tool stdio`）：将 `can_use_tool` 控制请求桥接到 Myriad 审批卡
//!   （`Ask` 先建步再弹窗、`Plan` 自动拒绝、`Full` 自动放行，且使用隔离工具名避免跨目录误记「总是允许」）；
//! - 优雅中断（`control_request` `interrupt`）与低版本/不兼容命令自动回退（`initialize` 握手失败时无感回退单次 `claude -p`）。

use crate::harness::{
    build_command, find_program, mcp_fingerprint, parse_claude, read_bounded_line_buffered,
    read_stderr_tail, split_command, str_of, tail, write_claude_mcp_config, Event, Finished,
    Launch, LineBuffer, SteerRequest, TempFileGuard, IDLE_TIMEOUT, STDERR_TAIL,
};
use sa_kernel::{ApprovalRequest, Observer, StepEnd, TurnEnd};
use sa_protocol::PermissionMode;
use sa_tools::process::Tree;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// 持久 Claude Code 进程池容量上限（超出时按最久未使用淘汰）。
const CLAUDE_POOL_MAX: usize = 16;
/// 持久 Claude Code 进程空闲超时时间。
const CLAUDE_IDLE_TTL: Duration = Duration::from_secs(15 * 60);
/// `initialize` 控制握手超时（超时视作不兼容或旧版 CLI，安全回退单次 `claude -p`）。
const INIT_TIMEOUT: Duration = Duration::from_secs(15);
/// 点「停止」时等待 `interrupt` 收尾的最长时间。
const INTERRUPT_GRACE: Duration = Duration::from_millis(800);

fn next_uuid() -> String {
    // 每次生成随机 UUIDv4，避免服务重启后 PID/计数器归零撞上 `--resume` 会话历史里的旧消息 UUID。
    uuid::Uuid::new_v4().to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ClaudeKey {
    scope: String,
    command: String,
    dir: PathBuf,
}

struct PooledClaude {
    child: Tree,
    stdin: tokio::process::ChildStdin,
    reader: BufReader<tokio::process::ChildStdout>,
    line_buf: LineBuffer,
    stderr: Option<tokio::task::JoinHandle<String>>,
    /// 临时 `--mcp-config` 文件守卫：随常驻进程存活，淘汰或关闭时自动删除。
    _mcp_temp: TempFileGuard,
    session_id: Option<String>,
    next_req_id: u64,
    mcp_fingerprint: String,
    permission: PermissionMode,
    /// 当前常驻进程是否支持 `--replay-user-messages` 回显 `uuid`（决定是否允许中途插话注入）。
    replay_supported: bool,
    last_used: Instant,
}

impl Drop for PooledClaude {
    fn drop(&mut self) {
        self.child.kill();
        if let Some(handle) = self.stderr.take() {
            handle.abort();
        }
    }
}

/// 持久 Claude Code 流式进程池：同一房间工位的 Claude Code Bot 跨轮保持常驻进程与会话。
#[derive(Default)]
pub(crate) struct ClaudePool {
    entries: Arc<Mutex<HashMap<ClaudeKey, PooledClaude>>>,
}

impl ClaudePool {
    fn take(
        &self,
        key: &ClaudeKey,
        expected_session: Option<&str>,
        mcp_fp: &str,
        permission: PermissionMode,
    ) -> Option<PooledClaude> {
        let mut stale = Vec::new();
        let taken = {
            let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            let expired_keys: Vec<ClaudeKey> = map
                .iter()
                .filter(|(_, v)| v.last_used.elapsed() > CLAUDE_IDLE_TTL)
                .map(|(k, _)| k.clone())
                .collect();
            for k in expired_keys {
                if let Some(v) = map.remove(&k) {
                    stale.push(v);
                }
            }
            map.remove(key)
        };
        drop(stale);
        let mut conn = taken?;
        let valid = !conn.child.has_exited()
            && conn.mcp_fingerprint == mcp_fp
            && conn.permission == permission
            && expected_session.is_some()
            && expected_session == conn.session_id.as_deref();
        if valid {
            Some(conn)
        } else {
            drop(conn);
            None
        }
    }

    fn put(&self, key: ClaudeKey, conn: PooledClaude) {
        self.put_with_ttl(key, conn, CLAUDE_IDLE_TTL);
    }

    fn put_with_ttl(&self, key: ClaudeKey, mut conn: PooledClaude, ttl: Duration) {
        let now = Instant::now();
        conn.last_used = now;
        let mut evicted = Vec::new();
        {
            let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            let expired_keys: Vec<ClaudeKey> = map
                .iter()
                .filter(|(_, v)| v.last_used.elapsed() > ttl)
                .map(|(k, _)| k.clone())
                .collect();
            for k in expired_keys {
                if let Some(v) = map.remove(&k) {
                    evicted.push(v);
                }
            }
            while map.len() >= CLAUDE_POOL_MAX {
                if let Some(oldest) = map
                    .iter()
                    .min_by_key(|(_, v)| v.last_used)
                    .map(|(k, _)| k.clone())
                {
                    if let Some(v) = map.remove(&oldest) {
                        evicted.push(v);
                    }
                } else {
                    break;
                }
            }
            if let Some(prev) = map.insert(key.clone(), conn) {
                evicted.push(prev);
            }
        }
        drop(evicted);

        // 主动按空闲截止时间安排回收定时器：复核 `last_used == now` 防止旧定时器误杀新连接。
        let weak = Arc::downgrade(&self.entries);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                tokio::time::sleep(ttl).await;
                let Some(entries) = weak.upgrade() else {
                    return;
                };
                let expired = {
                    let mut map = entries.lock().unwrap_or_else(|e| e.into_inner());
                    if map.get(&key).is_some_and(|v| v.last_used == now) {
                        map.remove(&key).map(|mut conn| {
                            // 释放锁前立即向进程树发终止信号，避免并发 evict_scope 提前返回。
                            conn.child.kill();
                            conn
                        })
                    } else {
                        None
                    }
                };
                drop(expired);
            });
        }
    }

    /// 驱逐指定作用域（重置工位会话、关闭工位）的全部常驻 Claude Code 进程。
    pub(crate) fn evict_scope(&self, scope: &str) {
        let mut evicted = Vec::new();
        {
            let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            let keys: Vec<ClaudeKey> = map.keys().filter(|k| k.scope == scope).cloned().collect();
            for k in keys {
                if let Some(v) = map.remove(&k) {
                    evicted.push(v);
                }
            }
        }
        drop(evicted);
    }

    /// 驱逐所有常驻 Claude Code 进程（服务关闭或 MCP 热重载时调用）。
    pub(crate) fn evict_all(&self) {
        let evicted: Vec<PooledClaude> = {
            let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            map.drain().map(|(_, v)| v).collect()
        };
        drop(evicted);
    }

    #[cfg(all(test, unix))]
    pub(crate) fn len(&self) -> usize {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// 尝试以双向流式 Claude Code 执行本轮的结果。
pub(crate) enum ClaudeAttempt {
    Finished(Finished),
    /// 启动或 `initialize` 握手前发现不兼容（或用户命令显式覆盖了输入/输出格式），安全回退单次 `claude -p`。
    FallbackToLegacy,
}

/// 若用户自定义命令已显式指定输入/输出格式或权限提示工具，则保留原有单次 `plan()` 路径不冲突。
fn wants_legacy_oneshot(extra: &[String]) -> bool {
    extra.iter().any(|w| {
        w == "--input-format"
            || w.starts_with("--input-format=")
            || w == "--output-format"
            || w.starts_with("--output-format=")
            || w == "--permission-prompt-tool"
            || w.starts_with("--permission-prompt-tool=")
    })
}

async fn write_json_line(
    stdin: &mut tokio::process::ChildStdin,
    payload: &Value,
) -> std::io::Result<()> {
    let mut line = serde_json::to_string(payload).unwrap_or_default();
    line.push('\n');
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await
}

/// 入口：尝试通过常驻双向流式 `claude --input-format stream-json` 执行本轮。
pub(crate) async fn run_claude_stream(
    launch: &Launch<'_>,
    steer_rx: &mut Option<mpsc::UnboundedReceiver<SteerRequest>>,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> ClaudeAttempt {
    let mut words = split_command(launch.command);
    if words.is_empty() {
        words.push("claude".to_string());
    }
    let program_name = words.remove(0);
    let extra = words;
    if wants_legacy_oneshot(&extra) {
        return ClaudeAttempt::FallbackToLegacy;
    }
    let Some(program) = find_program(&program_name, launch.dir) else {
        return ClaudeAttempt::Finished(Finished {
            end: TurnEnd::Failed(format!(
                "这台电脑上找不到 {program_name}，Claude Code 没法启动。装好之后再试，或者在 Bot 设置里写上它的完整路径"
            )),
            session: launch.session.map(String::from),
            text: String::new(),
            reasoning: None,
        });
    };

    let mcp_fp = mcp_fingerprint(launch.mcp_servers);
    let pool_key = match (launch.claude_pool, launch.acp_scope) {
        (Some(_), Some(scope)) => Some(ClaudeKey {
            scope: scope.to_string(),
            command: launch.command.trim().to_string(),
            dir: launch.dir.to_path_buf(),
        }),
        _ => None,
    };

    // 优先复用常驻进程；仅当连接在首条 `user` 消息写出前即断开时无感冷启动一次。
    if let (Some(pool), Some(key)) = (launch.claude_pool, pool_key.as_ref()) {
        if let Some(conn) = pool.take(key, launch.session, &mcp_fp, launch.permission) {
            match run_claude_turn_on(conn, launch, steer_rx, observer, cancel, true).await {
                ClaudeTurnOutcome::ReusedDeadBeforeDispatch => {}
                ClaudeTurnOutcome::Done { finished, keep } => {
                    if let Some(conn) = keep {
                        pool.put(key.clone(), *conn);
                    }
                    return ClaudeAttempt::Finished(finished);
                }
            }
        }
    }

    let conn = match spawn_claude_session(&program, &extra, launch, &mcp_fp, cancel).await {
        SpawnOutcome::Ready(conn) => *conn,
        SpawnOutcome::FallbackToLegacy => return ClaudeAttempt::FallbackToLegacy,
        SpawnOutcome::Aborted(end, session) => {
            return ClaudeAttempt::Finished(Finished {
                end,
                session,
                text: String::new(),
                reasoning: None,
            });
        }
    };

    match run_claude_turn_on(conn, launch, steer_rx, observer, cancel, false).await {
        ClaudeTurnOutcome::Done { finished, keep } => {
            if let (Some(pool), Some(key), Some(conn)) = (launch.claude_pool, pool_key, keep) {
                pool.put(key, *conn);
            }
            ClaudeAttempt::Finished(finished)
        }
        ClaudeTurnOutcome::ReusedDeadBeforeDispatch => ClaudeAttempt::Finished(Finished {
            end: TurnEnd::Failed("Claude Code 启动后立即退出了".into()),
            session: launch.session.map(String::from),
            text: String::new(),
            reasoning: None,
        }),
    }
}

enum SpawnOutcome {
    Ready(Box<PooledClaude>),
    FallbackToLegacy,
    Aborted(TurnEnd, Option<String>),
}

enum ClaudeTurnOutcome {
    ReusedDeadBeforeDispatch,
    Done {
        finished: Finished,
        keep: Option<Box<PooledClaude>>,
    },
}

/// 握手阶段等待匹配 `request_id` 的 `control_response`。
async fn wait_control_response(
    reader: &mut BufReader<tokio::process::ChildStdout>,
    line_buf: &mut LineBuffer,
    target_req_id: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<(bool, Option<String>), &'static str> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut session_id: Option<String> = None;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err("timeout");
        }
        let remain = deadline - now;
        let line = tokio::select! {
            _ = cancel.cancelled() => return Err("cancelled"),
            res = tokio::time::timeout(remain, read_bounded_line_buffered(reader, line_buf)) => match res {
                Ok(Ok(Some(line))) => line,
                Ok(Ok(None)) => return Err("eof"),
                Ok(Err(_)) => return Err("io"),
                Err(_) => return Err("timeout"),
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if let Some(sid) = value["session_id"].as_str().filter(|s| !s.is_empty()) {
            session_id = Some(sid.to_string());
        }
        if str_of(&value, "type") != "control_response" {
            continue;
        }
        let resp = &value["response"];
        let req_id = resp["request_id"]
            .as_str()
            .or_else(|| resp["requestId"].as_str())
            .unwrap_or("");
        if req_id != target_req_id {
            continue;
        }
        let ok = str_of(resp, "subtype") == "success";
        return Ok((ok, session_id));
    }
}

/// 启动双向流式 Claude Code 进程并完成 `control_request` `initialize` 握手。
async fn spawn_claude_session(
    program: &Path,
    extra: &[String],
    launch: &Launch<'_>,
    mcp_fp: &str,
    cancel: &CancellationToken,
) -> SpawnOutcome {
    let session = launch.session.map(String::from);
    let has = |flag: &str| {
        extra
            .iter()
            .any(|w| w == flag || w.starts_with(&format!("{flag}=")))
    };

    let mut args = Vec::new();
    args.extend(extra.iter().cloned());
    args.extend(
        [
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--replay-user-messages",
        ]
        .map(String::from),
    );
    if !has("--permission-mode") && !has("--dangerously-skip-permissions") {
        match launch.permission {
            PermissionMode::Full => {
                args.push("--dangerously-skip-permissions".into());
            }
            PermissionMode::Plan => {
                args.extend(
                    [
                        "--permission-mode",
                        "plan",
                        "--permission-prompt-tool",
                        "stdio",
                    ]
                    .map(String::from),
                );
            }
            PermissionMode::Ask => {
                args.extend(
                    [
                        "--permission-mode",
                        "acceptEdits",
                        "--permission-prompt-tool",
                        "stdio",
                    ]
                    .map(String::from),
                );
            }
        }
    }

    let mut mcp_temp = TempFileGuard(None);
    if !launch.mcp_servers.is_empty() && !has("--mcp-config") {
        if let Some(path) = write_claude_mcp_config(launch.mcp_servers) {
            args.extend(["--mcp-config".to_string(), path.display().to_string()]);
            mcp_temp = TempFileGuard(Some(path));
        }
    }
    if let Some(prev) = launch.session.filter(|s| !s.trim().is_empty()) {
        args.extend(["--resume".to_string(), prev.to_string()]);
    }

    let mut command = build_command(program, &args);
    command
        .current_dir(launch.dir)
        .env("SA_HARNESS_DIR", launch.dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for name in sa_tools::hidden_env() {
        command.env_remove(name);
    }

    let mut child = match Tree::spawn(command) {
        Ok(child) => child,
        Err(error) => {
            return SpawnOutcome::Aborted(
                TurnEnd::Failed(format!("Claude Code 启动不了：{error}")),
                session,
            );
        }
    };
    let Some(mut stdin) = child.stdin() else {
        child.kill();
        return SpawnOutcome::FallbackToLegacy;
    };
    let stderr = child
        .stderr()
        .map(|err| tokio::spawn(read_stderr_tail(err)));
    let mut reader = BufReader::new(child.stdout().expect("stdout piped"));
    let mut line_buf = LineBuffer::default();

    // 发送 `initialize` 控制请求作为无副作用握手探针：不兼容旧版 CLI 时在发 prompt 前安全回退单次 `claude -p`。
    let next_req_id = 2u64;
    let init_req_id = "myriad-init-1";
    let init_req = json!({
        "type": "control_request",
        "request_id": init_req_id,
        "request": {
            "subtype": "initialize"
        }
    });
    if write_json_line(&mut stdin, &init_req).await.is_err() {
        child.kill();
        if let Some(h) = stderr {
            h.abort();
        }
        return SpawnOutcome::FallbackToLegacy;
    }

    let init_sid = match wait_control_response(
        &mut reader,
        &mut line_buf,
        init_req_id,
        INIT_TIMEOUT,
        cancel,
    )
    .await
    {
        Ok((true, sid)) => sid,
        Err("cancelled") => {
            child.kill();
            if let Some(h) = stderr {
                h.abort();
            }
            return SpawnOutcome::Aborted(TurnEnd::Cancelled, session);
        }
        Ok((false, _)) | Err(_) => {
            child.kill();
            if let Some(h) = stderr {
                h.abort();
            }
            return SpawnOutcome::FallbackToLegacy;
        }
    };

    SpawnOutcome::Ready(Box::new(PooledClaude {
        child,
        stdin,
        reader,
        line_buf,
        stderr,
        _mcp_temp: mcp_temp,
        session_id: init_sid.or(session),
        next_req_id,
        mcp_fingerprint: mcp_fp.to_string(),
        permission: launch.permission,
        replay_supported: false,
        last_used: Instant::now(),
    }))
}

async fn collect_stderr(stderr: Option<tokio::task::JoinHandle<String>>) -> String {
    match stderr {
        Some(handle) => tokio::time::timeout(Duration::from_millis(200), handle)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default(),
        None => String::new(),
    }
}

fn claude_approval_summary(tool_name: &str, input: &Value) -> (String, String) {
    let detail = input["command"]
        .as_str()
        .or_else(|| input["file_path"].as_str())
        .or_else(|| input["path"].as_str())
        .or_else(|| input["url"].as_str())
        .unwrap_or("")
        .trim();
    let title = if detail.is_empty() {
        format!("Claude Code 请求使用 {tool_name}")
    } else {
        format!("Claude Code 请求使用 {tool_name}：{detail}")
    };
    let reason = format!("Claude Code 在当前权限档位下请求你批准执行工具 `{tool_name}`。");
    (title, reason)
}

/// 处理来自 Claude Code 的入站 `control_request`（主要是 `can_use_tool` 权限请求）。
async fn handle_claude_control_request(
    conn: &mut PooledClaude,
    value: &Value,
    permission: PermissionMode,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> std::io::Result<()> {
    let request_id = value["request_id"]
        .as_str()
        .or_else(|| value["requestId"].as_str())
        .unwrap_or("");
    let req = &value["request"];
    let subtype = str_of(req, "subtype");

    if subtype != "can_use_tool" {
        return write_json_line(
            &mut conn.stdin,
            &json!({
                "type": "control_response",
                "response": {
                    "subtype": "error",
                    "request_id": request_id,
                    "error": format!("Unsupported control request subtype: {subtype}"),
                }
            }),
        )
        .await;
    }

    let tool_name = str_of(req, "tool_name");
    let input = req.get("input").cloned().unwrap_or_else(|| json!({}));
    let call_id = format!("claude-approve-{request_id}");
    // 使用独立的 `claude_approval` 工具名，避免触发 `RoomObserver::rule_key` 的跨目录「总是允许」。
    let approval_tool = "claude_approval";

    // `AskUserQuestion` 需要宿主收集表单答案并回填 `updatedInput.answers`，不能只用批准/拒绝代替；
    // 明确拒绝并提示 Claude 直接在聊天回复里向用户提问，避免把空答案当成已批准回传。
    if tool_name == "AskUserQuestion" {
        observer
            .tool_started(
                &call_id,
                approval_tool,
                "Claude Code 尝试发起交互式问答（已转为对话提问）",
                &input.to_string(),
            )
            .await;
        observer
            .tool_finished(
                &call_id,
                StepEnd::Stopped,
                "当前宿主暂不支持结构化多选表单，请直接在聊天回复中提问",
            )
            .await;
        return write_json_line(
            &mut conn.stdin,
            &json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": request_id,
                    "response": {
                        "behavior": "deny",
                        "message": "Interactive AskUserQuestion form is not supported in this host; please ask the user directly in your chat message instead.",
                    }
                }
            }),
        )
        .await;
    }

    let (title, reason) = claude_approval_summary(tool_name, &input);

    let allowed = match permission {
        PermissionMode::Full => true,
        PermissionMode::Plan => {
            observer
                .tool_started(&call_id, approval_tool, &title, &input.to_string())
                .await;
            observer
                .tool_finished(
                    &call_id,
                    StepEnd::Stopped,
                    "规划模式（只读）已自动拦截工具执行请求",
                )
                .await;
            false
        }
        PermissionMode::Ask => {
            let args_json = input.to_string();
            observer
                .tool_started(&call_id, approval_tool, &title, &args_json)
                .await;
            let asked = observer.request_approval(ApprovalRequest {
                call_id: &call_id,
                tool: approval_tool,
                title: &title,
                reason: &reason,
                input: &input,
            });
            let decision = tokio::select! {
                decision = asked => decision,
                _ = cancel.cancelled() => false,
            };
            let (end, note) = if decision {
                (StepEnd::Ok, "已批准")
            } else if cancel.is_cancelled() {
                (StepEnd::Stopped, "已取消")
            } else {
                (StepEnd::Stopped, "已拒绝")
            };
            observer.tool_finished(&call_id, end, note).await;
            decision
        }
    };

    let response_body = if allowed {
        json!({
            "behavior": "allow",
            "updatedInput": input,
        })
    } else {
        let msg = if permission == PermissionMode::Plan {
            "Denied: room is in read-only Plan mode"
        } else if cancel.is_cancelled() {
            "Denied: turn was cancelled by user"
        } else {
            "Denied by user"
        };
        json!({
            "behavior": "deny",
            "message": msg,
        })
    };

    write_json_line(
        &mut conn.stdin,
        &json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": response_body,
            }
        }),
    )
    .await
}

struct PendingSteer {
    step_id: String,
    ack: oneshot::Sender<bool>,
}

/// 在常驻 Claude Code 进程上跑完一轮对话，支持流式输出、双向审批卡、运行中实时插话与优雅中断。
async fn run_claude_turn_on(
    mut conn: PooledClaude,
    launch: &Launch<'_>,
    steer_rx: &mut Option<mpsc::UnboundedReceiver<SteerRequest>>,
    observer: &dyn Observer,
    cancel: &CancellationToken,
    reused: bool,
) -> ClaudeTurnOutcome {
    let initial_uuid = next_uuid();
    let user_msg = json!({
        "type": "user",
        "uuid": initial_uuid,
        "session_id": conn.session_id.as_deref().unwrap_or(""),
        "parent_tool_use_id": Value::Null,
        "message": {
            "role": "user",
            "content": launch.prompt,
        }
    });
    if let Err(error) = write_json_line(&mut conn.stdin, &user_msg).await {
        if reused {
            return ClaudeTurnOutcome::ReusedDeadBeforeDispatch;
        }
        return ClaudeTurnOutcome::Done {
            finished: Finished {
                end: TurnEnd::Failed(format!("向 Claude Code 发送消息失败：{error}")),
                session: conn.session_id.clone(),
                text: String::new(),
                reasoning: None,
            },
            keep: None,
        };
    }

    // 复用已知支持 `--replay-user-messages` 的常驻进程时，必须先等到本轮 `initial_uuid` 回显才开始采纳输出与 `result`，
    // 避免两轮之间后台任务完成产生的滞留 `result` 误把新一轮回复提前关闭。
    let mut prompt_started = !(reused && conn.replay_supported);
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut streamed_block = false;
    let mut streamed_thinking = false;
    let mut needs_turn_sep = false;
    let mut open_steps: HashMap<String, String> = HashMap::new();
    let mut pending_steers: HashMap<String, PendingSteer> = HashMap::new();
    let mut steer_seq = 0u64;
    let mut step_seq = 0usize;
    let mut thoughts = 0usize;
    let mut failure: Option<String> = None;
    let mut clean_for_pool = false;

    let end = loop {
        let can_steer = prompt_started && conn.replay_supported && steer_rx.is_some();
        tokio::select! {
            _ = cancel.cancelled() => {
                let req_id = format!("myriad-int-{}", conn.next_req_id);
                conn.next_req_id += 1;
                let _ = write_json_line(&mut conn.stdin, &json!({
                    "type": "control_request",
                    "request_id": req_id,
                    "request": {
                        "subtype": "interrupt"
                    }
                })).await;
                let deadline = tokio::time::Instant::now() + INTERRUPT_GRACE;
                while tokio::time::Instant::now() < deadline {
                    let remain = deadline - tokio::time::Instant::now();
                    match tokio::time::timeout(
                        remain,
                        read_bounded_line_buffered(&mut conn.reader, &mut conn.line_buf),
                    ).await {
                        Ok(Ok(Some(line))) => {
                            if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
                                if str_of(&v, "type") == "result" {
                                    break;
                                }
                            }
                        }
                        _ => break,
                    }
                }
                break TurnEnd::Cancelled;
            }
            maybe_steer = async {
                match steer_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            }, if can_steer => {
                match maybe_steer {
                    Some(req) => {
                        steer_seq += 1;
                        let steer_uuid = next_uuid();
                        let step_id = format!("claude-steer-{steer_seq}");
                        let preview: String = req.text.chars().take(64).collect();
                        observer
                            .tool_started(&step_id, "steer", &format!("运行中插话：{preview}"), &req.text)
                            .await;
                        let payload = json!({
                            "type": "user",
                            "uuid": steer_uuid,
                            "session_id": conn.session_id.as_deref().unwrap_or(""),
                            "parent_tool_use_id": Value::Null,
                            "message": {
                                "role": "user",
                                "content": req.text,
                            }
                        });
                        if write_json_line(&mut conn.stdin, &payload).await.is_ok() {
                            pending_steers.insert(steer_uuid, PendingSteer { step_id, ack: req.ack });
                        } else {
                            observer
                                .tool_finished(&step_id, StepEnd::Stopped, "Claude Code 连接已关闭，未注入")
                                .await;
                            let _ = req.ack.send(false);
                        }
                    }
                    None => {
                        *steer_rx = None;
                    }
                }
            }
            line_res = tokio::time::timeout(
                IDLE_TIMEOUT,
                read_bounded_line_buffered(&mut conn.reader, &mut conn.line_buf),
            ) => {
                let line = match line_res {
                    Ok(Ok(Some(line))) => line,
                    Ok(Ok(None)) => {
                        let err_tail = collect_stderr(conn.stderr.take()).await;
                        let detail = if let Some(msg) = failure.take() {
                            msg
                        } else if !err_tail.trim().is_empty() {
                            tail(err_tail.trim(), STDERR_TAIL)
                        } else {
                            "Claude Code 在本轮结束前意外退出了".to_string()
                        };
                        break TurnEnd::Failed(detail);
                    }
                    Ok(Err(error)) => {
                        break TurnEnd::Failed(format!("读取 Claude Code 输出失败：{error}"));
                    }
                    Err(_) => {
                        break TurnEnd::Failed(format!(
                            "Claude Code 超过 {} 分钟没有任何输出，已自动停止",
                            IDLE_TIMEOUT.as_secs() / 60
                        ));
                    }
                };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
                    continue;
                };

                if let Some(sid) = value["session_id"].as_str().filter(|s| !s.is_empty()) {
                    conn.session_id = Some(sid.to_string());
                }

                let msg_type = str_of(&value, "type");
                if msg_type == "control_request" {
                    if handle_claude_control_request(
                        &mut conn,
                        &value,
                        launch.permission,
                        observer,
                        cancel,
                    )
                    .await
                    .is_err()
                    {
                        break TurnEnd::Failed("向 Claude Code 回复审批结果失败".into());
                    }
                    continue;
                }
                if matches!(
                    msg_type,
                    "control_response" | "keep_alive" | "control_cancel_request"
                ) {
                    continue;
                }

                // `--replay-user-messages` 回显：用于确认本进程支持回显、标记本轮已正式开始以及精确确认运行中插话已消费。
                if msg_type == "user" {
                    let echoed_uuid = str_of(&value, "uuid");
                    if echoed_uuid == initial_uuid {
                        conn.replay_supported = true;
                        prompt_started = true;
                        continue;
                    }
                    if !prompt_started {
                        continue;
                    }
                    if !echoed_uuid.is_empty() {
                        if let Some(pending) = pending_steers.remove(echoed_uuid) {
                            observer
                                .tool_finished(
                                    &pending.step_id,
                                    StepEnd::Ok,
                                    "已实时注入当前 Claude Code 会话",
                                )
                                .await;
                            let _ = pending.ack.send(true);
                        }
                    }
                }

                // 尚未收到本轮 `initial_uuid` 回显前，丢弃空闲期间后台任务产生的滞留输出与 `result`。
                if !prompt_started {
                    continue;
                }

                let is_stream_event = msg_type == "stream_event";
                for event in parse_claude(&value) {
                    match event {
                        Event::Session(id) if !id.is_empty() => {
                            conn.session_id = Some(id);
                        }
                        Event::Session(_) => {}
                        Event::TextDelta(delta) if !delta.is_empty() => {
                            if needs_turn_sep {
                                needs_turn_sep = false;
                                if !text.is_empty() {
                                    text.push_str("\n\n");
                                    observer.text_delta("\n\n").await;
                                }
                            }
                            streamed_block = true;
                            text.push_str(&delta);
                            observer.text_delta(&delta).await;
                        }
                        Event::TextDelta(_) => {}
                        Event::Text(chunk) if !chunk.is_empty() => {
                            if streamed_block {
                                streamed_block = false;
                                continue;
                            }
                            needs_turn_sep = false;
                            let piece = if text.is_empty() {
                                chunk
                            } else {
                                format!("\n\n{chunk}")
                            };
                            text.push_str(&piece);
                            observer.text_delta(&piece).await;
                        }
                        Event::Text(_) => {
                            streamed_block = false;
                        }
                        Event::Reasoning(chunk) if !chunk.is_empty() => {
                            if is_stream_event {
                                streamed_thinking = true;
                                reasoning.push_str(&chunk);
                            } else {
                                if !streamed_thinking {
                                    if !reasoning.is_empty() {
                                        reasoning.push('\n');
                                    }
                                    reasoning.push_str(&chunk);
                                }
                                streamed_thinking = false;
                                if chunk.chars().count() >= 12 {
                                    thoughts += 1;
                                    let key = format!("think-{thoughts}");
                                    observer.tool_started(&key, "think", "思考", "").await;
                                    observer.tool_finished(&key, StepEnd::Ok, &chunk).await;
                                }
                            }
                        }
                        Event::Reasoning(_) => {}
                        Event::Started { id, tool, title } => {
                            streamed_block = false;
                            streamed_thinking = false;
                            step_seq += 1;
                            let step_key = format!("cl{step_seq}-{id}");
                            open_steps.insert(id, step_key.clone());
                            observer.tool_started(&step_key, &tool, &title, "").await;
                        }
                        Event::Ended {
                            id,
                            tool,
                            title,
                            ok,
                            output,
                        } => {
                            let step_key = match open_steps.remove(&id) {
                                Some(k) => k,
                                None => {
                                    step_seq += 1;
                                    let k = format!("cl{step_seq}-{id}");
                                    observer.tool_started(&k, &tool, &title, "").await;
                                    k
                                }
                            };
                            let end = if ok { StepEnd::Ok } else { StepEnd::Failed };
                            observer.tool_finished(&step_key, end, &output).await;
                        }
                        Event::Usage { model, usage } => {
                            observer.usage(&model, usage).await;
                        }
                        Event::Failed(reason) => {
                            failure = Some(reason);
                        }
                    }
                }

                if msg_type == "result" {
                    let is_err = value["is_error"].as_bool().unwrap_or(false)
                        || str_of(&value, "subtype").starts_with("error");
                    if is_err || failure.is_some() {
                        let msg = failure.take().unwrap_or_else(|| {
                            let r = str_of(&value, "result");
                            if r.is_empty() {
                                format!("Claude Code 执行失败（{}）", str_of(&value, "subtype"))
                            } else {
                                r.to_string()
                            }
                        });
                        break TurnEnd::Failed(msg);
                    }
                    // 若仍有已写入 stdin 但尚未回显 `uuid` 的运行中插话：说明 Claude Code 已将其排在下一拍，
                    // 继续在本循环里等它回显并跑完对应 `result`，避免把带未消费队列的进程归还池中或造成重复执行。
                    if !pending_steers.is_empty() {
                        streamed_block = false;
                        streamed_thinking = false;
                        needs_turn_sep = !text.is_empty();
                        continue;
                    }
                    clean_for_pool = true;
                    break TurnEnd::Completed;
                }
            }
        }
    };

    // 任何未收到回显确认的插话通知调用方 `false`，由 room.rs 保留未读游标留待下一轮补答。
    for (_, pending) in pending_steers.drain() {
        observer
            .tool_finished(
                &pending.step_id,
                StepEnd::Stopped,
                "当前轮次已结束，转为下一轮处理",
            )
            .await;
        let _ = pending.ack.send(false);
    }

    let session_out = conn.session_id.clone();
    let reasoning_out = (!reasoning.trim().is_empty()).then_some(reasoning);

    // 仅当连接支持 `--replay-user-messages`（从而能在下轮精确按 `uuid` 屏蔽空闲期间后台滞留 `result`）时才入池复用。
    let keep = if clean_for_pool
        && conn.replay_supported
        && conn.session_id.is_some()
        && launch.claude_pool.is_some()
        && launch.acp_scope.is_some()
    {
        Some(Box::new(conn))
    } else {
        drop(conn);
        None
    };

    ClaudeTurnOutcome::Done {
        finished: Finished {
            end,
            session: session_out,
            text,
            reasoning: reasoning_out,
        },
        keep,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_oneshot_detection_respects_explicit_format_flags() {
        assert!(!wants_legacy_oneshot(&[]));
        assert!(!wants_legacy_oneshot(&[
            "--model".into(),
            "claude-sonnet-4-6".into()
        ]));
        assert!(wants_legacy_oneshot(&[
            "--input-format".into(),
            "text".into()
        ]));
        assert!(wants_legacy_oneshot(&["--output-format=json".into()]));
        assert!(wants_legacy_oneshot(&[
            "--permission-prompt-tool".into(),
            "mcp__perm".into()
        ]));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn claude_stream_reuses_pool_steers_live_approves_interrupts_and_falls_back() {
        use crate::harness::{run, run_interactive};
        use sa_llm::Usage;
        use sa_protocol::Harness;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Mutex;

        #[derive(Default)]
        struct RecordingObserver {
            approve_response: bool,
            started_ids: Mutex<Vec<String>>,
            approvals: Mutex<Vec<(String, String, bool)>>,
            usages: Mutex<Vec<Usage>>,
        }

        #[async_trait::async_trait]
        impl Observer for RecordingObserver {
            async fn text_delta(&self, _: &str) {}
            async fn tool_started(&self, id: &str, _: &str, _: &str, _: &str) {
                self.started_ids.lock().unwrap().push(id.to_string());
            }
            async fn tool_finished(&self, _: &str, _: StepEnd, _: &str) {}
            async fn usage(&self, _: &str, usage: Usage) {
                self.usages.lock().unwrap().push(usage);
            }
            async fn request_approval(&self, req: ApprovalRequest<'_>) -> bool {
                assert_eq!(req.tool, "claude_approval");
                let has_step = self
                    .started_ids
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|id| id == req.call_id);
                self.approvals.lock().unwrap().push((
                    req.call_id.to_string(),
                    req.title.to_string(),
                    has_step,
                ));
                self.approve_response
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-claude.py");
        std::fs::write(
            &script,
            r#"#!/usr/bin/env python3
import json, os, sys

# 支持双向 `--input-format stream-json` 与 legacy 单次 `-p --output-format stream-json`
is_bidirectional = "--input-format" in sys.argv
if not is_bidirectional:
    prompt = sys.stdin.read().strip()
    print(json.dumps({
        "type": "assistant",
        "session_id": "legacy-cc",
        "message": {"content": [{"type": "text", "text": f"legacy:{prompt}"}]}
    }), flush=True)
    print(json.dumps({"type": "result", "subtype": "success", "is_error": False, "session_id": "legacy-cc"}), flush=True)
    sys.exit(0)

pid = os.getpid()
sid = f"cc-{pid}"
if "--resume" in sys.argv:
    idx = sys.argv.index("--resume")
    if idx + 1 < len(sys.argv):
        sid = sys.argv[idx + 1]
perm_mode = "default"
if "--dangerously-skip-permissions" in sys.argv:
    perm_mode = "bypassPermissions"
elif "--permission-mode" in sys.argv:
    idx = sys.argv.index("--permission-mode")
    if idx + 1 < len(sys.argv):
        perm_mode = sys.argv[idx + 1]

turn_n = 0
def send(obj):
    print(json.dumps(obj), flush=True)

for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    msg = json.loads(raw)
    mtype = msg.get("type")
    if mtype == "control_request":
        rid = msg.get("request_id", "")
        sub = msg.get("request", {}).get("subtype", "")
        if sub == "initialize":
            send({"type": "control_response", "session_id": sid, "response": {"subtype": "success", "request_id": rid, "response": {}}})
        elif sub == "interrupt":
            send({"type": "control_response", "response": {"subtype": "success", "request_id": rid, "response": {}}})
            send({"type": "result", "subtype": "interrupted", "is_error": True, "session_id": sid})
        continue
    if mtype != "user":
        continue

    turn_n += 1
    uid = msg.get("uuid", "")
    prompt = msg["message"]["content"]
    # 回显 --replay-user-messages
    send({"type": "user", "uuid": uid, "session_id": sid, "isReplay": True, "message": msg["message"]})

    if "wait-steer" in prompt:
        # 故意拆分一条流式 delta 为半行验证 cancel-safe LineBuffer，并在收到 steer 后继续输出。
        frame = json.dumps({"type": "stream_event", "session_id": sid, "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "working..."}}})
        half = len(frame) // 2
        sys.stdout.write(frame[:half])
        sys.stdout.flush()
        for raw2 in sys.stdin:
            m2 = json.loads(raw2.strip())
            if m2.get("type") == "user":
                sys.stdout.write(frame[half:] + "\n")
                sys.stdout.flush()
                suid = m2.get("uuid", "")
                stext = m2["message"]["content"]
                send({"type": "user", "uuid": suid, "session_id": sid, "isReplay": True, "message": m2["message"]})
                send({"type": "stream_event", "session_id": sid, "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": f"|steered:{stext}"}}})
                break
    elif "queued-steer" in prompt:
        # 模拟 Case B：首轮先发完 `result`，随后立刻消费 stdin 中已排队的 steer 并发出第二轮 `result`。
        send({"type": "stream_event", "session_id": sid, "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "part1"}}})
        for raw2 in sys.stdin:
            m2 = json.loads(raw2.strip())
            if m2.get("type") == "user":
                send({"type": "result", "subtype": "success", "is_error": False, "session_id": sid})
                suid = m2.get("uuid", "")
                stext = m2["message"]["content"]
                send({"type": "user", "uuid": suid, "session_id": sid, "isReplay": True, "message": m2["message"]})
                send({"type": "stream_event", "session_id": sid, "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": f"part2:{stext}"}}})
                break
    elif "wait-interrupt" in prompt:
        send({"type": "stream_event", "session_id": sid, "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "long..."}}})
        for raw2 in sys.stdin:
            m2 = json.loads(raw2.strip())
            if m2.get("type") == "control_request" and m2["request"]["subtype"] == "interrupt":
                send({"type": "control_response", "response": {"subtype": "success", "request_id": m2["request_id"], "response": {}}})
                send({"type": "result", "subtype": "interrupted", "is_error": True, "session_id": sid})
                break
        continue
    elif "bg-after" in prompt:
        # 模拟 Turn 1 结束后，后台任务在空闲期间产生滞留输出与 result（早于下一轮 user 回显到达）。
        send({"type": "assistant", "session_id": sid, "message": {"content": [{"type": "text", "text": f"pid={pid},turn={turn_n},bg-started"}]}})
        send({"type": "result", "subtype": "success", "is_error": False, "session_id": sid})
        send({"type": "assistant", "session_id": sid, "message": {"content": [{"type": "text", "text": "stale-bg-output"}]}})
        send({"type": "result", "subtype": "success", "is_error": False, "session_id": sid})
        continue
    elif "ask-question" in prompt:
        send({
            "type": "control_request",
            "request_id": f"q-{turn_n}",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "AskUserQuestion",
                "tool_use_id": "tu-q1",
                "input": {"questions": [{"question": "SQLite or Postgres?"}]}
            }
        })
        resp = json.loads(sys.stdin.readline())
        behavior = resp["response"]["response"]["behavior"]
        send({"type": "assistant", "session_id": sid, "message": {"content": [{"type": "text", "text": f"question_behavior={behavior}"}]}})
    elif "ask-bash" in prompt:
        send({
            "type": "control_request",
            "request_id": f"perm-{turn_n}",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Bash",
                "tool_use_id": "tu1",
                "input": {"command": "cargo test"}
            }
        })
        resp = json.loads(sys.stdin.readline())
        behavior = resp["response"]["response"]["behavior"]
        send({"type": "assistant", "session_id": sid, "message": {"content": [{"type": "text", "text": f"pid={pid},turn={turn_n},perm={behavior},mode={perm_mode}"}]}})
    else:
        send({"type": "assistant", "session_id": sid, "message": {"content": [{"type": "text", "text": f"pid={pid},turn={turn_n},mode={perm_mode}"}]}})

    send({
        "type": "result",
        "subtype": "success",
        "is_error": False,
        "session_id": sid,
        "model": "claude-sonnet-4-6",
        "usage": {"input_tokens": 100, "cache_read_input_tokens": 50, "cache_creation_input_tokens": 20, "output_tokens": 40}
    })
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let pool = ClaudePool::default();
        let cmd = script.display().to_string();
        let obs = RecordingObserver {
            approve_response: true,
            ..Default::default()
        };

        // 1. 第一轮冷启动 + 第二轮复用常驻进程（PID 相同、turn 递增、Ask 档建步弹审批卡、用量精确含 cache_write）。
        let first = run(
            &Launch {
                harness: Harness::ClaudeCode,
                command: &cmd,
                dir: dir.path(),
                prompt: "hello",
                session: None,
                permission: PermissionMode::Ask,
                mcp_servers: &[],
                acp_pool: None,
                codex_pool: None,
                claude_pool: Some(&pool),
                acp_scope: Some("room:r1:bot:cc"),
                interactive_approvals: true,
            },
            &obs,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(first.end, TurnEnd::Completed);
        assert_eq!(pool.len(), 1);
        assert!(
            first.text.contains("turn=1,mode=acceptEdits"),
            "{}",
            first.text
        );

        let second = run(
            &Launch {
                harness: Harness::ClaudeCode,
                command: &cmd,
                dir: dir.path(),
                prompt: "ask-bash",
                session: first.session.as_deref(),
                permission: PermissionMode::Ask,
                mcp_servers: &[],
                acp_pool: None,
                codex_pool: None,
                claude_pool: Some(&pool),
                acp_scope: Some("room:r1:bot:cc"),
                interactive_approvals: true,
            },
            &obs,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(second.end, TurnEnd::Completed);
        assert_eq!(second.session, first.session);
        assert!(second.text.contains("turn=2,perm=allow"), "{}", second.text);
        let approvals = obs.approvals.lock().unwrap().clone();
        assert_eq!(approvals.len(), 1);
        assert!(
            approvals[0].2,
            "approval step was not started before request_approval"
        );
        let usages = obs.usages.lock().unwrap().clone();
        assert_eq!(usages.len(), 2);
        assert_eq!(usages[0].input_tokens, 170);
        assert_eq!(usages[0].cache_write_tokens, 20);

        // 1b. `AskUserQuestion` 必须显式拒绝而不弹批准卡（避免回传空 answers）。
        let q_obs = RecordingObserver {
            approve_response: true,
            ..Default::default()
        };
        let asked_q = run(
            &Launch {
                harness: Harness::ClaudeCode,
                command: &cmd,
                dir: dir.path(),
                prompt: "ask-question",
                session: second.session.as_deref(),
                permission: PermissionMode::Ask,
                mcp_servers: &[],
                acp_pool: None,
                codex_pool: None,
                claude_pool: Some(&pool),
                acp_scope: Some("room:r1:bot:cc"),
                interactive_approvals: true,
            },
            &q_obs,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(asked_q.end, TurnEnd::Completed);
        assert_eq!(asked_q.text, "question_behavior=deny");
        assert!(q_obs.approvals.lock().unwrap().is_empty());

        // 1c. 两轮之间后台任务向 stdout 写入滞留 `result` 时，下一轮必须跳过滞留输出直到收到本轮 `uuid` 回显。
        let bg_turn = run(
            &Launch {
                harness: Harness::ClaudeCode,
                command: &cmd,
                dir: dir.path(),
                prompt: "bg-after",
                session: asked_q.session.as_deref(),
                permission: PermissionMode::Ask,
                mcp_servers: &[],
                acp_pool: None,
                codex_pool: None,
                claude_pool: Some(&pool),
                acp_scope: Some("room:r1:bot:cc"),
                interactive_approvals: true,
            },
            &obs,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(bg_turn.end, TurnEnd::Completed);
        assert!(bg_turn.text.contains("bg-started"), "{}", bg_turn.text);

        let after_bg = run(
            &Launch {
                harness: Harness::ClaudeCode,
                command: &cmd,
                dir: dir.path(),
                prompt: "real-next-question",
                session: bg_turn.session.as_deref(),
                permission: PermissionMode::Ask,
                mcp_servers: &[],
                acp_pool: None,
                codex_pool: None,
                claude_pool: Some(&pool),
                acp_scope: Some("room:r1:bot:cc"),
                interactive_approvals: true,
            },
            &obs,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(after_bg.end, TurnEnd::Completed);
        assert!(
            !after_bg.text.contains("stale-bg-output"),
            "leaked stale bg output: {}",
            after_bg.text
        );
        assert!(after_bg.text.contains("turn=5"), "{}", after_bg.text);

        // 2. 切到 Plan 模式时自动淘汰旧 Ask 进程并自动拒绝 can_use_tool 请求。
        let plan_obs = RecordingObserver {
            approve_response: true,
            ..Default::default()
        };
        let planned = run(
            &Launch {
                harness: Harness::ClaudeCode,
                command: &cmd,
                dir: dir.path(),
                prompt: "ask-bash",
                session: second.session.as_deref(),
                permission: PermissionMode::Plan,
                mcp_servers: &[],
                acp_pool: None,
                codex_pool: None,
                claude_pool: Some(&pool),
                acp_scope: Some("room:r1:bot:cc"),
                interactive_approvals: true,
            },
            &plan_obs,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(planned.end, TurnEnd::Completed);
        assert!(
            planned.text.contains("turn=1,perm=deny,mode=plan"),
            "{}",
            planned.text
        );
        assert!(plan_obs.approvals.lock().unwrap().is_empty());

        // 3. 运行中实时插话（半帧跨 steer 恢复 + uuid 回显 ack）。
        let (steer_tx, steer_rx) = mpsc::unbounded_channel();
        let (ack_tx, ack_rx) = oneshot::channel();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            let _ = steer_tx.send(SteerRequest {
                text: "switch-plan".into(),
                ack: ack_tx,
            });
        });
        let steered = run_interactive(
            &Launch {
                harness: Harness::ClaudeCode,
                command: &cmd,
                dir: dir.path(),
                prompt: "wait-steer",
                session: planned.session.as_deref(),
                permission: PermissionMode::Plan,
                mcp_servers: &[],
                acp_pool: None,
                codex_pool: None,
                claude_pool: Some(&pool),
                acp_scope: Some("room:r1:bot:cc"),
                interactive_approvals: true,
            },
            Some(steer_rx),
            &obs,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(steered.end, TurnEnd::Completed);
        assert!(ack_rx.await.unwrap());
        assert_eq!(steered.text, "working...|steered:switch-plan");

        // 3b. 模拟 Case B：首轮 `result` 先于排队中的插话发出，驱动自动续读直到插话回显且第二轮 `result` 完成，并插入分段空行。
        let (steer_tx2, steer_rx2) = mpsc::unbounded_channel();
        let (ack_tx2, ack_rx2) = oneshot::channel();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            let _ = steer_tx2.send(SteerRequest {
                text: "add-tests".into(),
                ack: ack_tx2,
            });
        });
        let queued_steered = run_interactive(
            &Launch {
                harness: Harness::ClaudeCode,
                command: &cmd,
                dir: dir.path(),
                prompt: "queued-steer",
                session: steered.session.as_deref(),
                permission: PermissionMode::Plan,
                mcp_servers: &[],
                acp_pool: None,
                codex_pool: None,
                claude_pool: Some(&pool),
                acp_scope: Some("room:r1:bot:cc"),
                interactive_approvals: true,
            },
            Some(steer_rx2),
            &obs,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(queued_steered.end, TurnEnd::Completed);
        assert!(ack_rx2.await.unwrap());
        assert_eq!(queued_steered.text, "part1\n\npart2:add-tests");
        assert_eq!(pool.len(), 1);

        // 4. 取消时发 interrupt 优雅结束且不归还连接池。
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            trigger.cancel();
        });
        let interrupted = run(
            &Launch {
                harness: Harness::ClaudeCode,
                command: &cmd,
                dir: dir.path(),
                prompt: "wait-interrupt",
                session: steered.session.as_deref(),
                permission: PermissionMode::Plan,
                mcp_servers: &[],
                acp_pool: None,
                codex_pool: None,
                claude_pool: Some(&pool),
                acp_scope: Some("room:r1:bot:cc"),
                interactive_approvals: true,
            },
            &obs,
            &cancel,
        )
        .await;
        assert_eq!(interrupted.end, TurnEnd::Cancelled);
        assert_eq!(pool.len(), 0);

        // 5. `interactive_approvals: false`（例如 `sa share`）直接走 legacy 单次模式。
        let shared = run(
            &Launch {
                harness: Harness::ClaudeCode,
                command: &cmd,
                dir: dir.path(),
                prompt: "remote-task",
                session: None,
                permission: PermissionMode::Ask,
                mcp_servers: &[],
                acp_pool: None,
                codex_pool: None,
                claude_pool: None,
                acp_scope: None,
                interactive_approvals: false,
            },
            &obs,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(shared.end, TurnEnd::Completed);
        assert_eq!(shared.text, "legacy:remote-task");

        // 6. 旧版 Claude CLI（不支持 `--input-format stream-json`）握手失败后自动回退 legacy。
        let old_script = dir.path().join("legacy-claude.py");
        std::fs::write(
            &old_script,
            r#"#!/usr/bin/env python3
import json, sys
if "--input-format" in sys.argv:
    sys.stderr.write("error: unknown option '--input-format'\n")
    sys.exit(2)
prompt = sys.stdin.read().strip()
print(json.dumps({"type": "assistant", "session_id": "old-cc", "message": {"content": [{"type": "text", "text": f"fallback:{prompt}"}]}}), flush=True)
"#,
        )
        .unwrap();
        std::fs::set_permissions(&old_script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let fallback = run(
            &Launch {
                harness: Harness::ClaudeCode,
                command: &old_script.display().to_string(),
                dir: dir.path(),
                prompt: "still-works",
                session: None,
                permission: PermissionMode::Ask,
                mcp_servers: &[],
                acp_pool: None,
                codex_pool: None,
                claude_pool: Some(&pool),
                acp_scope: Some("room:r1:bot:oldcc"),
                interactive_approvals: true,
            },
            &obs,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(fallback.end, TurnEnd::Completed);
        assert_eq!(fallback.session.as_deref(), Some("old-cc"));
        assert_eq!(fallback.text, "fallback:still-works");

        // 7. 空闲定时器在 TTL 到期后主动淘汰并回收常驻进程（即使不再调用 take/put）。
        let expiring_pool = ClaudePool::default();
        let key = ClaudeKey {
            scope: "room:r1:bot:ttl".into(),
            command: cmd.clone(),
            dir: dir.path().to_path_buf(),
        };
        let spawned = spawn_claude_session(
            Path::new(&cmd),
            &[],
            &Launch {
                harness: Harness::ClaudeCode,
                command: &cmd,
                dir: dir.path(),
                prompt: "ttl",
                session: None,
                permission: PermissionMode::Ask,
                mcp_servers: &[],
                acp_pool: None,
                codex_pool: None,
                claude_pool: None,
                acp_scope: None,
                interactive_approvals: true,
            },
            &mcp_fingerprint(&[]),
            &CancellationToken::new(),
        )
        .await;
        let SpawnOutcome::Ready(conn) = spawned else {
            panic!("expected ready claude session");
        };
        expiring_pool.put_with_ttl(key, *conn, Duration::from_millis(60));
        assert_eq!(expiring_pool.len(), 1);
        tokio::time::sleep(Duration::from_millis(140)).await;
        assert_eq!(expiring_pool.len(), 0);

        // 校验随机 UUIDv4 格式与唯一性（重启不撞历史会话 ID）。
        let u1 = next_uuid();
        let u2 = next_uuid();
        assert!(uuid::Uuid::parse_str(&u1).is_ok());
        assert_ne!(u1, u2);

        pool.evict_all();
        assert_eq!(pool.len(), 0);
    }
}
