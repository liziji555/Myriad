//! Codex `app-server` 双向持久会话驱动与运行中插话（`turn/steer`）支持。
//!
//! 相比单次冷启动的 `codex exec --json`，`codex app-server --stdio` 通过持久 JSON-RPC
//! 连接跨轮复用线程与内存状态，支持：
//! - 持久进程池（`CodexPool`）：同工位跨轮复用常驻 `codex app-server` 进程与 `threadId`；
//! - 实时运行中插话（`turn/steer`）：用户在 Codex 干活中途发来新指示时直接注入当前活跃轮次；
//! - 双向交互式审批：将 `item/commandExecution/requestApproval` / `item/fileChange/requestApproval`
//!   等请求桥接到 Myriad 审批卡（`Plan` 模式自动拒绝写入/执行，`Full` 自动放行）；
//! - 优雅中断（`turn/interrupt`）与低版本 Codex 自动回退（握手阶段失败时无感回退到 `codex exec --json`）。

use crate::harness::{
    build_command, codex_item, find_program, mcp_fingerprint, read_bounded_line, read_stderr_tail,
    split_command, str_of, tail, Finished, Launch, IDLE_TIMEOUT, STDERR_TAIL,
};
use sa_kernel::{ApprovalRequest, Observer, StepEnd, TurnEnd};
use sa_llm::Usage;
use sa_protocol::PermissionMode;
use sa_tools::mcp::McpSpec;
use sa_tools::process::Tree;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// 持久 Codex app-server 进程池容量上限（超出时按最久未使用淘汰）。
const CODEX_POOL_MAX: usize = 16;
/// 持久 Codex app-server 进程空闲超时时间。
const CODEX_IDLE_TTL: Duration = Duration::from_secs(15 * 60);
/// `initialize` 握手等待超时（超时视作不兼容或卡死，安全回退 legacy `codex exec`）。
const INIT_TIMEOUT: Duration = Duration::from_secs(15);
/// 停止时等待 `turn/interrupt` 收尾的最长时间。
const INTERRUPT_GRACE: Duration = Duration::from_millis(800);

/// 运行中插话请求：由 `room.rs` 在收到新消息时发送，由 `codex app-server` 通过 `turn/steer` 注入。
/// `ack` 为 `true` 表示 `turn/steer` 已被 Codex 接收，`false`（或通道关闭）表示未能注入，调用方保留未读游标留待下轮处理。
pub(crate) struct SteerRequest {
    pub text: String,
    pub ack: oneshot::Sender<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CodexKey {
    scope: String,
    command: String,
    dir: PathBuf,
}

#[derive(Debug, Default, Clone, Copy)]
struct TokenTotals {
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
}

struct PooledCodex {
    child: Tree,
    stdin: tokio::process::ChildStdin,
    reader: BufReader<tokio::process::ChildStdout>,
    stderr: Option<tokio::task::JoinHandle<String>>,
    thread_id: String,
    next_id: u64,
    mcp_fingerprint: String,
    permission: PermissionMode,
    /// 上一次从 `thread/tokenUsage/updated` 观测到的线程累计 token，用于计算本轮增量用量。
    prev_total: TokenTotals,
    /// `prev_total` 是否已与当前线程累计基线对齐（新线程天生为 `true`；`thread/resume` 首次收到 usage 时按 `last` 对齐，避免重复计入历史轮次）。
    total_initialized: bool,
    last_used: Instant,
}

impl Drop for PooledCodex {
    fn drop(&mut self) {
        self.child.kill();
        if let Some(handle) = self.stderr.take() {
            handle.abort();
        }
    }
}

/// 持久 Codex `app-server` 进程池：同一房间工位的 Codex Bot 跨轮保持常驻进程与线程上下文。
#[derive(Default)]
pub(crate) struct CodexPool {
    entries: std::sync::Mutex<HashMap<CodexKey, PooledCodex>>,
}

impl CodexPool {
    fn take(
        &self,
        key: &CodexKey,
        expected_session: Option<&str>,
        mcp_fp: &str,
        permission: PermissionMode,
    ) -> Option<PooledCodex> {
        let mut stale = Vec::new();
        let taken = {
            let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            let expired_keys: Vec<CodexKey> = map
                .iter()
                .filter(|(_, v)| v.last_used.elapsed() > CODEX_IDLE_TTL)
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
            && expected_session == Some(conn.thread_id.as_str());
        if valid {
            Some(conn)
        } else {
            drop(conn);
            None
        }
    }

    fn put(&self, key: CodexKey, mut conn: PooledCodex) {
        conn.last_used = Instant::now();
        let mut evicted = Vec::new();
        {
            let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            let expired_keys: Vec<CodexKey> = map
                .iter()
                .filter(|(_, v)| v.last_used.elapsed() > CODEX_IDLE_TTL)
                .map(|(k, _)| k.clone())
                .collect();
            for k in expired_keys {
                if let Some(v) = map.remove(&k) {
                    evicted.push(v);
                }
            }
            while map.len() >= CODEX_POOL_MAX {
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
            if let Some(prev) = map.insert(key, conn) {
                evicted.push(prev);
            }
        }
        drop(evicted);
    }

    /// 驱逐指定作用域（重置工位会话、关闭工位）的全部常驻 Codex 进程。
    pub(crate) fn evict_scope(&self, scope: &str) {
        let mut evicted = Vec::new();
        {
            let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            let keys: Vec<CodexKey> = map.keys().filter(|k| k.scope == scope).cloned().collect();
            for k in keys {
                if let Some(v) = map.remove(&k) {
                    evicted.push(v);
                }
            }
        }
        drop(evicted);
    }

    /// 驱逐所有常驻 Codex 进程（服务关闭或 MCP 热重载时调用）。
    pub(crate) fn evict_all(&self) {
        let evicted: Vec<PooledCodex> = {
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

/// 尝试以 `codex app-server` 执行本轮的结果。
pub(crate) enum CodexAttempt {
    /// 已通过 `codex app-server` 执行完成（成功、失败或取消）。
    Finished(Finished),
    /// 启动或 `initialize` 握手前发现该命令/版本不支持 `app-server`，调用方可安全回退到 legacy `codex exec --json`。
    FallbackToLegacy,
}

/// 判断用户自定义的 Codex 命令是否显式使用了 `exec` 专属参数（若是则直接走 legacy `codex exec`）。
fn wants_legacy_exec(extra: &[String]) -> bool {
    if extra.first().is_some_and(|w| w == "exec") {
        return true;
    }
    extra.iter().any(|w| {
        matches!(
            w.as_str(),
            "--json"
                | "--skip-git-repo-check"
                | "--full-auto"
                | "--dangerously-bypass-approvals-and-sandbox"
        )
    })
}

/// 根据权限档位映射 Codex `app-server` 的 `approvalPolicy` 与 `sandbox` 策略。
fn codex_permission_params(permission: PermissionMode) -> (&'static str, &'static str) {
    match permission {
        PermissionMode::Full => ("never", "danger-full-access"),
        PermissionMode::Plan => ("never", "read-only"),
        PermissionMode::Ask => ("on-request", "workspace-write"),
    }
}

/// 将 Myriad 的 MCP 规格转换为 `codex app-server` 的 `thread/start.config` 覆盖表和环境变量。
fn build_codex_mcp_config(
    specs: &[McpSpec],
) -> (serde_json::Map<String, Value>, Vec<(String, String)>) {
    let mut config = serde_json::Map::new();
    let mut env_vars = Vec::new();
    let mut seen_keys = HashSet::new();
    for spec in specs {
        if spec.name.is_empty()
            || !spec
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        {
            continue;
        }
        let key = spec.name.clone();
        if !seen_keys.insert(key.clone()) {
            continue;
        }
        if let Some(url) = &spec.url {
            config.insert(format!("mcp_servers.{key}.url"), Value::String(url.clone()));
            for (hname, hval) in &spec.headers {
                if hname.eq_ignore_ascii_case("authorization") {
                    let token = hval
                        .strip_prefix("Bearer ")
                        .or_else(|| hval.strip_prefix("bearer "))
                        .unwrap_or(hval.as_str())
                        .trim();
                    if !token.is_empty() {
                        let env_name = format!(
                            "MYRIAD_MCP_BEARER_{}",
                            key.to_ascii_uppercase().replace('-', "_")
                        );
                        env_vars.push((env_name.clone(), token.to_string()));
                        config.insert(
                            format!("mcp_servers.{key}.bearer_token_env_var"),
                            Value::String(env_name),
                        );
                    }
                    break;
                }
            }
        } else if !spec.command.trim().is_empty() {
            config.insert(
                format!("mcp_servers.{key}.command"),
                Value::String(spec.command.clone()),
            );
            if !spec.args.is_empty() {
                config.insert(format!("mcp_servers.{key}.args"), json!(spec.args));
            }
            if !spec.env.is_empty() {
                let env_obj: serde_json::Map<String, Value> = spec
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                    .collect();
                config.insert(format!("mcp_servers.{key}.env"), Value::Object(env_obj));
            }
        }
    }
    (config, env_vars)
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

/// 入口：尝试通过持久 `codex app-server --stdio` 执行本轮，支持连接复用、交互审批与运行中 `turn/steer`。
pub(crate) async fn run_codex_app_server(
    launch: &Launch<'_>,
    steer_rx: &mut Option<mpsc::UnboundedReceiver<SteerRequest>>,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> CodexAttempt {
    let mut words = split_command(launch.command);
    if words.is_empty() {
        words.push("codex".to_string());
    }
    let program_name = words.remove(0);
    let extra = words;
    if wants_legacy_exec(&extra) {
        return CodexAttempt::FallbackToLegacy;
    }
    let Some(program) = find_program(&program_name, launch.dir) else {
        return CodexAttempt::Finished(Finished {
            end: TurnEnd::Failed(format!(
                "这台电脑上找不到 {program_name}，Codex 没法启动。装好之后再试，或者在 Bot 设置里写上它的完整路径"
            )),
            session: launch.session.map(String::from),
            text: String::new(),
            reasoning: None,
        });
    };

    let mcp_fp = mcp_fingerprint(launch.mcp_servers);
    let pool_key = match (launch.codex_pool, launch.acp_scope) {
        (Some(_), Some(scope)) => Some(CodexKey {
            scope: scope.to_string(),
            command: launch.command.trim().to_string(),
            dir: launch.dir.to_path_buf(),
        }),
        _ => None,
    };

    // 优先复用常驻进程与线程；仅当连接在 `turn/start` 发出前已死时静默冷启动一次。
    if let (Some(pool), Some(key)) = (launch.codex_pool, pool_key.as_ref()) {
        if let Some(conn) = pool.take(key, launch.session, &mcp_fp, launch.permission) {
            match run_codex_turn_on(conn, launch, steer_rx, observer, cancel, true).await {
                CodexTurnOutcome::ReusedDeadBeforeDispatch => {}
                CodexTurnOutcome::Done { finished, keep } => {
                    if let Some(conn) = keep {
                        pool.put(key.clone(), *conn);
                    }
                    return CodexAttempt::Finished(finished);
                }
            }
        }
    }

    let conn = match spawn_codex_session(&program, &extra, launch, &mcp_fp, cancel).await {
        SpawnOutcome::Ready(conn) => *conn,
        SpawnOutcome::FallbackToLegacy => return CodexAttempt::FallbackToLegacy,
        SpawnOutcome::Aborted(end, session) => {
            return CodexAttempt::Finished(Finished {
                end,
                session,
                text: String::new(),
                reasoning: None,
            });
        }
    };

    match run_codex_turn_on(conn, launch, steer_rx, observer, cancel, false).await {
        CodexTurnOutcome::Done { finished, keep } => {
            if let (Some(pool), Some(key), Some(conn)) = (launch.codex_pool, pool_key, keep) {
                pool.put(key, *conn);
            }
            CodexAttempt::Finished(finished)
        }
        CodexTurnOutcome::ReusedDeadBeforeDispatch => CodexAttempt::Finished(Finished {
            end: TurnEnd::Failed("Codex app-server 启动后立即退出了".into()),
            session: launch.session.map(String::from),
            text: String::new(),
            reasoning: None,
        }),
    }
}

enum SpawnOutcome {
    Ready(Box<PooledCodex>),
    FallbackToLegacy,
    Aborted(TurnEnd, Option<String>),
}

enum CodexTurnOutcome {
    /// 复用的连接在写出 `turn/start` 前就已断开，调用方无感冷启动重试。
    ReusedDeadBeforeDispatch,
    Done {
        finished: Finished,
        keep: Option<Box<PooledCodex>>,
    },
}

/// 读取直到收到指定 `id` 的 JSON-RPC 响应（握手阶段使用）。
async fn wait_rpc_response(
    reader: &mut BufReader<tokio::process::ChildStdout>,
    target_id: u64,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Result<Value, (i64, String)>, &'static str> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err("timeout");
        }
        let remain = deadline - now;
        let line = tokio::select! {
            _ = cancel.cancelled() => return Err("cancelled"),
            res = tokio::time::timeout(remain, read_bounded_line(reader)) => match res {
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
            // 握手阶段若 stdout 印出非 JSON（如旧 CLI usage 帮助），跳过或等待退出；若最终 EOF 将自动回退。
            continue;
        };
        if value.get("id").and_then(|v| v.as_u64()) != Some(target_id) {
            continue;
        }
        if let Some(err) = value.get("error").filter(|e| !e.is_null()) {
            let code = err["code"].as_i64().unwrap_or(-32000);
            let msg = err["message"]
                .as_str()
                .unwrap_or("Codex app-server error")
                .to_string();
            return Ok(Err((code, msg)));
        }
        return Ok(Ok(value["result"].clone()));
    }
}

/// 启动 `codex app-server --stdio` 并完成 `initialize` -> `initialized` -> `thread/resume` 或 `thread/start`。
async fn spawn_codex_session(
    program: &std::path::Path,
    extra: &[String],
    launch: &Launch<'_>,
    mcp_fp: &str,
    cancel: &CancellationToken,
) -> SpawnOutcome {
    let session = launch.session.map(String::from);
    let (mcp_config, mcp_env) = build_codex_mcp_config(launch.mcp_servers);
    let mut args = Vec::with_capacity(extra.len() + 2);
    args.push("app-server".to_string());
    args.push("--stdio".to_string());
    args.extend(extra.iter().cloned());

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
    for (k, v) in mcp_env {
        command.env(k, v);
    }

    let mut child = match Tree::spawn(command) {
        Ok(child) => child,
        Err(error) => {
            return SpawnOutcome::Aborted(
                TurnEnd::Failed(format!("Codex 启动不了：{error}")),
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

    // 1. initialize 握手：若不支持 app-server（进程立刻退出、EOF、-32601 或超时），安全回退到 legacy codex exec。
    let mut next_id = 1u64;
    let init_id = next_id;
    next_id += 1;
    let init_req = json!({
        "jsonrpc": "2.0",
        "id": init_id,
        "method": "initialize",
        "params": {
            "clientInfo": {
                "name": "myriad",
                "title": "Myriad",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {
                "experimentalApi": true,
            }
        }
    });
    if write_json_line(&mut stdin, &init_req).await.is_err() {
        child.kill();
        if let Some(h) = stderr {
            h.abort();
        }
        return SpawnOutcome::FallbackToLegacy;
    }

    match wait_rpc_response(&mut reader, init_id, INIT_TIMEOUT, cancel).await {
        Ok(Ok(_)) => {}
        Err("cancelled") => {
            child.kill();
            if let Some(h) = stderr {
                h.abort();
            }
            return SpawnOutcome::Aborted(TurnEnd::Cancelled, session);
        }
        Ok(Err(_)) | Err(_) => {
            child.kill();
            if let Some(h) = stderr {
                h.abort();
            }
            return SpawnOutcome::FallbackToLegacy;
        }
    }

    // 2. initialized 通知
    let _ = write_json_line(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }),
    )
    .await;

    // 3. thread/resume（如有旧会话）-> 失败则自动回退到 thread/start
    let (approval_policy, sandbox) = codex_permission_params(launch.permission);
    let cwd = launch.dir.to_string_lossy().into_owned();
    let mut thread_id: Option<String> = None;
    let mut resumed_existing = false;

    if let Some(prev_id) = launch.session.filter(|s| !s.trim().is_empty()) {
        let resume_id = next_id;
        next_id += 1;
        let mut params = serde_json::Map::new();
        params.insert("threadId".into(), Value::String(prev_id.to_string()));
        params.insert("cwd".into(), Value::String(cwd.clone()));
        params.insert(
            "approvalPolicy".into(),
            Value::String(approval_policy.to_string()),
        );
        params.insert("sandbox".into(), Value::String(sandbox.to_string()));
        params.insert("excludeTurns".into(), Value::Bool(true));
        if !mcp_config.is_empty() {
            params.insert("config".into(), Value::Object(mcp_config.clone()));
        }
        let req = json!({
            "jsonrpc": "2.0",
            "id": resume_id,
            "method": "thread/resume",
            "params": Value::Object(params),
        });
        if write_json_line(&mut stdin, &req).await.is_ok() {
            match wait_rpc_response(&mut reader, resume_id, INIT_TIMEOUT, cancel).await {
                Ok(Ok(res)) => {
                    let id = res["thread"]["id"]
                        .as_str()
                        .or_else(|| res["threadId"].as_str())
                        .filter(|s| !s.is_empty())
                        .unwrap_or(prev_id);
                    thread_id = Some(id.to_string());
                    resumed_existing = true;
                }
                Err("cancelled") => {
                    child.kill();
                    if let Some(h) = stderr {
                        h.abort();
                    }
                    return SpawnOutcome::Aborted(TurnEnd::Cancelled, session);
                }
                // resume 失败（如旧 rollout 损坏或换了 provider）：继续尝试 thread/start 开新线程
                Ok(Err(_)) | Err(_) => {}
            }
        }
    }

    if thread_id.is_none() {
        let start_id = next_id;
        next_id += 1;
        let mut params = serde_json::Map::new();
        params.insert("cwd".into(), Value::String(cwd));
        params.insert(
            "approvalPolicy".into(),
            Value::String(approval_policy.to_string()),
        );
        params.insert("sandbox".into(), Value::String(sandbox.to_string()));
        if !mcp_config.is_empty() {
            params.insert("config".into(), Value::Object(mcp_config));
        }
        let req = json!({
            "jsonrpc": "2.0",
            "id": start_id,
            "method": "thread/start",
            "params": Value::Object(params),
        });
        if let Err(error) = write_json_line(&mut stdin, &req).await {
            child.kill();
            if let Some(h) = stderr {
                h.abort();
            }
            return SpawnOutcome::Aborted(
                TurnEnd::Failed(format!("Codex app-server 创建线程失败：{error}")),
                session,
            );
        }
        match wait_rpc_response(&mut reader, start_id, INIT_TIMEOUT, cancel).await {
            Ok(Ok(res)) => {
                if let Some(id) = res["thread"]["id"]
                    .as_str()
                    .or_else(|| res["threadId"].as_str())
                    .filter(|s| !s.is_empty())
                {
                    thread_id = Some(id.to_string());
                } else {
                    child.kill();
                    if let Some(h) = stderr {
                        h.abort();
                    }
                    return SpawnOutcome::Aborted(
                        TurnEnd::Failed("Codex app-server thread/start 未返回 thread id".into()),
                        session,
                    );
                }
            }
            Err("cancelled") => {
                child.kill();
                if let Some(h) = stderr {
                    h.abort();
                }
                return SpawnOutcome::Aborted(TurnEnd::Cancelled, session);
            }
            Ok(Err((_, msg))) => {
                child.kill();
                let err_tail = collect_stderr(stderr).await;
                let detail = if err_tail.trim().is_empty() {
                    msg
                } else {
                    format!("{msg}\n{}", tail(err_tail.trim(), STDERR_TAIL))
                };
                return SpawnOutcome::Aborted(
                    TurnEnd::Failed(format!("Codex app-server 创建线程失败：{detail}")),
                    session,
                );
            }
            Err(why) => {
                child.kill();
                let err_tail = collect_stderr(stderr).await;
                let detail = if err_tail.trim().is_empty() {
                    why.to_string()
                } else {
                    tail(err_tail.trim(), STDERR_TAIL)
                };
                return SpawnOutcome::Aborted(
                    TurnEnd::Failed(format!("Codex app-server 创建线程失败：{detail}")),
                    session,
                );
            }
        }
    }

    SpawnOutcome::Ready(Box::new(PooledCodex {
        child,
        stdin,
        reader,
        stderr,
        thread_id: thread_id.expect("checked above"),
        next_id,
        mcp_fingerprint: mcp_fp.to_string(),
        permission: launch.permission,
        prev_total: TokenTotals::default(),
        total_initialized: !resumed_existing,
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

/// 从 `item.summary` / `item.content` / `item.text` 提取完整 reasoning 文本。
fn extract_codex_reasoning(item: &Value) -> String {
    let join_arr = |v: &Value| -> String {
        v.as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|part| part.as_str().or_else(|| part["text"].as_str()))
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n\n")
            })
            .unwrap_or_default()
    };
    let summary = join_arr(&item["summary"]);
    if !summary.is_empty() {
        return summary;
    }
    let content = join_arr(&item["content"]);
    if !content.is_empty() {
        return content;
    }
    str_of(item, "text").to_string()
}

fn parse_usage_fields(obj: &Value) -> Option<TokenTotals> {
    if !obj.is_object() {
        return None;
    }
    let input_tokens = obj["inputTokens"]
        .as_u64()
        .or_else(|| obj["input_tokens"].as_u64())
        .unwrap_or(0);
    let cached_tokens = obj["cachedInputTokens"]
        .as_u64()
        .or_else(|| obj["cached_input_tokens"].as_u64())
        .unwrap_or(0);
    let output_tokens = obj["outputTokens"]
        .as_u64()
        .or_else(|| obj["output_tokens"].as_u64())
        .unwrap_or(0);
    let reasoning_output = obj["reasoningOutputTokens"]
        .as_u64()
        .or_else(|| obj["reasoning_output_tokens"].as_u64())
        .unwrap_or(0);
    Some(TokenTotals {
        input_tokens,
        output_tokens: output_tokens.saturating_add(reasoning_output),
        cached_tokens,
    })
}

fn format_approval_summary(method: &str, params: &Value) -> (&'static str, String, String) {
    match method {
        "item/commandExecution/requestApproval" => {
            let cmd = str_of(params, "command").trim();
            let title = if !cmd.is_empty() {
                format!("Codex 请求执行命令：{cmd}")
            } else if let Some(host) = params["networkApprovalContext"]["host"].as_str() {
                format!("Codex 请求访问网络：{host}")
            } else {
                "Codex 请求执行命令".to_string()
            };
            let reason = str_of(params, "reason").trim();
            let reason = if reason.is_empty() {
                "Codex app-server 在当前权限档位下请求你批准执行该命令。".to_string()
            } else {
                reason.to_string()
            };
            ("shell", title, reason)
        }
        "item/fileChange/requestApproval" => {
            let root = str_of(params, "grantRoot").trim();
            let title = if !root.is_empty() {
                format!("Codex 请求修改目录：{root}")
            } else {
                "Codex 请求修改文件".to_string()
            };
            let reason = str_of(params, "reason").trim();
            let reason = if reason.is_empty() {
                "Codex app-server 请求批准应用文件更改。".to_string()
            } else {
                reason.to_string()
            };
            ("edit", title, reason)
        }
        "item/permissions/requestApproval" => {
            let reason = str_of(params, "reason").trim();
            let title = if reason.is_empty() {
                "Codex 请求额外权限".to_string()
            } else {
                format!("Codex 请求权限：{reason}")
            };
            (
                "permission",
                title,
                "Codex app-server 请求授予额外的沙箱或网络权限。".to_string(),
            )
        }
        "mcpServer/elicitation/request" => {
            let server = str_of(params, "serverName");
            let msg = str_of(params, "message");
            let title = if !msg.is_empty() {
                format!("MCP ({server}) 请求确认：{msg}")
            } else {
                format!("MCP ({server}) 请求批准操作")
            };
            (
                "mcp",
                title,
                "外部 MCP 工具通过 Codex 请求人工批准。".to_string(),
            )
        }
        _ => ("harness", "Codex 请求批准".to_string(), String::new()),
    }
}

/// 处理来自 `codex app-server` 的服务端请求（审批、MCP 交互、用户输入等）。
async fn handle_server_request(
    conn: &mut PooledCodex,
    req_id: &Value,
    method: &str,
    params: &Value,
    permission: PermissionMode,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> std::io::Result<()> {
    match method {
        "item/commandExecution/requestApproval"
        | "item/fileChange/requestApproval"
        | "item/permissions/requestApproval"
        | "mcpServer/elicitation/request" => {
            let (tool, title, reason) = format_approval_summary(method, params);
            let call_id = format!(
                "codex-approve-{}",
                params["itemId"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| req_id.to_string())
            );
            let allowed = match permission {
                PermissionMode::Full => true,
                PermissionMode::Plan => {
                    observer.tool_started(&call_id, tool, &title, "").await;
                    observer
                        .tool_finished(
                            &call_id,
                            StepEnd::Stopped,
                            "规划模式（只读）已自动拦截修改/执行请求",
                        )
                        .await;
                    false
                }
                PermissionMode::Ask => {
                    let asked = observer.request_approval(ApprovalRequest {
                        call_id: &call_id,
                        tool,
                        title: &title,
                        reason: &reason,
                        input: params,
                    });
                    tokio::select! {
                        decision = asked => decision,
                        _ = cancel.cancelled() => false,
                    }
                }
            };
            let result = match method {
                "item/permissions/requestApproval" => {
                    if allowed {
                        json!({
                            "permissions": params.get("permissions").cloned().unwrap_or_else(|| json!({})),
                            "scope": "turn",
                            "strictAutoReview": false,
                        })
                    } else {
                        json!({
                            "permissions": {},
                            "scope": "turn",
                            "strictAutoReview": false,
                        })
                    }
                }
                "mcpServer/elicitation/request" => {
                    if allowed {
                        json!({ "action": "accept", "content": {} })
                    } else {
                        json!({ "action": "decline" })
                    }
                }
                _ => {
                    let decision = if allowed {
                        "accept"
                    } else if cancel.is_cancelled() {
                        "cancel"
                    } else {
                        "decline"
                    };
                    json!({ "decision": decision })
                }
            };
            write_json_line(
                &mut conn.stdin,
                &json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": result,
                }),
            )
            .await
        }
        "item/tool/requestUserInput" => {
            // 协议合法的空答案（绝不替用户伪造选项回答）。
            write_json_line(
                &mut conn.stdin,
                &json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": { "answers": {} },
                }),
            )
            .await
        }
        _ => {
            write_json_line(
                &mut conn.stdin,
                &json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {
                        "code": -32601,
                        "message": format!("Myriad does not handle Codex request {method}"),
                    }
                }),
            )
            .await
        }
    }
}

struct PendingSteer {
    step_id: String,
    ack: oneshot::Sender<bool>,
}

/// 在已建立的 `codex app-server` 连接上跑完一轮 `turn/start`，期间实时响应流式事件、交互审批、`turn/steer` 与取消中断。
async fn run_codex_turn_on(
    mut conn: PooledCodex,
    launch: &Launch<'_>,
    steer_rx: &mut Option<mpsc::UnboundedReceiver<SteerRequest>>,
    observer: &dyn Observer,
    cancel: &CancellationToken,
    reused: bool,
) -> CodexTurnOutcome {
    let turn_req_id = conn.next_id;
    conn.next_id += 1;
    let turn_start_req = json!({
        "jsonrpc": "2.0",
        "id": turn_req_id,
        "method": "turn/start",
        "params": {
            "threadId": conn.thread_id,
            "input": [{ "type": "text", "text": launch.prompt }],
        }
    });
    if let Err(error) = write_json_line(&mut conn.stdin, &turn_start_req).await {
        if reused {
            return CodexTurnOutcome::ReusedDeadBeforeDispatch;
        }
        return CodexTurnOutcome::Done {
            finished: Finished {
                end: TurnEnd::Failed(format!("发送 Codex turn/start 失败：{error}")),
                session: Some(conn.thread_id.clone()),
                text: String::new(),
                reasoning: None,
            },
            keep: None,
        };
    }

    let mut active_turn_id: Option<String> = None;
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut streamed_text_items: HashSet<String> = HashSet::new();
    let mut streamed_reasoning_items: HashMap<String, i64> = HashMap::new();
    let mut open_steps: HashMap<String, String> = HashMap::new();
    let mut pending_steers: HashMap<u64, PendingSteer> = HashMap::new();
    let mut steer_seq = 0u64;
    let mut step_seq = 0usize;
    let mut thoughts = 0usize;
    let mut last_error_notice: Option<String> = None;
    let mut clean_for_pool = false;
    let mut turn_usage_emitted = false;
    let mut last_seen_fallback_usage: Option<TokenTotals> = None;

    let end = loop {
        let can_steer = active_turn_id.is_some() && steer_rx.is_some();
        tokio::select! {
            _ = cancel.cancelled() => {
                if let Some(tid) = active_turn_id.as_deref() {
                    let int_id = conn.next_id;
                    conn.next_id += 1;
                    let _ = write_json_line(&mut conn.stdin, &json!({
                        "jsonrpc": "2.0",
                        "id": int_id,
                        "method": "turn/interrupt",
                        "params": {
                            "threadId": conn.thread_id,
                            "turnId": tid,
                        }
                    })).await;
                    // 短暂等待 Codex 完成中断收尾，随后释放进程。
                    let deadline = tokio::time::Instant::now() + INTERRUPT_GRACE;
                    while tokio::time::Instant::now() < deadline {
                        let remain = deadline - tokio::time::Instant::now();
                        match tokio::time::timeout(remain, read_bounded_line(&mut conn.reader)).await {
                            Ok(Ok(Some(line))) => {
                                if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
                                    if str_of(&v, "method") == "turn/completed" {
                                        break;
                                    }
                                }
                            }
                            _ => break,
                        }
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
                        let Some(tid) = active_turn_id.as_deref() else {
                            let _ = req.ack.send(false);
                            continue;
                        };
                        let rpc_id = conn.next_id;
                        conn.next_id += 1;
                        steer_seq += 1;
                        let step_id = format!("codex-steer-{steer_seq}");
                        let preview: String = req.text.chars().take(64).collect();
                        observer
                            .tool_started(&step_id, "steer", &format!("运行中插话：{preview}"), &req.text)
                            .await;
                        let payload = json!({
                            "jsonrpc": "2.0",
                            "id": rpc_id,
                            "method": "turn/steer",
                            "params": {
                                "threadId": conn.thread_id,
                                "expectedTurnId": tid,
                                "input": [{ "type": "text", "text": req.text }],
                            }
                        });
                        if write_json_line(&mut conn.stdin, &payload).await.is_ok() {
                            pending_steers.insert(rpc_id, PendingSteer { step_id, ack: req.ack });
                        } else {
                            observer
                                .tool_finished(&step_id, StepEnd::Stopped, "Codex 连接已关闭，未注入")
                                .await;
                            let _ = req.ack.send(false);
                        }
                    }
                    None => {
                        *steer_rx = None;
                    }
                }
            }
            line_res = tokio::time::timeout(IDLE_TIMEOUT, read_bounded_line(&mut conn.reader)) => {
                let line = match line_res {
                    Ok(Ok(Some(line))) => line,
                    Ok(Ok(None)) => {
                        let err_tail = collect_stderr(conn.stderr.take()).await;
                        let detail = if let Some(msg) = last_error_notice.take() {
                            msg
                        } else if !err_tail.trim().is_empty() {
                            tail(err_tail.trim(), STDERR_TAIL)
                        } else {
                            "Codex app-server 在本轮结束前意外退出了".to_string()
                        };
                        break TurnEnd::Failed(detail);
                    }
                    Ok(Err(error)) => {
                        break TurnEnd::Failed(format!("读取 Codex app-server 输出失败：{error}"));
                    }
                    Err(_) => {
                        break TurnEnd::Failed(format!(
                            "Codex 超过 {} 分钟没有任何输出，已自动停止",
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

                // 1. JSON-RPC 响应（匹配 turn/start 或 turn/steer）
                if let Some(id_val) = value.get("id").filter(|v| !v.is_null()) {
                    if value.get("method").is_none() {
                        if let Some(id_u64) = id_val.as_u64() {
                            if id_u64 == turn_req_id {
                                if let Some(err) = value.get("error").filter(|e| !e.is_null()) {
                                    let msg = err["message"]
                                        .as_str()
                                        .unwrap_or("Codex turn/start failed")
                                        .to_string();
                                    break TurnEnd::Failed(msg);
                                }
                                if let Some(tid) = value["result"]["turn"]["id"]
                                    .as_str()
                                    .or_else(|| value["result"]["turnId"].as_str())
                                    .filter(|s| !s.is_empty())
                                {
                                    active_turn_id = Some(tid.to_string());
                                }
                            } else if let Some(pending) = pending_steers.remove(&id_u64) {
                                if let Some(err) = value.get("error").filter(|e| !e.is_null()) {
                                    let msg = err["message"]
                                        .as_str()
                                        .unwrap_or("当前轮次无法插话");
                                    observer
                                        .tool_finished(&pending.step_id, StepEnd::Stopped, msg)
                                        .await;
                                    let _ = pending.ack.send(false);
                                } else {
                                    observer
                                        .tool_finished(
                                            &pending.step_id,
                                            StepEnd::Ok,
                                            "已实时注入当前 Codex 轮次",
                                        )
                                        .await;
                                    let _ = pending.ack.send(true);
                                }
                            }
                        }
                        continue;
                    }

                    // 2. 服务端发起的双向请求（审批 / 权限 / 用户输入）
                    let method = str_of(&value, "method");
                    let params = &value["params"];
                    if handle_server_request(
                        &mut conn,
                        id_val,
                        method,
                        params,
                        launch.permission,
                        observer,
                        cancel,
                    )
                    .await
                    .is_err()
                    {
                        break TurnEnd::Failed("向 Codex app-server 回复审批结果失败".into());
                    }
                    continue;
                }

                // 3. 服务端通知（流式文本、思考、工具步骤、用量、轮次起止）
                let method = str_of(&value, "method");
                let params = &value["params"];
                if let Some(tid) = params["threadId"].as_str().filter(|s| !s.is_empty()) {
                    if tid != conn.thread_id {
                        continue;
                    }
                }
                match method {
                    "turn/started" => {
                        if let Some(tid) = params["turn"]["id"]
                            .as_str()
                            .or_else(|| params["turnId"].as_str())
                            .filter(|s| !s.is_empty())
                        {
                            active_turn_id = Some(tid.to_string());
                        }
                    }
                    "item/agentMessage/delta" => {
                        let item_id = str_of(params, "itemId").to_string();
                        if !item_id.is_empty() {
                            streamed_text_items.insert(item_id);
                        }
                        let delta = str_of(params, "delta");
                        if !delta.is_empty() {
                            text.push_str(delta);
                            observer.text_delta(delta).await;
                        }
                    }
                    "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => {
                        let item_id = str_of(params, "itemId").to_string();
                        let part = params["summaryIndex"]
                            .as_i64()
                            .or_else(|| params["contentIndex"].as_i64())
                            .unwrap_or(0);
                        let sep = if !item_id.is_empty() {
                            let prev = streamed_reasoning_items.insert(item_id, part);
                            prev.is_some_and(|p| p != part)
                        } else {
                            false
                        };
                        let delta = str_of(params, "delta");
                        if !delta.is_empty() {
                            if sep && !reasoning.is_empty() {
                                reasoning.push_str("\n\n");
                            }
                            reasoning.push_str(delta);
                        }
                    }
                    "item/plan/delta" => {
                        let delta = str_of(params, "delta");
                        if !delta.is_empty() {
                            reasoning.push_str(delta);
                        }
                    }
                    "item/started" => {
                        let item = &params["item"];
                        if str_of(item, "type") == "userMessage" {
                            continue;
                        }
                        if let Some((tool, title)) = codex_item(item) {
                            let raw_id = str_of(item, "id").to_string();
                            step_seq += 1;
                            let step_key = format!("c{step_seq}-{raw_id}");
                            open_steps.insert(raw_id, step_key.clone());
                            observer.tool_started(&step_key, &tool, &title, "").await;
                        }
                    }
                    "item/completed" => {
                        let item = &params["item"];
                        let item_type = str_of(item, "type");
                        let raw_id = str_of(item, "id").to_string();
                        if item_type == "agentMessage" || item_type == "agent_message" {
                            if !streamed_text_items.contains(&raw_id) {
                                let chunk = str_of(item, "text");
                                if !chunk.is_empty() {
                                    let piece = if text.is_empty() {
                                        chunk.to_string()
                                    } else {
                                        format!("\n\n{chunk}")
                                    };
                                    text.push_str(&piece);
                                    observer.text_delta(&piece).await;
                                }
                            }
                            continue;
                        }
                        if item_type == "reasoning" {
                            if !streamed_reasoning_items.contains_key(&raw_id) {
                                let chunk = extract_codex_reasoning(item);
                                if !chunk.is_empty() {
                                    if !reasoning.is_empty() {
                                        reasoning.push('\n');
                                    }
                                    reasoning.push_str(&chunk);
                                    if chunk.chars().count() >= 12 {
                                        thoughts += 1;
                                        let key = format!("think-{thoughts}");
                                        observer.tool_started(&key, "think", "思考", "").await;
                                        observer.tool_finished(&key, StepEnd::Ok, &chunk).await;
                                    }
                                }
                            }
                            continue;
                        }
                        if let Some((tool, title)) = codex_item(item) {
                            let step_key = match open_steps.remove(&raw_id) {
                                Some(k) => k,
                                None => {
                                    step_seq += 1;
                                    let k = format!("c{step_seq}-{raw_id}");
                                    observer.tool_started(&k, &tool, &title, "").await;
                                    k
                                }
                            };
                            let status = str_of(item, "status");
                            let has_error = item.get("error").is_some_and(|e| !e.is_null());
                            let ok = !has_error
                                && !matches!(status, "failed" | "error" | "declined" | "cancelled")
                                && match item["exitCode"]
                                    .as_i64()
                                    .or_else(|| item["exit_code"].as_i64())
                                {
                                    Some(code) => code == 0,
                                    None => true,
                                };
                            let output = [
                                str_of(item, "aggregatedOutput"),
                                str_of(item, "aggregated_output"),
                                str_of(item, "output"),
                                str_of(item, "summary"),
                                str_of(&item["result"], "content"),
                                str_of(&item["error"], "message"),
                            ]
                            .into_iter()
                            .find(|s| !s.is_empty())
                            .map(String::from)
                            .unwrap_or_else(|| {
                                if has_error {
                                    item["error"].to_string()
                                } else {
                                    String::new()
                                }
                            });
                            let end = if ok { StepEnd::Ok } else { StepEnd::Failed };
                            observer.tool_finished(&step_key, end, &output).await;
                        }
                    }
                    "thread/tokenUsage/updated" => {
                        let tu = &params["tokenUsage"];
                        if let Some(total) = parse_usage_fields(&tu["total"]) {
                            let (delta_in, delta_out, delta_cached) = if !conn.total_initialized {
                                // `thread/resume` 后首次收到累计值：用 `last` 作为本次增量并同步累计基线，防止重算历史轮次。
                                conn.total_initialized = true;
                                conn.prev_total = total;
                                match parse_usage_fields(&tu["last"]) {
                                    Some(last) => (
                                        last.input_tokens,
                                        last.output_tokens,
                                        last.cached_tokens,
                                    ),
                                    None => (0, 0, 0),
                                }
                            } else {
                                let d_in = total
                                    .input_tokens
                                    .saturating_sub(conn.prev_total.input_tokens);
                                let d_out = total
                                    .output_tokens
                                    .saturating_sub(conn.prev_total.output_tokens);
                                let d_cached = total
                                    .cached_tokens
                                    .saturating_sub(conn.prev_total.cached_tokens);
                                conn.prev_total = total;
                                (d_in, d_out, d_cached)
                            };
                            if delta_in + delta_out + delta_cached > 0 {
                                turn_usage_emitted = true;
                                observer
                                    .usage(
                                        "codex",
                                        Usage {
                                            input_tokens: delta_in,
                                            output_tokens: delta_out,
                                            cached_tokens: delta_cached,
                                            cache_write_tokens: 0,
                                        },
                                    )
                                    .await;
                            }
                        } else if let Some(last) = parse_usage_fields(&tu["last"]) {
                            last_seen_fallback_usage = Some(last);
                        }
                    }
                    "error" => {
                        let will_retry = params["willRetry"].as_bool().unwrap_or(false);
                        if !will_retry {
                            let msg = params["error"]["message"]
                                .as_str()
                                .or_else(|| params["message"].as_str())
                                .unwrap_or("Codex app-server error");
                            last_error_notice = Some(msg.to_string());
                        }
                    }
                    "turn/completed" => {
                        let done_turn = &params["turn"];
                        if let (Some(active), Some(done_id)) = (
                            active_turn_id.as_deref(),
                            done_turn["id"].as_str().filter(|s| !s.is_empty()),
                        ) {
                            if active != done_id {
                                continue;
                            }
                        }
                        let status = str_of(done_turn, "status");
                        match status {
                            "" | "completed" => {
                                clean_for_pool = true;
                                break TurnEnd::Completed;
                            }
                            "interrupted" => {
                                break TurnEnd::Cancelled;
                            }
                            "failed" => {
                                let msg = done_turn["error"]["message"]
                                    .as_str()
                                    .or(last_error_notice.as_deref())
                                    .unwrap_or("Codex 执行失败")
                                    .to_string();
                                break TurnEnd::Failed(msg);
                            }
                            other => {
                                break TurnEnd::Failed(format!(
                                    "Codex 轮次状态异常：{other}"
                                ));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    };

    // 兜底：若服务只推送了 tokenUsage.last 未推 total，在轮次结束时上报一次用量。
    if !turn_usage_emitted {
        if let Some(last) = last_seen_fallback_usage {
            if last.input_tokens + last.output_tokens + last.cached_tokens > 0 {
                observer
                    .usage(
                        "codex",
                        Usage {
                            input_tokens: last.input_tokens,
                            output_tokens: last.output_tokens,
                            cached_tokens: last.cached_tokens,
                            cache_write_tokens: 0,
                        },
                    )
                    .await;
            }
        }
    }

    // 任何还没收到回包的运行中插话通知调用方“未能注入”，让 room.rs 保留未读游标留待下轮补答。
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

    let session_id = conn.thread_id.clone();
    let reasoning_out = (!reasoning.trim().is_empty()).then_some(reasoning);

    let keep = if clean_for_pool && launch.codex_pool.is_some() && launch.acp_scope.is_some() {
        Some(Box::new(conn))
    } else {
        drop(conn);
        None
    };

    CodexTurnOutcome::Done {
        finished: Finished {
            end,
            session: Some(session_id),
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
    fn legacy_exec_detection_respects_explicit_exec_flags() {
        assert!(!wants_legacy_exec(&[]));
        assert!(!wants_legacy_exec(&["-m".into(), "o3".into()]));
        assert!(wants_legacy_exec(&["exec".into(), "--json".into()]));
        assert!(wants_legacy_exec(&["--full-auto".into()]));
        assert!(wants_legacy_exec(&[
            "--dangerously-bypass-approvals-and-sandbox".into()
        ]));
    }

    #[test]
    fn permission_mapping_covers_ask_full_and_plan() {
        assert_eq!(
            codex_permission_params(PermissionMode::Full),
            ("never", "danger-full-access")
        );
        assert_eq!(
            codex_permission_params(PermissionMode::Plan),
            ("never", "read-only")
        );
        assert_eq!(
            codex_permission_params(PermissionMode::Ask),
            ("on-request", "workspace-write")
        );
    }

    #[test]
    fn mcp_config_builds_dotted_keys_and_bearer_env() {
        let specs = vec![
            McpSpec {
                name: "wiki".into(),
                url: Some("https://wiki.example/mcp".into()),
                headers: vec![("Authorization".into(), "Bearer secret-tok".into())],
                ..Default::default()
            },
            McpSpec {
                name: "local-fs".into(),
                command: "npx".into(),
                args: vec!["-y".into(), "mcp-fs".into()],
                ..Default::default()
            },
        ];
        let (cfg, env_vars) = build_codex_mcp_config(&specs);
        assert_eq!(cfg["mcp_servers.wiki.url"], "https://wiki.example/mcp");
        assert_eq!(
            cfg["mcp_servers.wiki.bearer_token_env_var"],
            "MYRIAD_MCP_BEARER_WIKI"
        );
        assert_eq!(
            env_vars,
            vec![("MYRIAD_MCP_BEARER_WIKI".into(), "secret-tok".into())]
        );
        assert_eq!(cfg["mcp_servers.local-fs.command"], "npx");
        assert_eq!(cfg["mcp_servers.local-fs.args"], json!(["-y", "mcp-fs"]));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn codex_app_server_reuses_pool_steers_live_approves_interrupts_and_falls_back() {
        use crate::harness::{run, run_interactive};
        use sa_protocol::Harness;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Mutex;

        #[derive(Default)]
        struct RecordingObserver {
            approve_response: bool,
            approvals: Mutex<Vec<String>>,
            usages: Mutex<Vec<Usage>>,
            steps: Mutex<Vec<(String, StepEnd, String)>>,
        }

        #[async_trait::async_trait]
        impl Observer for RecordingObserver {
            async fn text_delta(&self, _: &str) {}
            async fn tool_started(&self, _: &str, _: &str, _: &str, _: &str) {}
            async fn tool_finished(&self, id: &str, end: StepEnd, output: &str) {
                self.steps
                    .lock()
                    .unwrap()
                    .push((id.to_string(), end, output.to_string()));
            }
            async fn usage(&self, _: &str, usage: Usage) {
                self.usages.lock().unwrap().push(usage);
            }
            async fn request_approval(&self, req: ApprovalRequest<'_>) -> bool {
                self.approvals.lock().unwrap().push(req.title.to_string());
                self.approve_response
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-codex.py");
        std::fs::write(
            &script,
            r#"#!/usr/bin/env python3
import json, os, sys

# Mock codex CLI supporting BOTH `app-server --stdio` and legacy `exec --json`.
if len(sys.argv) >= 2 and sys.argv[1] == "exec":
    prompt = sys.stdin.read().strip()
    print(json.dumps({"type": "thread.started", "thread_id": "legacy-thr"}), flush=True)
    print(json.dumps({"type": "item.completed", "item": {"id": "m1", "type": "agent_message", "text": f"legacy:{prompt}"}}), flush=True)
    sys.exit(0)

if len(sys.argv) < 2 or sys.argv[1] != "app-server":
    sys.stderr.write("unsupported subcommand\n")
    sys.exit(2)

pid = os.getpid()
turn_n = 0
total_in = 0
total_out = 0
thread_id = f"thr-{pid}"
sandbox = "workspace-write"
approval_policy = "on-request"

def send(obj):
    print(json.dumps(obj), flush=True)

for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    msg = json.loads(raw)
    method = msg.get("method")
    mid = msg.get("id")
    params = msg.get("params") or {}
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"userAgent": "codex/0.169.0"}})
    elif method == "initialized":
        continue
    elif method in ("thread/start", "thread/resume"):
        sandbox = params.get("sandbox", sandbox)
        approval_policy = params.get("approvalPolicy", approval_policy)
        if method == "thread/resume" and params.get("threadId"):
            thread_id = params["threadId"]
        send({"jsonrpc": "2.0", "id": mid, "result": {"thread": {"id": thread_id}}})
    elif method == "turn/start":
        turn_n += 1
        tid = f"turn-{turn_n}"
        prompt_text = params["input"][0]["text"]
        send({"jsonrpc": "2.0", "id": mid, "result": {"turn": {"id": tid}}})
        send({"jsonrpc": "2.0", "method": "turn/started", "params": {"threadId": thread_id, "turn": {"id": tid}}})

        if "wait-steer" in prompt_text:
            send({"jsonrpc": "2.0", "method": "item/agentMessage/delta", "params": {"threadId": thread_id, "itemId": "am1", "delta": "working..."}})
            # Block until turn/steer arrives!
            for raw2 in sys.stdin:
                m2 = json.loads(raw2.strip())
                if m2.get("method") == "turn/steer":
                    assert m2["params"]["threadId"] == thread_id
                    assert m2["params"]["expectedTurnId"] == tid
                    steered = m2["params"]["input"][0]["text"]
                    send({"jsonrpc": "2.0", "id": m2["id"], "result": {}})
                    send({"jsonrpc": "2.0", "method": "item/agentMessage/delta", "params": {"threadId": thread_id, "itemId": "am1", "delta": f"|steered:{steered}"}})
                    break
        elif "wait-interrupt" in prompt_text:
            send({"jsonrpc": "2.0", "method": "item/agentMessage/delta", "params": {"threadId": thread_id, "itemId": "am1", "delta": "long-task..."}})
            for raw2 in sys.stdin:
                m2 = json.loads(raw2.strip())
                if m2.get("method") == "turn/interrupt":
                    send({"jsonrpc": "2.0", "id": m2["id"], "result": {}})
                    send({"jsonrpc": "2.0", "method": "turn/completed", "params": {"threadId": thread_id, "turn": {"id": tid, "status": "interrupted"}}})
                    break
            continue
        elif "ask-cmd" in prompt_text:
            send({
                "jsonrpc": "2.0",
                "id": 777,
                "method": "item/commandExecution/requestApproval",
                "params": {"itemId": "cmd1", "command": "rm -rf /tmp/demo", "reason": "cleanup"}
            })
            resp = json.loads(sys.stdin.readline())
            decision = resp["result"]["decision"]
            send({"jsonrpc": "2.0", "method": "item/agentMessage/delta", "params": {"threadId": thread_id, "itemId": "am1", "delta": f"pid={pid},turn={turn_n},decision={decision},sandbox={sandbox}"}})
        else:
            send({"jsonrpc": "2.0", "method": "item/agentMessage/delta", "params": {"threadId": thread_id, "itemId": "am1", "delta": f"pid={pid},turn={turn_n},sandbox={sandbox},policy={approval_policy}"}})

        total_in += 100
        total_out += 25
        send({
            "jsonrpc": "2.0",
            "method": "thread/tokenUsage/updated",
            "params": {
                "threadId": thread_id,
                "tokenUsage": {
                    "total": {"inputTokens": total_in, "outputTokens": total_out, "cachedInputTokens": 10},
                    "last": {"inputTokens": 100, "outputTokens": 25, "cachedInputTokens": 10},
                }
            }
        })
        send({"jsonrpc": "2.0", "method": "turn/completed", "params": {"threadId": thread_id, "turn": {"id": tid, "status": "completed"}}})
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let pool = CodexPool::default();
        let cmd = script.display().to_string();
        let obs = RecordingObserver {
            approve_response: true,
            ..Default::default()
        };

        // 1. 第一轮冷启动 + 第二轮复用常驻进程（PID 相同、turn 递增、用量按增量上报）。
        let first = run(
            &Launch {
                harness: Harness::Codex,
                command: &cmd,
                dir: dir.path(),
                prompt: "hello",
                session: None,
                permission: PermissionMode::Ask,
                mcp_servers: &[],
                acp_pool: None,
                codex_pool: Some(&pool),
                acp_scope: Some("room:r1:bot:codex"),
            },
            &obs,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(first.end, TurnEnd::Completed);
        assert_eq!(pool.len(), 1);
        assert!(
            first
                .text
                .contains("turn=1,sandbox=workspace-write,policy=on-request"),
            "{}",
            first.text
        );

        let second = run(
            &Launch {
                harness: Harness::Codex,
                command: &cmd,
                dir: dir.path(),
                prompt: "ask-cmd",
                session: first.session.as_deref(),
                permission: PermissionMode::Ask,
                mcp_servers: &[],
                acp_pool: None,
                codex_pool: Some(&pool),
                acp_scope: Some("room:r1:bot:codex"),
            },
            &obs,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(second.end, TurnEnd::Completed);
        assert_eq!(second.session, first.session);
        assert!(
            second.text.contains("turn=2,decision=accept"),
            "{}",
            second.text
        );
        assert_eq!(obs.approvals.lock().unwrap().len(), 1);
        let usages = obs.usages.lock().unwrap().clone();
        assert_eq!(usages.len(), 2);
        assert_eq!(usages[0].input_tokens, 100);
        assert_eq!(usages[1].input_tokens, 100); // 增量而非累计 200

        // 2. 切到 Plan 模式时不会复用旧 Ask 权限连接，且自动拦截审批请求。
        let plan_obs = RecordingObserver {
            approve_response: true,
            ..Default::default()
        };
        let planned = run(
            &Launch {
                harness: Harness::Codex,
                command: &cmd,
                dir: dir.path(),
                prompt: "ask-cmd",
                session: second.session.as_deref(),
                permission: PermissionMode::Plan,
                mcp_servers: &[],
                acp_pool: None,
                codex_pool: Some(&pool),
                acp_scope: Some("room:r1:bot:codex"),
            },
            &plan_obs,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(planned.end, TurnEnd::Completed);
        assert!(
            planned
                .text
                .contains("turn=1,decision=decline,sandbox=read-only"),
            "{}",
            planned.text
        );
        assert!(plan_obs.approvals.lock().unwrap().is_empty());

        // 3. 运行中实时插话（turn/steer）：在轮次执行中途发送 SteerRequest，验证 ack=true 且输出包含插话内容。
        let (steer_tx, steer_rx) = mpsc::unbounded_channel();
        let (ack_tx, ack_rx) = oneshot::channel();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            let _ = steer_tx.send(SteerRequest {
                text: "change-direction-now".into(),
                ack: ack_tx,
            });
        });
        let steered = run_interactive(
            &Launch {
                harness: Harness::Codex,
                command: &cmd,
                dir: dir.path(),
                prompt: "wait-steer",
                session: planned.session.as_deref(),
                permission: PermissionMode::Plan,
                mcp_servers: &[],
                acp_pool: None,
                codex_pool: Some(&pool),
                acp_scope: Some("room:r1:bot:codex"),
            },
            Some(steer_rx),
            &obs,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(steered.end, TurnEnd::Completed);
        assert!(ack_rx.await.unwrap());
        assert_eq!(steered.text, "working...|steered:change-direction-now");

        // 4. 取消时发送 turn/interrupt 并优雅结束，同时不归还已中断连接到池中。
        let cancel = CancellationToken::new();
        let cancel_trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            cancel_trigger.cancel();
        });
        let interrupted = run(
            &Launch {
                harness: Harness::Codex,
                command: &cmd,
                dir: dir.path(),
                prompt: "wait-interrupt",
                session: steered.session.as_deref(),
                permission: PermissionMode::Plan,
                mcp_servers: &[],
                acp_pool: None,
                codex_pool: Some(&pool),
                acp_scope: Some("room:r1:bot:codex"),
            },
            &obs,
            &cancel,
        )
        .await;
        assert_eq!(interrupted.end, TurnEnd::Cancelled);
        assert_eq!(pool.len(), 0);

        // 5. 旧版本 Codex（不支持 app-server）在握手阶段失败后无感回退 legacy `codex exec --json`。
        let legacy_script = dir.path().join("legacy-codex.py");
        std::fs::write(
            &legacy_script,
            r#"#!/usr/bin/env python3
import json, sys
if len(sys.argv) >= 2 and sys.argv[1] == "app-server":
    sys.stderr.write("error: unrecognized subcommand 'app-server'\n")
    sys.exit(2)
prompt = sys.stdin.read().strip()
print(json.dumps({"type": "thread.started", "thread_id": "old-thr"}), flush=True)
print(json.dumps({"type": "item.completed", "item": {"id": "m1", "type": "agent_message", "text": f"fallback:{prompt}"}}), flush=True)
"#,
        )
        .unwrap();
        std::fs::set_permissions(&legacy_script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let fallback = run(
            &Launch {
                harness: Harness::Codex,
                command: &legacy_script.display().to_string(),
                dir: dir.path(),
                prompt: "still-works",
                session: None,
                permission: PermissionMode::Ask,
                mcp_servers: &[],
                acp_pool: None,
                codex_pool: Some(&pool),
                acp_scope: Some("room:r1:bot:legacy"),
            },
            &obs,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(fallback.end, TurnEnd::Completed);
        assert_eq!(fallback.session.as_deref(), Some("old-thr"));
        assert_eq!(fallback.text, "fallback:still-works");

        pool.evict_all();
        assert_eq!(pool.len(), 0);
    }
}
