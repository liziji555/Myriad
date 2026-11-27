//! harness：本机装好的 Agent 命令行（Claude Code、Codex、OpenCode、Gemini CLI、Kimi Code、
//! 通用 ACP Agent，或者自己写的命令）。
//!
//! Myriad 在 Bot 的工位目录里启动它，把这一轮的话交给它，一行一行读它的 JSON / ACP 输出，
//! 转成和内置 Bot 一样的文字、思考、步骤和用量。点停止时连它开出来的子进程一起结束。

use sa_kernel::{ApprovalRequest, Observer, StepEnd, TurnEnd};
use sa_llm::Usage;
use sa_protocol::{Harness, HarnessProbe, PermissionMode};
use sa_tools::mcp::McpSpec;
use sa_tools::process::Tree;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// 持久 ACP 会话池容量上限（超出时按最久未使用淘汰并清理进程树）。
const ACP_POOL_MAX: usize = 16;
/// 持久 ACP 会话空闲超时时间：超过后自动回收，避免闲置子进程长期占内存。
const ACP_IDLE_TTL: Duration = Duration::from_secs(15 * 60);

/// 房间工位对应的持久 ACP 会话作用域标识。
pub(crate) fn acp_scope(room: &sa_protocol::RoomId, bot: &sa_protocol::BotId) -> String {
    format!("room:{room}:bot:{bot}")
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AcpKey {
    scope: String,
    command: String,
    dir: PathBuf,
}

struct PooledAcp {
    child: Tree,
    stdin: tokio::process::ChildStdin,
    reader: BufReader<tokio::process::ChildStdout>,
    stderr: Option<tokio::task::JoinHandle<String>>,
    session_id: String,
    next_id: u64,
    mcp_fingerprint: String,
    available_modes: Vec<String>,
    current_mode: Option<String>,
    last_used: Instant,
}

impl Drop for PooledAcp {
    fn drop(&mut self) {
        self.child.kill();
        if let Some(handle) = self.stderr.take() {
            handle.abort();
        }
    }
}

/// 持久 ACP 进程池：同一房间工位的 ACP Agent 跨轮保持常驻进程与内存会话，免去每轮冷启动。
#[derive(Default)]
pub(crate) struct AcpPool {
    entries: std::sync::Arc<std::sync::Mutex<HashMap<AcpKey, PooledAcp>>>,
}

impl AcpPool {
    fn take(
        &self,
        key: &AcpKey,
        expected_session: Option<&str>,
        mcp_fingerprint: &str,
    ) -> Option<PooledAcp> {
        let mut stale = Vec::new();
        let taken = {
            let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            let expired_keys: Vec<AcpKey> = map
                .iter()
                .filter(|(_, v)| v.last_used.elapsed() > ACP_IDLE_TTL)
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
            && conn.mcp_fingerprint == mcp_fingerprint
            && expected_session == Some(conn.session_id.as_str());
        if valid {
            Some(conn)
        } else {
            drop(conn);
            None
        }
    }

    fn put(&self, key: AcpKey, conn: PooledAcp) {
        self.put_with_ttl(key, conn, ACP_IDLE_TTL);
    }

    fn put_with_ttl(&self, key: AcpKey, mut conn: PooledAcp, ttl: Duration) {
        let now = Instant::now();
        conn.last_used = now;
        let mut evicted = Vec::new();
        {
            let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            let expired_keys: Vec<AcpKey> = map
                .iter()
                .filter(|(_, v)| v.last_used.elapsed() > ttl)
                .map(|(k, _)| k.clone())
                .collect();
            for k in expired_keys {
                if let Some(v) = map.remove(&k) {
                    evicted.push(v);
                }
            }
            while map.len() >= ACP_POOL_MAX {
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
        let weak = std::sync::Arc::downgrade(&self.entries);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                tokio::time::sleep(ttl).await;
                let Some(entries) = weak.upgrade() else {
                    return;
                };
                let expired = {
                    let mut map = entries.lock().unwrap_or_else(|e| e.into_inner());
                    if map.get(&key).is_some_and(|v| v.last_used == now) {
                        map.remove(&key)
                    } else {
                        None
                    }
                };
                drop(expired);
            });
        }
    }

    /// 驱逐指定作用域（例如重置工位会话、关闭工位）的全部常驻 ACP 进程。
    pub(crate) fn evict_scope(&self, scope: &str) {
        let mut evicted = Vec::new();
        {
            let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            let keys: Vec<AcpKey> = map.keys().filter(|k| k.scope == scope).cloned().collect();
            for k in keys {
                if let Some(v) = map.remove(&k) {
                    evicted.push(v);
                }
            }
        }
        drop(evicted);
    }

    /// 驱逐所有常驻 ACP 进程（服务关闭或 MCP 热重载时调用）。
    pub(crate) fn evict_all(&self) {
        let evicted: Vec<PooledAcp> = {
            let mut map = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            map.drain().map(|(_, v)| v).collect()
        };
        drop(evicted);
    }

    #[cfg(all(test, unix))]
    fn len(&self) -> usize {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

pub(crate) fn mcp_fingerprint(specs: &[McpSpec]) -> String {
    serde_json::to_string(&acp_mcp_servers(specs)).unwrap_or_default()
}

/// 出错时最多带上这么多字的标准错误输出。
pub(crate) const STDERR_TAIL: usize = 2000;
/// harness 退出后，再等它剩下的输出多久。
const DRAIN: Duration = Duration::from_secs(1);
/// 长时间没有任何输出时自动中止，防止 CLI 卡死在交互提示上。
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// 探测本机已安装 CLI 版本时的超时时间。
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// 启动一次 harness 要的东西。
pub(crate) struct Launch<'a> {
    pub harness: Harness,
    /// Bot 上写的命令。空的就用默认程序。
    pub command: &'a str,
    pub dir: &'a Path,
    pub prompt: &'a str,
    /// 上次的会话号，有的话接着聊。
    pub session: Option<&'a str>,
    /// 房间的权限档位：完全权限时透传给支持的 CLI（跳过二次审批）。
    pub permission: PermissionMode,
    /// Myriad 已配置的 MCP 服务：按各 CLI 支持的格式自动注入（Claude `--mcp-config`、Codex `-c`、ACP `mcpServers`）。
    pub mcp_servers: &'a [McpSpec],
    /// 可选的持久 ACP 进程池：传入时复用同工位的常驻 ACP 进程，免去每轮冷启动。
    pub acp_pool: Option<&'a AcpPool>,
    /// 可选的持久 Codex app-server 进程池：传入时跨轮复用同工位 Codex 进程与线程，并支持运行中 `turn/steer`。
    pub codex_pool: Option<&'a crate::codex_app_server::CodexPool>,
    /// 可选的持久 Claude Code 流式进程池：传入时跨轮复用同工位常驻 Claude Code 进程，并支持运行中插话与双向审批。
    pub claude_pool: Option<&'a crate::claude_stream::ClaudePool>,
    /// 与 `acp_pool` / `codex_pool` / `claude_pool` 配对的作用域键（如 `room:{id}:bot:{id}`）。
    pub acp_scope: Option<&'a str>,
    /// 调用方观察者是否能弹出并回传交互式审批卡（本地 `RoomObserver` 为 `true`，`sa share` 远程转发暂为 `false`）。
    pub interactive_approvals: bool,
}

/// 运行中插话请求：由 `room.rs` 在收到新消息时发送，由持久双向 harness（Codex / Claude Code）实时注入活跃轮次。
/// `ack` 为 `true` 表示已被对方确认接收，`false`（或通道关闭）表示未能注入，调用方保留未读游标留待下轮处理。
pub(crate) struct SteerRequest {
    pub text: String,
    pub ack: tokio::sync::oneshot::Sender<bool>,
}

/// 可在 `tokio::select!` 中安全取消的行缓冲状态：被其它分支打断时完整保留半行字节与截断标记，绝不丢帧。
#[derive(Default)]
pub(crate) struct LineBuffer {
    buf: Vec<u8>,
    capped: bool,
}

static TEMP_MCP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 临时文件守卫：单轮或常驻 CLI 生命周期结束后自动清理临时 `--mcp-config` JSON 文件。
pub(crate) struct TempFileGuard(pub(crate) Option<PathBuf>);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

pub(crate) struct Finished {
    pub end: TurnEnd,
    /// harness 这次报的会话号。下次接着用。
    pub session: Option<String>,
    /// 它说的话（不含步骤）。
    pub text: String,
    /// 它的思考过程（若 CLI 输出了 thinking / reasoning）。
    pub reasoning: Option<String>,
}

/// harness 输出里的一件事。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Event {
    Session(String),
    /// 完整的段落文字（不同段落之间用空行分隔）。
    Text(String),
    /// 流式增量片段（直接拼接，不加空行）。
    TextDelta(String),
    /// 思考过程（完整或增量）。
    Reasoning(String),
    Started {
        id: String,
        tool: String,
        title: String,
    },
    Ended {
        id: String,
        tool: String,
        title: String,
        ok: bool,
        output: String,
    },
    Usage {
        model: String,
        usage: Usage,
    },
    Failed(String),
}

/// 把命令拆成程序和参数：按空白分开，支持双引号与单引号（保留 Windows 路径里的反斜杠）。
pub(crate) fn split_command(text: &str) -> Vec<String> {
    let mut parts = vec![];
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some('"'), '\\') if chars.peek() == Some(&'"') => {
                chars.next();
                current.push('"');
                any = true;
            }
            (Some(q), c) if c == q => {
                quote = None;
                any = true;
            }
            (None, '"' | '\'') => {
                quote = Some(c);
                any = true;
            }
            (None, c) if c.is_whitespace() => {
                if any || !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
                any = false;
            }
            (_, c) => {
                current.push(c);
                any = true;
            }
        }
    }
    if any || !current.is_empty() {
        parts.push(current);
    }
    parts
}

fn is_executable_file(candidate: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(candidate) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// 常见的用户级 CLI 安装目录（GUI 启动或非登录 shell 下可能不在 PATH 里）。
fn vendor_bin_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    if let Some(home) = home {
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".opencode/bin"));
        dirs.push(home.join(".kimi-code/bin"));
        dirs.push(home.join(".pi/agent/bin"));
        dirs.push(home.join(".cargo/bin"));
    }
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            dirs.push(PathBuf::from(appdata).join("npm"));
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(PathBuf::from(local).join("agy/bin"));
        }
    }
    dirs
}

/// 在 PATH 与常见安装目录里找程序。Windows 上按 PATHEXT 补后缀（`claude` 其实是 `claude.cmd`）。
///
/// 安全约束：搜索 PATH 时跳过相对路径项，避免工作目录里的恶意同名文件劫持命令。
pub fn find_program(program: &str, dir: &Path) -> Option<PathBuf> {
    let path = Path::new(program);
    let exts: Vec<String> = if cfg!(windows) {
        let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        std::iter::once(String::new())
            .chain(pathext.split(';').map(|e| e.to_ascii_lowercase()))
            .collect()
    } else {
        vec![String::new()]
    };
    let find_file = |base: &Path, require_exec: bool| {
        exts.iter()
            .map(|ext| PathBuf::from(format!("{}{ext}", base.display())))
            .find(|candidate| {
                if require_exec {
                    is_executable_file(candidate)
                } else {
                    candidate.is_file()
                }
            })
    };
    // `./scripts/harness` 这种带目录的相对路径从 harness 的工作目录算起。
    if path.components().count() > 1 || path.is_absolute() {
        return find_file(&dir.join(path), false);
    }
    if let Some(path_env) = std::env::var_os("PATH") {
        if let Some(found) = std::env::split_paths(&path_env)
            .filter(|p| p.is_absolute())
            .find_map(|p| find_file(&p.join(path), true))
        {
            return Some(found);
        }
    }
    vendor_bin_dirs()
        .into_iter()
        .find_map(|p| find_file(&p.join(path), true))
}

/// 构造子进程命令：Windows 下 `.cmd`/`.bat` 经 `cmd.exe /d /s /c` 启动，防止 CreateProcess 失败。
pub(crate) fn build_command(program: &Path, args: &[String]) -> Command {
    #[cfg(windows)]
    {
        let is_batch = program
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"));
        if is_batch {
            let mut cmd = Command::new("cmd.exe");
            cmd.args(["/d", "/s", "/c"]).arg(program).args(args);
            return cmd;
        }
    }
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd
}

/// 这种 harness 用哪个程序、哪些参数，提示词从哪给，以及需要随本轮结束清理的临时配置文件。
fn plan(launch: &Launch<'_>) -> Result<(PathBuf, Vec<String>, bool, TempFileGuard), String> {
    let mut words = split_command(launch.command);
    if words.is_empty() {
        match launch.harness.default_program() {
            Some(program) => words.push(program.to_string()),
            None => return Err("没有写要跑的命令".into()),
        }
    }
    let program = words.remove(0);
    let extra = words;
    let has = |flag: &str| {
        extra
            .iter()
            .any(|w| w == flag || w.starts_with(&format!("{flag}=")))
    };
    let full = launch.permission == PermissionMode::Full;
    let plan_mode = launch.permission == PermissionMode::Plan;
    let mut args = vec![];
    let mut stdin = true;
    let mut temp_file = TempFileGuard(None);
    match launch.harness {
        Harness::ClaudeCode => {
            args.extend(extra.iter().cloned());
            args.extend(
                [
                    "-p",
                    "--output-format",
                    "stream-json",
                    "--verbose",
                    "--include-partial-messages",
                ]
                .map(String::from),
            );
            if !has("--permission-mode") && !has("--dangerously-skip-permissions") {
                if full {
                    args.push("--dangerously-skip-permissions".into());
                } else if plan_mode {
                    args.extend(["--permission-mode", "plan"].map(String::from));
                } else {
                    args.extend(["--permission-mode", "acceptEdits"].map(String::from));
                }
            }
            if !launch.mcp_servers.is_empty() && !has("--mcp-config") {
                if let Some(path) = write_claude_mcp_config(launch.mcp_servers) {
                    args.extend(["--mcp-config".to_string(), path.display().to_string()]);
                    temp_file = TempFileGuard(Some(path));
                }
            }
            if let Some(session) = launch.session {
                args.extend(["--resume".to_string(), session.to_string()]);
            }
        }
        Harness::Codex => {
            args.push("exec".into());
            // `resume` 是 `codex exec` 的子命令，必须紧跟在 `exec` 后面。
            if launch.session.is_some() {
                args.push("resume".into());
            }
            args.extend(["--json", "--skip-git-repo-check"].map(String::from));
            if launch.session.is_some() {
                args.push("--all".into());
            }
            if !has("--sandbox")
                && !has("-s")
                && !has("--full-auto")
                && !has("--dangerously-bypass-approvals-and-sandbox")
            {
                if full {
                    args.push("--dangerously-bypass-approvals-and-sandbox".into());
                } else if plan_mode {
                    args.extend(
                        ["--sandbox", "read-only", "-c", "approval_policy=\"never\""]
                            .map(String::from),
                    );
                } else {
                    args.extend(
                        [
                            "--sandbox",
                            "workspace-write",
                            "-c",
                            "approval_policy=\"never\"",
                        ]
                        .map(String::from),
                    );
                }
            }
            for spec in launch.mcp_servers {
                if let Some(overrides) = codex_mcp_overrides(spec) {
                    for kv in overrides {
                        args.push("-c".into());
                        args.push(kv);
                    }
                }
            }
            args.extend(extra.iter().cloned());
            if let Some(session) = launch.session {
                args.push(session.to_string());
            }
            args.push("-".into());
        }
        Harness::OpenCode => {
            args.push("run".into());
            args.extend(extra.iter().cloned());
            args.extend(["--format", "json"].map(String::from));
            if let Some(session) = launch.session {
                args.extend(["--session".to_string(), session.to_string()]);
            }
            args.push("--".into());
            args.push(launch.prompt.to_string());
            stdin = false;
        }
        Harness::Gemini => {
            args.extend(extra.iter().cloned());
            args.extend(["--output-format", "stream-json"].map(String::from));
            if full && !has("--yolo") && !has("-y") && !has("--approval-mode") {
                args.push("--yolo".into());
            }
            if let Some(session) = launch.session {
                args.extend(["--resume".to_string(), session.to_string()]);
            }
            args.extend(["-p".to_string(), launch.prompt.to_string()]);
            stdin = false;
        }
        Harness::Kimi => {
            args.extend(extra.iter().cloned());
            args.extend(["--print", "--output-format", "stream-json"].map(String::from));
            if full && !has("--yolo") && !has("-y") {
                args.push("--yolo".into());
            }
            if let Some(session) = launch.session {
                args.extend(["--session".to_string(), session.to_string()]);
            }
        }
        Harness::Acp | Harness::Command => args.extend(extra.iter().cloned()),
        Harness::Builtin => return Err("内置 Bot 不走 harness".into()),
    }
    let resolved = find_program(&program, launch.dir).ok_or_else(|| {
        format!(
            "这台电脑上找不到 {program}，{} 没法启动。装好之后再试，或者在 Bot 设置里写上它的完整路径",
            launch.harness.label()
        )
    })?;
    Ok((resolved, args, stdin, temp_file))
}

pub(crate) fn write_claude_mcp_config(specs: &[McpSpec]) -> Option<PathBuf> {
    let mut servers = serde_json::Map::new();
    for spec in specs {
        let entry = if let Some(url) = &spec.url {
            let headers: serde_json::Map<String, Value> = spec
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect();
            let mut obj = serde_json::Map::new();
            obj.insert("type".into(), Value::String("http".into()));
            obj.insert("url".into(), Value::String(url.clone()));
            if !headers.is_empty() {
                obj.insert("headers".into(), Value::Object(headers));
            }
            Value::Object(obj)
        } else if !spec.command.trim().is_empty() {
            let env: serde_json::Map<String, Value> = spec
                .env
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect();
            let mut obj = serde_json::Map::new();
            obj.insert("command".into(), Value::String(spec.command.clone()));
            obj.insert("args".into(), json!(spec.args));
            if !env.is_empty() {
                obj.insert("env".into(), Value::Object(env));
            }
            Value::Object(obj)
        } else {
            continue;
        };
        servers.insert(spec.name.clone(), entry);
    }
    if servers.is_empty() {
        return None;
    }
    let payload = json!({ "mcpServers": servers });
    let seq = TEMP_MCP_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("myriad-mcp-{}-{seq}.json", std::process::id()));
    let bytes = serde_json::to_vec(&payload).ok()?;
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .ok()?;
        file.write_all(&bytes).ok()?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&path, &bytes).ok()?;
    }
    Some(path)
}

fn codex_mcp_overrides(spec: &McpSpec) -> Option<Vec<String>> {
    if spec.name.is_empty()
        || !spec
            .name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return None;
    }
    let mut out = Vec::new();
    if let Some(url) = &spec.url {
        let encoded = serde_json::to_string(url).ok()?;
        out.push(format!("mcp_servers.{}.url={encoded}", spec.name));
    } else if !spec.command.trim().is_empty() {
        let cmd = serde_json::to_string(&spec.command).ok()?;
        out.push(format!("mcp_servers.{}.command={cmd}", spec.name));
        if !spec.args.is_empty() {
            let args = serde_json::to_string(&spec.args).ok()?;
            out.push(format!("mcp_servers.{}.args={args}", spec.name));
        }
    } else {
        return None;
    }
    Some(out)
}

fn acp_mcp_servers(specs: &[McpSpec]) -> Vec<Value> {
    let mut out = Vec::new();
    for spec in specs {
        if let Some(url) = &spec.url {
            let headers: Vec<Value> = spec
                .headers
                .iter()
                .map(|(k, v)| json!({ "name": k, "value": v }))
                .collect();
            out.push(json!({
                "type": "http",
                "name": spec.name,
                "url": url,
                "headers": headers,
            }));
        } else if !spec.command.trim().is_empty() {
            let env: Vec<Value> = spec
                .env
                .iter()
                .map(|(k, v)| json!({ "name": k, "value": v }))
                .collect();
            out.push(json!({
                "name": spec.name,
                "command": spec.command,
                "args": spec.args,
                "env": env,
            }));
        }
    }
    out
}

/// 探测本机常见的 Agent CLI 是否已安装以及版本号。
pub async fn probe_harnesses(dir: &Path) -> Vec<HarnessProbe> {
    let candidates: &[(Harness, &str, &str)] = &[
        (Harness::ClaudeCode, "claude", "Claude Code"),
        (Harness::Codex, "codex", "Codex"),
        (Harness::OpenCode, "opencode", "OpenCode"),
        (Harness::Gemini, "gemini", "Gemini CLI"),
        (Harness::Kimi, "kimi", "Kimi Code"),
        (Harness::Acp, "cursor-agent", "Cursor Agent (ACP)"),
    ];
    let dir = dir.to_path_buf();
    let futs = candidates.iter().map(|&(harness, program, label)| {
        let dir = dir.clone();
        async move {
            let found = find_program(program, &dir);
            let version = match &found {
                Some(path) => probe_version(path).await,
                None => None,
            };
            HarnessProbe {
                harness,
                program: program.to_string(),
                label: label.to_string(),
                available: found.is_some(),
                path: found.map(|p| p.display().to_string()),
                version,
            }
        }
    });
    futures_util::future::join_all(futs).await
}

async fn probe_version(program: &Path) -> Option<String> {
    let mut cmd = build_command(program, &["--version".to_string()]);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(PROBE_TIMEOUT, cmd.output())
        .await
        .ok()?
        .ok()?;
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    extract_semver(&combined)
}

fn extract_semver(text: &str) -> Option<String> {
    for token in text.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ',' | ';')) {
        let t = token.trim_start_matches(['v', 'V']);
        let core = t.split('-').next().unwrap_or(t);
        let mut parts = core.split('.');
        if let (Some(a), Some(b), Some(c), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        {
            if !a.is_empty()
                && !b.is_empty()
                && !c.is_empty()
                && a.bytes().all(|x| x.is_ascii_digit())
                && b.bytes().all(|x| x.is_ascii_digit())
                && c.bytes().all(|x| x.is_ascii_digit())
            {
                return Some(t.to_string());
            }
        }
    }
    None
}

/// 「以后都允许」记的是哪个 Bot 用哪个 harness、哪条命令：换了 harness 或命令就要重新问。
pub fn approval_key(bot: &str, harness: Harness, command: &str) -> String {
    format!("{bot}\n{harness:?}\n{}", command.trim())
}

/// stderr 最多在内存里保留的末尾字节数，避免失控子进程把内存撑爆。
const MAX_STDERR_BYTES: usize = 32 * 1024;
/// stdout 单行最多读入的字节数，超出部分丢弃到下一个换行符。
pub(crate) const MAX_STDOUT_LINE_BYTES: usize = 4 * 1024 * 1024;

pub(crate) async fn read_stderr_tail(mut err: tokio::process::ChildStderr) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while let Ok(n) = err.read(&mut chunk).await {
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_STDERR_BYTES * 2 {
            let drain = buf.len() - MAX_STDERR_BYTES;
            buf.drain(..drain);
        }
    }
    if buf.len() > MAX_STDERR_BYTES {
        let drain = buf.len() - MAX_STDERR_BYTES;
        buf.drain(..drain);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

pub(crate) async fn read_bounded_line_buffered<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    state: &mut LineBuffer,
) -> std::io::Result<Option<String>> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if state.buf.is_empty() {
                state.capped = false;
                return Ok(None);
            }
            break;
        }
        let newline = available.iter().position(|&b| b == b'\n');
        let take = newline.map_or(available.len(), |pos| pos + 1);
        if !state.capped {
            let remaining = MAX_STDOUT_LINE_BYTES.saturating_sub(state.buf.len());
            if take <= remaining {
                state.buf.extend_from_slice(&available[..take]);
            } else {
                state.buf.extend_from_slice(&available[..remaining]);
                state.capped = true;
            }
        }
        reader.consume(take);
        if newline.is_some() {
            break;
        }
    }
    let mut out = std::mem::take(&mut state.buf);
    state.capped = false;
    if out.ends_with(b"\n") {
        out.pop();
        if out.ends_with(b"\r") {
            out.pop();
        }
    }
    Ok(Some(String::from_utf8_lossy(&out).into_owned()))
}

async fn read_bounded_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<String>> {
    let mut state = LineBuffer::default();
    read_bounded_line_buffered(reader, &mut state).await
}

/// 跑一次 harness，直到它结束、出错或者被停下。
pub(crate) async fn run(
    launch: &Launch<'_>,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> Finished {
    run_interactive(launch, None, observer, cancel).await
}

/// 跑一次支持运行中插话的 harness：
/// - `Harness::Codex` 优先走 `codex app-server --listen stdio://` 双向持久会话，不兼容时回退 `codex exec --json`；
/// - `Harness::ClaudeCode` 优先走 `claude --input-format stream-json` 双向持久会话，不兼容时回退单次 `claude -p`。
pub(crate) async fn run_interactive(
    launch: &Launch<'_>,
    mut steer_rx: Option<tokio::sync::mpsc::UnboundedReceiver<SteerRequest>>,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> Finished {
    if launch.harness == Harness::Acp {
        return run_acp(launch, observer, cancel).await;
    }
    if launch.harness == Harness::Codex && launch.interactive_approvals {
        match crate::codex_app_server::run_codex_app_server(launch, &mut steer_rx, observer, cancel)
            .await
        {
            crate::codex_app_server::CodexAttempt::Finished(done) => return done,
            crate::codex_app_server::CodexAttempt::FallbackToLegacy => {
                // 回退单次 `codex exec --json` 时主动关闭 steer 接收端，确保调用方保留未读消息到下一轮。
                steer_rx = None;
            }
        }
    } else if launch.harness == Harness::ClaudeCode && launch.interactive_approvals {
        match crate::claude_stream::run_claude_stream(launch, &mut steer_rx, observer, cancel).await
        {
            crate::claude_stream::ClaudeAttempt::Finished(done) => return done,
            crate::claude_stream::ClaudeAttempt::FallbackToLegacy => {
                steer_rx = None;
            }
        }
    }
    drop(steer_rx);
    let mut session = launch.session.map(String::from);
    let failed = |reason: String, session: Option<String>| Finished {
        end: TurnEnd::Failed(reason),
        session,
        text: String::new(),
        reasoning: None,
    };
    let (program, args, use_stdin, _temp_file) = match plan(launch) {
        Ok(plan) => plan,
        Err(reason) => return failed(reason, session),
    };
    let mut command = build_command(&program, &args);
    command
        .current_dir(launch.dir)
        .env("SA_HARNESS_DIR", launch.dir)
        .stdin(if use_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // sa 自己用的密钥不交给 harness：它和 Bot 执行的命令一样拿不到。
    for name in sa_tools::hidden_env() {
        command.env_remove(name);
    }
    match launch.session {
        Some(id) => command.env("SA_HARNESS_SESSION", id),
        None => command.env_remove("SA_HARNESS_SESSION"),
    };
    let mut child = match Tree::spawn(command) {
        Ok(child) => child,
        Err(error) => {
            return failed(
                format!("{} 启动不了：{error}", launch.harness.label()),
                session,
            )
        }
    };
    if let Some(mut stdin) = child.stdin() {
        let prompt = launch.prompt.to_string();
        // 写完就关上，harness 读到结尾才开始干活。它不读也不卡在这里。
        tokio::spawn(async move {
            let _ = stdin.write_all(prompt.as_bytes()).await;
            let _ = stdin.shutdown().await;
        });
    }
    let stderr = child
        .stderr()
        .map(|err| tokio::spawn(read_stderr_tail(err)));
    let mut reader = BufReader::new(child.stdout().expect("上面接了管道"));
    let mut text = String::new();
    let mut reasoning = String::new();
    // 已经通过流式增量发出过文字时，后续收到的同轮完整 assistant 块不再重复拼接。
    let mut streamed_block = false;
    let mut open: HashMap<String, ()> = HashMap::new();
    let mut failure: Option<String> = None;
    let mut seen = 0usize;
    let mut thoughts = 0usize;

    // 它自己退出了，开出来的后台进程可能还占着输出：再读一小会儿剩下的就不等了。
    let mut exited = None;
    let mut timed_out = false;
    let stopped = loop {
        let draining = exited.is_some();
        let line = tokio::select! {
            _ = cancel.cancelled() => break true,
            status = child.wait(), if !draining => {
                exited = Some(status);
                continue;
            }
            line = async {
                if draining {
                    tokio::time::timeout(DRAIN, read_bounded_line(&mut reader))
                        .await
                        .unwrap_or(Ok(None))
                } else {
                    match tokio::time::timeout(IDLE_TIMEOUT, read_bounded_line(&mut reader)).await {
                        Ok(res) => res,
                        Err(_) => {
                            timed_out = true;
                            Ok(None)
                        }
                    }
                }
            } => line,
        };
        let Ok(Some(line)) = line else { break false };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            // 自定义命令直接印出普通文字时照样能看；别的 harness 的杂音（启动横幅之类）跳过。
            if launch.harness == Harness::Command {
                let piece = if text.is_empty() {
                    trimmed.to_string()
                } else {
                    format!("\n{trimmed}")
                };
                text.push_str(&piece);
                observer.text_delta(&piece).await;
            }
            continue;
        };
        for event in parse(launch.harness, &value) {
            match event {
                Event::Session(id) if !id.is_empty() => session = Some(id),
                Event::Session(_) => {}
                Event::TextDelta(delta) if !delta.is_empty() => {
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
                    if !reasoning.is_empty() {
                        reasoning.push('\n');
                    }
                    reasoning.push_str(&chunk);
                    // 完整思考块折叠成步骤显示在时间线上，方便点开看推理过程。
                    if chunk.chars().count() >= 12 {
                        thoughts += 1;
                        let key = format!("think-{thoughts}");
                        observer.tool_started(&key, "think", "思考", "").await;
                        observer.tool_finished(&key, StepEnd::Ok, &chunk).await;
                    }
                }
                Event::Reasoning(_) => {}
                Event::Started { id, tool, title } => {
                    streamed_block = false;
                    seen += 1;
                    let key = format!("h{seen}-{id}");
                    if open.insert(key.clone(), ()).is_none() {
                        observer.tool_started(&key, &tool, &title, "").await;
                    }
                }
                Event::Ended {
                    id,
                    tool,
                    title,
                    ok,
                    output,
                } => {
                    // 开始时编了号：按原来的 id 找回去。没报过开始的，现在补一个。
                    let key = open
                        .keys()
                        .find(|key| key.split_once('-').map(|(_, raw)| raw) == Some(id.as_str()))
                        .cloned();
                    let key = match key {
                        Some(key) => key,
                        None => {
                            seen += 1;
                            let key = format!("h{seen}-{id}");
                            observer.tool_started(&key, &tool, &title, "").await;
                            key
                        }
                    };
                    open.remove(&key);
                    let end = if ok { StepEnd::Ok } else { StepEnd::Failed };
                    observer.tool_finished(&key, end, &output).await;
                }
                Event::Usage { model, usage } => {
                    observer.usage(&model, usage).await;
                }
                Event::Failed(reason) => failure = Some(reason),
            }
        }
    };

    let reasoning_out = (!reasoning.trim().is_empty()).then_some(reasoning);
    if stopped {
        child.kill();
        let _ = child.wait().await;
        return Finished {
            end: TurnEnd::Cancelled,
            session,
            text,
            reasoning: reasoning_out,
        };
    }
    if timed_out {
        child.kill();
        let _ = child.wait().await;
        return Finished {
            end: TurnEnd::Failed(format!(
                "{} 超过 {} 分钟没有任何输出，已自动停止",
                launch.harness.label(),
                IDLE_TIMEOUT.as_secs() / 60
            )),
            session,
            text,
            reasoning: reasoning_out,
        };
    }
    let status = match exited {
        Some(status) => status,
        None => tokio::select! {
            _ = cancel.cancelled() => {
                child.kill();
                let _ = child.wait().await;
                return Finished { end: TurnEnd::Cancelled, session, text, reasoning: reasoning_out };
            }
            status = child.wait() => status,
        },
    };
    // 它自己退出了，但开出来的子进程可能还在：一起结束。
    child.kill();
    // 万一还有没结束掉的后台进程占着错误输出：也只等一小会儿。
    let said = match stderr {
        Some(mut task) => match tokio::time::timeout(DRAIN, &mut task).await {
            Ok(said) => said.unwrap_or_default(),
            Err(_) => {
                task.abort();
                String::new()
            }
        },
        None => String::new(),
    };
    let end = match (failure, status) {
        (Some(reason), _) => TurnEnd::Failed(reason),
        (None, Ok(status)) if status.success() => TurnEnd::Completed,
        (None, status) => {
            let code = status
                .ok()
                .and_then(|s| s.code())
                .map(|c| format!("退出码 {c}"))
                .unwrap_or_else(|| "被结束了".into());
            let tail = tail(said.trim(), STDERR_TAIL);
            TurnEnd::Failed(if tail.is_empty() {
                format!("{} 中途退出了（{code}）", launch.harness.label())
            } else {
                format!("{} 中途退出了（{code}）：{tail}", launch.harness.label())
            })
        }
    };
    Finished {
        end,
        session,
        text,
        reasoning: reasoning_out,
    }
}

async fn send_acp(stdin: &mut tokio::process::ChildStdin, value: &Value) -> Result<(), String> {
    let mut line = serde_json::to_string(value).unwrap_or_default();
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stdin.flush().await.map_err(|e| e.to_string())
}

/// 从 ACP `session/new` / `session/load` 返回体里提取可用模式列表与当前模式。
fn parse_acp_modes(result: &Value) -> (Vec<String>, Option<String>) {
    let modes_obj = &result["modes"];
    let available: Vec<String> = modes_obj["availableModes"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    m["id"]
                        .as_str()
                        .or_else(|| m.as_str())
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                })
                .collect()
        })
        .unwrap_or_default();
    let current = modes_obj["currentModeId"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);
    (available, current)
}

fn is_acp_plan_mode(mode: &str) -> bool {
    let lower = mode.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "plan" | "architect" | "read-only" | "readonly" | "read_only"
    )
}

/// 根据房间权限档位与 ACP Agent 声明的模式列表，决定是否需要调用 `session/set_mode`。
fn desired_acp_mode(
    available: &[String],
    current: Option<&str>,
    permission: PermissionMode,
) -> Option<String> {
    if available.is_empty() {
        return None;
    }
    let target = if permission == PermissionMode::Plan {
        available.iter().find(|m| is_acp_plan_mode(m))?
    } else if current.is_some_and(is_acp_plan_mode) {
        available
            .iter()
            .find(|m| {
                let lower = m.to_ascii_lowercase();
                matches!(lower.as_str(), "default" | "code" | "agent" | "ask")
            })
            .or_else(|| available.iter().find(|m| !is_acp_plan_mode(m)))?
    } else {
        return None;
    };
    if current == Some(target.as_str()) {
        None
    } else {
        Some(target.clone())
    }
}

/// 通用 ACP（Agent Client Protocol）stdio JSON-RPC 驱动：
/// 支持 `cursor-agent acp`、`gemini --acp`、`kimi acp`、`opencode acp`、`goose acp` 等所有 ACP Agent。
/// 当传入 `acp_pool` + `acp_scope` 时复用常驻子进程与内存会话，免去每轮冷启动。
async fn run_acp(
    launch: &Launch<'_>,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> Finished {
    let mcp_fp = mcp_fingerprint(launch.mcp_servers);
    let pool_key = match (launch.acp_pool, launch.acp_scope) {
        (Some(_), Some(scope)) => Some(AcpKey {
            scope: scope.to_string(),
            command: launch.command.trim().to_string(),
            dir: launch.dir.to_path_buf(),
        }),
        _ => None,
    };

    // 先尝试复用常驻会话；若复用连接在首包发送或等待响应前即已断开，自动降级为冷启动重试一次。
    if let (Some(pool), Some(key)) = (launch.acp_pool, pool_key.as_ref()) {
        if let Some(conn) = pool.take(key, launch.session, &mcp_fp) {
            match run_acp_turn_on(conn, launch, observer, cancel, true).await {
                AcpTurnOutcome::ReusedDeadBeforeOutput => {
                    // 静默回退到冷启动
                }
                AcpTurnOutcome::Done { finished, keep } => {
                    if let Some(conn) = keep {
                        pool.put(key.clone(), *conn);
                    }
                    return finished;
                }
            }
        }
    }

    let conn = match spawn_acp_session(launch, &mcp_fp, cancel).await {
        Ok(conn) => conn,
        Err((end, session)) => {
            return Finished {
                end,
                session,
                text: String::new(),
                reasoning: None,
            };
        }
    };

    match run_acp_turn_on(conn, launch, observer, cancel, false).await {
        AcpTurnOutcome::Done { finished, keep } => {
            if let (Some(pool), Some(key), Some(conn)) = (launch.acp_pool, pool_key, keep) {
                pool.put(key, *conn);
            }
            finished
        }
        AcpTurnOutcome::ReusedDeadBeforeOutput => Finished {
            end: TurnEnd::Failed("ACP Agent 启动后立即退出了".into()),
            session: launch.session.map(String::from),
            text: String::new(),
            reasoning: None,
        },
    }
}

enum AcpTurnOutcome {
    /// 复用池中的旧连接在任何输出产生前就断开了，调用方应无感冷启动重试。
    ReusedDeadBeforeOutput,
    Done {
        finished: Finished,
        /// 仅在完全正常结束本轮且会话同步干净时归还连接池。
        keep: Option<Box<PooledAcp>>,
    },
}

/// 启动一个新的 ACP 子进程，完成 `initialize` + `session/load`（失败回退 `session/new`）。
async fn spawn_acp_session(
    launch: &Launch<'_>,
    mcp_fp: &str,
    cancel: &CancellationToken,
) -> Result<PooledAcp, (TurnEnd, Option<String>)> {
    let mut session = launch.session.map(String::from);
    let (program, args, _, _temp_file) =
        plan(launch).map_err(|reason| (TurnEnd::Failed(reason), session.clone()))?;
    let mut command = build_command(&program, &args);
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
    let mut child = Tree::spawn(command).map_err(|error| {
        (
            TurnEnd::Failed(format!("ACP Agent 启动不了：{error}")),
            session.clone(),
        )
    })?;
    let mut stdin = child.stdin().ok_or_else(|| {
        (
            TurnEnd::Failed("ACP Agent 标准输入不可用".into()),
            session.clone(),
        )
    })?;
    let stderr = child
        .stderr()
        .map(|err| tokio::spawn(read_stderr_tail(err)));
    let mut reader = BufReader::new(child.stdout().expect("上面接了管道"));

    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": 1,
            "clientCapabilities": {},
            "clientInfo": { "name": "myriad", "version": env!("CARGO_PKG_VERSION") }
        }
    });
    if let Err(error) = send_acp(&mut stdin, &init).await {
        child.kill();
        return Err((
            TurnEnd::Failed(format!("ACP initialize 发送失败：{error}")),
            session,
        ));
    }

    let cwd = launch.dir.display().to_string();
    let acp_mcps = acp_mcp_servers(launch.mcp_servers);
    let mut stage = 1u8; // 1 = wait init, 2 = wait session
    let mut tried_load = false;
    let mut failure: Option<String> = None;

    let stopped = loop {
        let line = tokio::select! {
            _ = cancel.cancelled() => break true,
            line = tokio::time::timeout(IDLE_TIMEOUT, read_bounded_line(&mut reader)) => match line {
                Ok(res) => res,
                Err(_) => {
                    failure = Some("ACP Agent 握手超时，已自动停止".into());
                    break false;
                }
            },
        };
        let Ok(Some(line)) = line else { break false };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if msg.get("method").is_some() {
            continue;
        }
        let id = msg["id"].as_u64();
        if let Some(err) = msg.get("error").filter(|v| !v.is_null()) {
            if id == Some(2) && stage == 2 && tried_load {
                tried_load = false;
                session = None;
                let fallback = json!({
                    "jsonrpc": "2.0",
                    "id": 20,
                    "method": "session/new",
                    "params": { "cwd": cwd, "mcpServers": acp_mcps }
                });
                if let Err(error) = send_acp(&mut stdin, &fallback).await {
                    failure = Some(format!("ACP 创建新会话失败：{error}"));
                    break false;
                }
                continue;
            }
            let reason = str_of(err, "message");
            failure = Some(if reason.is_empty() {
                format!("ACP Agent 报错：{err}")
            } else {
                reason.to_string()
            });
            break false;
        }
        if id == Some(1) && stage == 1 {
            stage = 2;
            let req = match &session {
                Some(sid) => {
                    tried_load = true;
                    json!({
                        "jsonrpc": "2.0",
                        "id": 2,
                        "method": "session/load",
                        "params": { "sessionId": sid, "cwd": cwd, "mcpServers": acp_mcps }
                    })
                }
                None => json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "session/new",
                    "params": { "cwd": cwd, "mcpServers": acp_mcps }
                }),
            };
            if let Err(error) = send_acp(&mut stdin, &req).await {
                failure = Some(format!("ACP 创建会话失败：{error}"));
                break false;
            }
        } else if matches!(id, Some(2) | Some(20)) && stage == 2 {
            if let Some(sid) = msg["result"]["sessionId"]
                .as_str()
                .filter(|s| !s.is_empty())
            {
                session = Some(sid.to_string());
            }
            let (available_modes, current_mode) = parse_acp_modes(&msg["result"]);
            let Some(sid) = session else {
                failure = Some("ACP Agent 没有返回 sessionId".into());
                break false;
            };
            return Ok(PooledAcp {
                child,
                stdin,
                reader,
                stderr,
                session_id: sid,
                next_id: 30,
                mcp_fingerprint: mcp_fp.to_string(),
                available_modes,
                current_mode,
                last_used: Instant::now(),
            });
        }
    };

    drop(stdin);
    child.kill();
    let _ = child.wait().await;
    if stopped {
        if let Some(h) = stderr {
            h.abort();
        }
        return Err((TurnEnd::Cancelled, session));
    }
    let said = match stderr {
        Some(mut task) => match tokio::time::timeout(DRAIN, &mut task).await {
            Ok(said) => said.unwrap_or_default(),
            Err(_) => {
                task.abort();
                String::new()
            }
        },
        None => String::new(),
    };
    let reason = match failure {
        Some(r) => r,
        None => {
            let tail = tail(said.trim(), STDERR_TAIL);
            if tail.is_empty() {
                "ACP Agent 提前退出了".into()
            } else {
                format!("ACP Agent 提前退出了：{tail}")
            }
        }
    };
    Err((TurnEnd::Failed(reason), session))
}

/// 在已握手就绪的 ACP 会话上执行一轮 `session/prompt`（必要时先同步模式）。
async fn run_acp_turn_on(
    mut conn: PooledAcp,
    launch: &Launch<'_>,
    observer: &dyn Observer,
    cancel: &CancellationToken,
    reused: bool,
) -> AcpTurnOutcome {
    let sid = conn.session_id.clone();

    // 若 Agent 声明了模式（如 plan / default），自动随房间权限档位切换。
    if let Some(target_mode) = desired_acp_mode(
        &conn.available_modes,
        conn.current_mode.as_deref(),
        launch.permission,
    ) {
        let mode_id = conn.next_id;
        conn.next_id += 1;
        let req = json!({
            "jsonrpc": "2.0",
            "id": mode_id,
            "method": "session/set_mode",
            "params": { "sessionId": sid, "modeId": target_mode }
        });
        if send_acp(&mut conn.stdin, &req).await.is_err() {
            if reused {
                return AcpTurnOutcome::ReusedDeadBeforeOutput;
            }
        } else {
            // 等待 `session/set_mode` 响应（超时或报错不阻断本轮，继续按权限拦截兜底）。
            let deadline = tokio::time::sleep(Duration::from_secs(5));
            tokio::pin!(deadline);
            loop {
                let line = tokio::select! {
                    _ = cancel.cancelled() => {
                        return AcpTurnOutcome::Done {
                            finished: Finished {
                                end: TurnEnd::Cancelled,
                                session: Some(sid),
                                text: String::new(),
                                reasoning: None,
                            },
                            keep: None,
                        };
                    }
                    _ = &mut deadline => break,
                    line = read_bounded_line(&mut conn.reader) => line,
                };
                let Ok(Some(line)) = line else {
                    if reused {
                        return AcpTurnOutcome::ReusedDeadBeforeOutput;
                    }
                    break;
                };
                let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else {
                    continue;
                };
                if str_of(&msg, "method") == "session/update" {
                    let update = &msg["params"]["update"];
                    if str_of(update, "sessionUpdate") == "current_mode_update" {
                        let mode = match str_of(update, "currentModeId") {
                            "" => str_of(update, "modeId"),
                            m => m,
                        };
                        if !mode.is_empty() {
                            conn.current_mode = Some(mode.to_string());
                        }
                    }
                    continue;
                }
                if msg.get("method").is_none() && msg["id"].as_u64() == Some(mode_id) {
                    if msg.get("error").is_none_or(Value::is_null) {
                        conn.current_mode = Some(target_mode);
                    }
                    break;
                }
            }
        }
    }

    // 发 prompt 之前最后检查一次复用进程是否仍存活；一旦发出 prompt 就不再自动重放，避免重复改动工作目录。
    if reused && conn.child.has_exited() {
        return AcpTurnOutcome::ReusedDeadBeforeOutput;
    }

    let prompt_id = conn.next_id;
    conn.next_id += 1;
    let prompt_req = json!({
        "jsonrpc": "2.0",
        "id": prompt_id,
        "method": "session/prompt",
        "params": {
            "sessionId": sid,
            "prompt": [{ "type": "text", "text": launch.prompt }]
        }
    });
    if let Err(error) = send_acp(&mut conn.stdin, &prompt_req).await {
        if reused {
            return AcpTurnOutcome::ReusedDeadBeforeOutput;
        }
        return AcpTurnOutcome::Done {
            finished: Finished {
                end: TurnEnd::Failed(format!("ACP 发送提示词失败：{error}")),
                session: Some(sid),
                text: String::new(),
                reasoning: None,
            },
            keep: None,
        };
    }

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut open: HashMap<String, (String, String)> = HashMap::new();
    let mut perm_seq = 0u64;
    let mut plan_seq = 0u64;
    let mut got_prompt_response = false;
    let mut failure: Option<String> = None;

    let stopped = loop {
        let line = tokio::select! {
            _ = cancel.cancelled() => break true,
            line = tokio::time::timeout(IDLE_TIMEOUT, read_bounded_line(&mut conn.reader)) => match line {
                Ok(res) => res,
                Err(_) => {
                    failure = Some("ACP Agent 长时间无响应，已自动停止".into());
                    break false;
                }
            },
        };
        let Ok(Some(line)) = line else { break false };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };

        if msg.get("method").is_none() {
            let id = msg["id"].as_u64();
            if id == Some(prompt_id) {
                if let Some(err) = msg.get("error").filter(|v| !v.is_null()) {
                    let reason = str_of(err, "message");
                    failure = Some(if reason.is_empty() {
                        format!("ACP Agent 报错：{err}")
                    } else {
                        reason.to_string()
                    });
                } else {
                    got_prompt_response = true;
                }
                break false;
            }
            continue;
        }

        match str_of(&msg, "method") {
            "session/update" => {
                let update = &msg["params"]["update"];
                match str_of(update, "sessionUpdate") {
                    "agent_message_chunk" => {
                        let chunk = str_of(&update["content"], "text");
                        if !chunk.is_empty() {
                            text.push_str(chunk);
                            observer.text_delta(chunk).await;
                        }
                    }
                    "agent_thought_chunk" => {
                        let chunk = str_of(&update["content"], "text");
                        if !chunk.is_empty() {
                            reasoning.push_str(chunk);
                        }
                    }
                    "plan" => {
                        if let Some(entries) = update["entries"].as_array() {
                            let summary: Vec<String> = entries
                                .iter()
                                .filter_map(|e| {
                                    let content = str_of(e, "content");
                                    if content.is_empty() {
                                        return None;
                                    }
                                    let status = match str_of(e, "status") {
                                        "completed" => "[x]",
                                        "in_progress" => "[-]",
                                        _ => "[ ]",
                                    };
                                    Some(format!("{status} {content}"))
                                })
                                .collect();
                            if !summary.is_empty() {
                                plan_seq += 1;
                                let key = format!("acp-plan-{plan_seq}");
                                observer.tool_started(&key, "todo", "更新计划", "").await;
                                observer
                                    .tool_finished(&key, StepEnd::Ok, &summary.join("\n"))
                                    .await;
                            }
                        }
                    }
                    "tool_call" | "tool_call_update" => {
                        let call_id = str_of(update, "toolCallId").to_string();
                        if call_id.is_empty() {
                            continue;
                        }
                        let kind = str_of(update, "kind");
                        let tool = match kind {
                            "execute" => "shell",
                            "read" => "read",
                            "edit" | "delete" | "move" => "edit",
                            "search" => "search",
                            "fetch" => "web_fetch",
                            other if !other.is_empty() => other,
                            _ => "tool",
                        };
                        let title = match str_of(update, "title") {
                            "" => tool.to_string(),
                            t => t.to_string(),
                        };
                        let status = str_of(update, "status");
                        let key = format!("acp-{call_id}");
                        if !open.contains_key(&key) {
                            open.insert(key.clone(), (tool.to_string(), title.clone()));
                            observer.tool_started(&key, tool, &title, "").await;
                        }
                        if matches!(status, "completed" | "failed") {
                            open.remove(&key);
                            let out = content_text(&update["content"]);
                            let end = if status == "completed" {
                                StepEnd::Ok
                            } else {
                                StepEnd::Failed
                            };
                            observer.tool_finished(&key, end, &out).await;
                        }
                    }
                    "current_mode_update" => {
                        let mode = match str_of(update, "currentModeId") {
                            "" => str_of(update, "modeId"),
                            m => m,
                        };
                        if !mode.is_empty() {
                            conn.current_mode = Some(mode.to_string());
                        }
                    }
                    "usage_update" => {
                        let used = update["used"].as_u64().unwrap_or(0);
                        if used > 0 {
                            observer
                                .usage(
                                    "acp",
                                    Usage {
                                        input_tokens: used,
                                        output_tokens: 0,
                                        cached_tokens: 0,
                                        cache_write_tokens: 0,
                                    },
                                )
                                .await;
                        }
                    }
                    _ => {}
                }
            }
            "session/request_permission" => {
                let req_id = msg["id"].clone();
                let params = &msg["params"];
                let options = params["options"].as_array().cloned().unwrap_or_default();
                let pick = |wanted: &[&str]| {
                    options
                        .iter()
                        .find(|opt| wanted.contains(&str_of(opt, "kind")))
                        .and_then(|opt| opt["optionId"].as_str())
                        .map(String::from)
                };
                let allow_id = pick(&["allow_once", "allow_always"]).or_else(|| {
                    options
                        .first()
                        .and_then(|o| o["optionId"].as_str())
                        .map(String::from)
                });
                let deny_id = pick(&["reject_once", "reject_always"]);

                let tool_kind = str_of(&params["toolCall"], "kind");
                let mutating = matches!(tool_kind, "execute" | "edit" | "delete" | "move");

                let approved = if launch.permission == PermissionMode::Plan && mutating {
                    perm_seq += 1;
                    let key = format!("acp-plan-deny-{perm_seq}");
                    let title = match str_of(&params["toolCall"], "title") {
                        "" => "规划模式拦截写操作",
                        t => t,
                    };
                    observer.tool_started(&key, "harness", title, "").await;
                    observer
                        .tool_finished(
                            &key,
                            StepEnd::Stopped,
                            "当前处于规划模式（Plan Mode），已自动拒绝修改文件/执行命令请求。",
                        )
                        .await;
                    false
                } else if launch.permission == PermissionMode::Full {
                    true
                } else {
                    let title = match str_of(&params["toolCall"], "title") {
                        "" => "ACP Agent 请求执行操作",
                        t => t,
                    };
                    let raw_call_id = str_of(&params["toolCall"], "toolCallId");
                    let existing_key =
                        (!raw_call_id.is_empty()).then(|| format!("acp-{raw_call_id}"));
                    let (step_key, created_temp) = match existing_key {
                        Some(key) if open.contains_key(&key) => (key, false),
                        _ => {
                            perm_seq += 1;
                            let key = format!("acp-perm-{perm_seq}");
                            observer
                                .tool_started(
                                    &key,
                                    "harness",
                                    title,
                                    &params["toolCall"].to_string(),
                                )
                                .await;
                            (key, true)
                        }
                    };
                    let req = ApprovalRequest {
                        call_id: &step_key,
                        tool: "harness",
                        title,
                        reason: "ACP Agent 请求你批准这一步操作。",
                        input: &params["toolCall"],
                    };
                    let ok = tokio::select! {
                        ok = observer.request_approval(req) => ok,
                        _ = cancel.cancelled() => false,
                    };
                    if created_temp {
                        observer
                            .tool_finished(
                                &step_key,
                                if ok { StepEnd::Ok } else { StepEnd::Stopped },
                                if ok { "已批准" } else { "已拒绝" },
                            )
                            .await;
                    }
                    ok
                };
                let outcome = match (approved, allow_id, deny_id) {
                    (true, Some(id), _) => json!({ "outcome": "selected", "optionId": id }),
                    (false, _, Some(id)) => json!({ "outcome": "selected", "optionId": id }),
                    _ => json!({ "outcome": "cancelled" }),
                };
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": { "outcome": outcome }
                });
                let _ = send_acp(&mut conn.stdin, &reply).await;
            }
            _ => {}
        }
    };

    if !reasoning.trim().is_empty() {
        observer
            .tool_started("acp-think", "think", "思考", "")
            .await;
        observer
            .tool_finished("acp-think", StepEnd::Ok, reasoning.trim())
            .await;
    }
    let reasoning_out = (!reasoning.trim().is_empty()).then_some(reasoning);

    if stopped {
        let cancel_note = json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": sid }
        });
        let _ = tokio::time::timeout(
            Duration::from_millis(300),
            send_acp(&mut conn.stdin, &cancel_note),
        )
        .await;
        drop(conn);
        return AcpTurnOutcome::Done {
            finished: Finished {
                end: TurnEnd::Cancelled,
                session: Some(sid),
                text,
                reasoning: reasoning_out,
            },
            keep: None,
        };
    }

    if got_prompt_response && failure.is_none() {
        let keep = if launch.acp_pool.is_some() && launch.acp_scope.is_some() {
            Some(Box::new(conn))
        } else {
            drop(conn);
            None
        };
        return AcpTurnOutcome::Done {
            finished: Finished {
                end: TurnEnd::Completed,
                session: Some(sid),
                text,
                reasoning: reasoning_out,
            },
            keep,
        };
    }

    // 异常退出或报错：回收 stderr 摘要并丢弃该进程（不放回池中）。
    let stderr_handle = conn.stderr.take();
    conn.child.kill();
    let _ = conn.child.wait().await;
    drop(conn);
    let said = match stderr_handle {
        Some(mut task) => match tokio::time::timeout(DRAIN, &mut task).await {
            Ok(said) => said.unwrap_or_default(),
            Err(_) => {
                task.abort();
                String::new()
            }
        },
        None => String::new(),
    };
    let end = match failure {
        Some(reason) => TurnEnd::Failed(reason),
        None => {
            let tail = tail(said.trim(), STDERR_TAIL);
            TurnEnd::Failed(if tail.is_empty() {
                "ACP Agent 提前退出了".into()
            } else {
                format!("ACP Agent 提前退出了：{tail}")
            })
        }
    };
    AcpTurnOutcome::Done {
        finished: Finished {
            end,
            session: Some(sid),
            text,
            reasoning: reasoning_out,
        },
        keep: None,
    }
}

pub(crate) fn tail(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let rest: String = text.chars().skip(count - max).collect();
    format!("…{rest}")
}

pub(crate) fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}

/// 一行 JSON 里有哪些事。不认识的行什么都不是。
fn parse(harness: Harness, value: &Value) -> Vec<Event> {
    match harness {
        Harness::ClaudeCode => parse_claude(value),
        Harness::Codex => parse_codex(value),
        Harness::OpenCode => parse_opencode(value),
        Harness::Gemini => parse_gemini(value),
        Harness::Kimi => parse_kimi(value),
        Harness::Command | Harness::Builtin | Harness::Acp => parse_plain(value),
    }
}

/// Claude Code 的工具换成 Myriad 的叫法，标题挑最有用的那个参数。
fn claude_tool(name: &str, input: &Value) -> (String, String) {
    let pick = |keys: &[&str]| {
        keys.iter()
            .map(|k| str_of(input, k))
            .find(|v| !v.is_empty())
            .unwrap_or("")
            .to_string()
    };
    let (tool, detail) = match name {
        "Bash" => ("shell", pick(&["command"])),
        "Read" => ("read", pick(&["file_path"])),
        "Write" => ("write", pick(&["file_path"])),
        "Edit" | "MultiEdit" | "NotebookEdit" => ("edit", pick(&["file_path", "notebook_path"])),
        "Glob" | "Grep" | "LS" => ("search", pick(&["pattern", "path"])),
        "WebFetch" | "WebSearch" => ("web_fetch", pick(&["url", "query"])),
        "Task" | "Agent" => ("bot", pick(&["description"])),
        other => (other, String::new()),
    };
    let title = if detail.is_empty() {
        name.to_string()
    } else {
        format!("{name} {detail}")
    };
    (tool.to_string(), title)
}

fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                part["text"]
                    .as_str()
                    .or_else(|| part["content"]["text"].as_str())
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

pub(crate) fn parse_claude(value: &Value) -> Vec<Event> {
    let mut events = vec![];
    if let Some(id) = value["session_id"].as_str().filter(|id| !id.is_empty()) {
        events.push(Event::Session(id.to_string()));
    }
    let parts = value["message"]["content"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    match str_of(value, "type") {
        // `--include-partial-messages` 带来的实时文字/思考增量。
        "stream_event" => {
            let ev = &value["event"];
            if str_of(ev, "type") == "content_block_delta" {
                let delta = &ev["delta"];
                match str_of(delta, "type") {
                    "text_delta" => {
                        let t = str_of(delta, "text");
                        if !t.is_empty() {
                            events.push(Event::TextDelta(t.to_string()));
                        }
                    }
                    "thinking_delta" => {
                        let t = str_of(delta, "thinking");
                        if !t.is_empty() {
                            events.push(Event::Reasoning(t.to_string()));
                        }
                    }
                    _ => {}
                }
            }
        }
        "assistant" => {
            // 过滤 Claude Code 续聊时的合成占位回复。
            if str_of(&value["message"], "model") == "<synthetic>" {
                return events;
            }
            if value["isApiErrorMessage"].as_bool().unwrap_or(false)
                || value.get("error").is_some_and(|e| !e.is_null())
            {
                let msg = str_of(value, "error");
                let text = parts
                    .iter()
                    .filter_map(|p| p["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                let reason = if !text.is_empty() {
                    text
                } else if !msg.is_empty() {
                    msg.to_string()
                } else {
                    "Claude Code 接口报错".into()
                };
                events.push(Event::Failed(reason));
                return events;
            }
            for part in &parts {
                match str_of(part, "type") {
                    "thinking" => {
                        let t = str_of(part, "thinking");
                        if !t.is_empty() {
                            events.push(Event::Reasoning(t.to_string()));
                        }
                    }
                    "text" => events.push(Event::Text(str_of(part, "text").to_string())),
                    "tool_use" => {
                        let (tool, title) = claude_tool(str_of(part, "name"), &part["input"]);
                        events.push(Event::Started {
                            id: str_of(part, "id").to_string(),
                            tool,
                            title,
                        });
                    }
                    _ => {}
                }
            }
        }
        "user" => {
            for part in parts.iter().filter(|p| str_of(p, "type") == "tool_result") {
                events.push(Event::Ended {
                    id: str_of(part, "tool_use_id").to_string(),
                    tool: "tool".into(),
                    title: "工具".into(),
                    ok: !part["is_error"].as_bool().unwrap_or(false),
                    output: content_text(&part["content"]),
                });
            }
        }
        "result" => {
            if let Some(u) = value.get("usage").filter(|v| v.is_object()) {
                let uncached = u["input_tokens"].as_u64().unwrap_or(0);
                let output_tokens = u["output_tokens"].as_u64().unwrap_or(0);
                let cached_tokens = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                let cache_write_tokens = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                let input_tokens = uncached + cached_tokens + cache_write_tokens;
                if input_tokens + output_tokens > 0 {
                    let model = match str_of(value, "model") {
                        "" => "claude-code",
                        m => m,
                    };
                    events.push(Event::Usage {
                        model: model.to_string(),
                        usage: Usage {
                            input_tokens,
                            output_tokens,
                            cached_tokens,
                            cache_write_tokens,
                        },
                    });
                }
            }
            if value["is_error"].as_bool().unwrap_or(false) {
                let said = str_of(value, "result");
                let reason = if said.is_empty() {
                    format!("Claude Code 没做完（{}）", str_of(value, "subtype"))
                } else {
                    said.to_string()
                };
                events.push(Event::Failed(reason));
            }
        }
        _ => {}
    }
    events
}

pub(crate) fn codex_item(item: &Value) -> Option<(String, String)> {
    let kind = str_of(item, "type");
    Some(match kind {
        "command_execution" | "commandExecution" => {
            let cmd = match str_of(item, "command") {
                "" => str_of(item, "cmd"),
                c => c,
            };
            ("shell".into(), cmd.to_string())
        }
        "file_change" | "fileChange" => {
            let paths: Vec<&str> = item["changes"]
                .as_array()
                .map(|c| c.iter().filter_map(|c| c["path"].as_str()).collect())
                .unwrap_or_default();
            ("edit".into(), format!("改文件 {}", paths.join("、")))
        }
        "mcp_tool_call" | "mcpToolCall" => {
            let tool = match str_of(item, "tool") {
                "" => str_of(item, "name"),
                t => t,
            };
            (
                tool.to_string(),
                format!("{} {tool}", str_of(item, "server")),
            )
        }
        "web_search" | "webSearch" => (
            "web_fetch".into(),
            format!("搜索 {}", str_of(item, "query")),
        ),
        "plan" | "update_plan" => ("todo".into(), "更新计划".into()),
        _ => return None,
    })
}

fn parse_codex(value: &Value) -> Vec<Event> {
    let item = &value["item"];
    let id = str_of(item, "id").to_string();
    match str_of(value, "type") {
        "thread.started" => vec![Event::Session(str_of(value, "thread_id").to_string())],
        "item.started" => codex_item(item)
            .map(|(tool, title)| vec![Event::Started { id, tool, title }])
            .unwrap_or_default(),
        "item.completed" => {
            match str_of(item, "type") {
                "agent_message" => return vec![Event::Text(str_of(item, "text").to_string())],
                "reasoning" => {
                    let t = str_of(item, "text");
                    return if t.is_empty() {
                        vec![]
                    } else {
                        vec![Event::Reasoning(t.to_string())]
                    };
                }
                _ => {}
            }
            let Some((tool, title)) = codex_item(item) else {
                return vec![];
            };
            let status = str_of(item, "status");
            let has_error = item.get("error").is_some_and(|e| !e.is_null());
            let ok = !has_error
                && !matches!(status, "failed" | "error" | "declined" | "cancelled")
                && match item["exit_code"]
                    .as_i64()
                    .or_else(|| item["exitCode"].as_i64())
                {
                    Some(code) => code == 0,
                    None => true,
                };
            let output = [
                str_of(item, "aggregated_output"),
                str_of(item, "aggregatedOutput"),
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
            vec![Event::Ended {
                id,
                tool,
                title,
                ok,
                output,
            }]
        }
        "turn.completed" => {
            let Some(u) = value.get("usage").filter(|v| v.is_object()) else {
                return vec![];
            };
            let input_tokens = u["input_tokens"].as_u64().unwrap_or(0);
            let output_tokens = u["output_tokens"].as_u64().unwrap_or(0);
            let cached_tokens = u["cached_input_tokens"].as_u64().unwrap_or(0);
            if input_tokens + output_tokens + cached_tokens == 0 {
                return vec![];
            }
            vec![Event::Usage {
                model: "codex".into(),
                usage: Usage {
                    input_tokens,
                    output_tokens,
                    cached_tokens,
                    cache_write_tokens: 0,
                },
            }]
        }
        "turn.failed" => vec![Event::Failed(
            str_of(&value["error"], "message").to_string(),
        )],
        "error" => vec![Event::Failed(str_of(value, "message").to_string())],
        _ => vec![],
    }
}

fn parse_opencode(value: &Value) -> Vec<Event> {
    let part = &value["part"];
    let mut events = vec![];
    let session = [str_of(value, "sessionID"), str_of(part, "sessionID")]
        .into_iter()
        .find(|id| !id.is_empty());
    if let Some(id) = session {
        events.push(Event::Session(id.to_string()));
    }
    match str_of(value, "type") {
        "text" => events.push(Event::Text(str_of(part, "text").to_string())),
        "reasoning" => {
            let t = str_of(part, "text");
            if !t.is_empty() {
                events.push(Event::Reasoning(t.to_string()));
            }
        }
        "tool_use" => {
            let state = &part["state"];
            let tool = str_of(part, "tool").to_string();
            let title = match str_of(state, "title") {
                "" => tool.clone(),
                title => format!("{tool} {title}"),
            };
            let id = match str_of(part, "callID") {
                "" => str_of(part, "id").to_string(),
                id => id.to_string(),
            };
            match str_of(state, "status") {
                "completed" | "error" => events.push(Event::Ended {
                    id,
                    tool,
                    title,
                    ok: str_of(state, "status") == "completed",
                    output: match str_of(state, "output") {
                        "" => str_of(state, "error").to_string(),
                        out => out.to_string(),
                    },
                }),
                _ => events.push(Event::Started { id, tool, title }),
            }
        }
        "step_finish" => {
            let tokens = &part["tokens"];
            let uncached = tokens["input"].as_u64().unwrap_or(0);
            let output_tokens = tokens["output"].as_u64().unwrap_or(0);
            let cached_tokens = tokens["cache"]["read"].as_u64().unwrap_or(0);
            let cache_write_tokens = tokens["cache"]["write"].as_u64().unwrap_or(0);
            let input_tokens = uncached + cached_tokens + cache_write_tokens;
            if input_tokens + output_tokens > 0 {
                events.push(Event::Usage {
                    model: "opencode".into(),
                    usage: Usage {
                        input_tokens,
                        output_tokens,
                        cached_tokens,
                        cache_write_tokens,
                    },
                });
            }
        }
        "error" => {
            let error = &value["error"];
            let reason = [
                str_of(&error["data"], "message"),
                str_of(error, "message"),
                error.as_str().unwrap_or(""),
            ]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or("OpenCode 出错了");
            events.push(Event::Failed(reason.to_string()));
        }
        _ => {}
    }
    events
}

/// Gemini CLI (`gemini -p ... --output-format stream-json`) 的事件解析。
fn parse_gemini(value: &Value) -> Vec<Event> {
    let mut events = vec![];
    if let Some(id) = value["session_id"].as_str().filter(|s| !s.is_empty()) {
        events.push(Event::Session(id.to_string()));
    }
    match str_of(value, "type") {
        "message" if str_of(value, "role") == "assistant" => {
            let content = str_of(value, "content");
            if !content.is_empty() {
                if value["delta"].as_bool().unwrap_or(false) {
                    events.push(Event::TextDelta(content.to_string()));
                } else {
                    events.push(Event::Text(content.to_string()));
                }
            }
        }
        "tool_use" => {
            let name = str_of(value, "tool_name");
            let id = str_of(value, "tool_id").to_string();
            let (tool, title) = claude_tool(name, &value["parameters"]);
            events.push(Event::Started { id, tool, title });
        }
        "tool_result" => {
            let id = str_of(value, "tool_id").to_string();
            let ok = str_of(value, "status") != "error";
            let output = str_of(value, "output").to_string();
            events.push(Event::Ended {
                id,
                tool: "tool".into(),
                title: "工具".into(),
                ok,
                output,
            });
        }
        "result" => {
            if let Some(stats) = value.get("stats").filter(|v| v.is_object()) {
                let input_tokens = stats["input_tokens"].as_u64().unwrap_or(0);
                let output_tokens = stats["output_tokens"].as_u64().unwrap_or(0);
                let cached_tokens = stats["cached"].as_u64().unwrap_or(0);
                if input_tokens + output_tokens + cached_tokens > 0 {
                    events.push(Event::Usage {
                        model: "gemini-cli".into(),
                        usage: Usage {
                            input_tokens,
                            output_tokens,
                            cached_tokens,
                            cache_write_tokens: 0,
                        },
                    });
                }
            }
            if str_of(value, "status") == "error" {
                let reason = str_of(&value["error"], "message");
                events.push(Event::Failed(if reason.is_empty() {
                    "Gemini CLI 执行失败".into()
                } else {
                    reason.to_string()
                }));
            }
        }
        "error" => {
            let reason = str_of(value, "message");
            if !reason.is_empty() {
                events.push(Event::Failed(reason.to_string()));
            }
        }
        _ => {}
    }
    events
}

/// Kimi Code (`kimi --print --output-format stream-json`) 的事件解析。
fn parse_kimi(value: &Value) -> Vec<Event> {
    // Kimi Code stream-json 兼容 Claude / OpenAI 风格消息块，同时支持 role="assistant" 顶层块。
    let mut events = parse_claude(value);
    if !events.is_empty() {
        return events;
    }
    if str_of(value, "role") == "assistant" {
        let content = content_text(&value["content"]);
        if !content.is_empty() {
            events.push(Event::Text(content));
        }
    }
    events
}

/// 自定义命令的格式：一行一个 JSON。
///
/// - `{"type":"session","id":"…"}` 会话号，下次通过环境变量 `SA_HARNESS_SESSION` 交回来
/// - `{"type":"text","text":"…"}` 说的话
/// - `{"type":"thinking","text":"…"}` 思考过程
/// - `{"type":"step","id":"1","tool":"shell","title":"cargo test"}` 开始一步
/// - `{"type":"step_end","id":"1","ok":true,"output":"…"}` 这一步结束
/// - `{"type":"error","message":"…"}` 做不下去了
fn parse_plain(value: &Value) -> Vec<Event> {
    let id = str_of(value, "id").to_string();
    match str_of(value, "type") {
        "session" => vec![Event::Session(id)],
        "text" => vec![Event::Text(str_of(value, "text").to_string())],
        "thinking" => vec![Event::Reasoning(str_of(value, "text").to_string())],
        "step" => vec![Event::Started {
            id,
            tool: str_of(value, "tool").to_string(),
            title: str_of(value, "title").to_string(),
        }],
        "step_end" => vec![Event::Ended {
            id,
            tool: str_of(value, "tool").to_string(),
            title: str_of(value, "title").to_string(),
            ok: value["ok"].as_bool().unwrap_or(true),
            output: str_of(value, "output").to_string(),
        }],
        "error" => vec![Event::Failed(str_of(value, "message").to_string())],
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn relative_programs_with_a_directory_start_from_the_desk() {
        let desk = tempfile::tempdir().unwrap();
        std::fs::create_dir(desk.path().join("scripts")).unwrap();
        std::fs::write(desk.path().join("scripts/run"), "").unwrap();
        assert_eq!(
            find_program("./scripts/run", desk.path()),
            Some(desk.path().join("./scripts/run"))
        );
        assert_eq!(find_program("./scripts/missing", desk.path()), None);
    }

    #[test]
    fn split_command_handles_single_and_double_quotes() {
        assert_eq!(
            split_command(r#"claude --model "claude-sonnet" --system-prompt 'hi there'"#),
            vec![
                "claude",
                "--model",
                "claude-sonnet",
                "--system-prompt",
                "hi there"
            ]
        );
    }

    #[test]
    fn codex_resume_places_subcommand_right_after_exec() {
        let desk = tempfile::tempdir().unwrap();
        let fake = desk.path().join("codex-bin");
        std::fs::write(&fake, "").unwrap();
        let launch = Launch {
            harness: Harness::Codex,
            command: &format!("{} -m o3", fake.display()),
            dir: desk.path(),
            prompt: "hi",
            session: Some("thr_123"),
            permission: PermissionMode::Full,
            mcp_servers: &[],
            acp_pool: None,
            codex_pool: None,
            claude_pool: None,
            acp_scope: None,
            interactive_approvals: true,
        };
        let (_, args, stdin, _guard) = plan(&launch).unwrap();
        assert!(stdin);
        assert_eq!(args[0], "exec");
        assert_eq!(args[1], "resume");
        assert!(args.contains(&"--all".to_string()));
        assert!(args.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
        assert_eq!(args[args.len() - 2], "thr_123");
        assert_eq!(args[args.len() - 1], "-");
    }

    #[test]
    fn claude_full_permission_passes_skip_permissions_and_mcp_config() {
        let desk = tempfile::tempdir().unwrap();
        let fake = desk.path().join("claude-bin");
        std::fs::write(&fake, "").unwrap();
        let specs = vec![McpSpec {
            name: "wiki".into(),
            url: Some("https://wiki.example.com/mcp".into()),
            headers: vec![("Authorization".into(), "Bearer tok".into())],
            ..Default::default()
        }];
        let launch = Launch {
            harness: Harness::ClaudeCode,
            command: &fake.display().to_string(),
            dir: desk.path(),
            prompt: "hi",
            session: None,
            permission: PermissionMode::Full,
            mcp_servers: &specs,
            acp_pool: None,
            codex_pool: None,
            claude_pool: None,
            acp_scope: None,
            interactive_approvals: true,
        };
        let (_, args, _, guard) = plan(&launch).unwrap();
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(args.contains(&"--include-partial-messages".to_string()));
        let idx = args.iter().position(|a| a == "--mcp-config").unwrap();
        let cfg_path = PathBuf::from(&args[idx + 1]);
        let content = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(content.contains("wiki.example.com/mcp"), "{content}");
        drop(guard);
        assert!(!cfg_path.exists());

        let acp_list = acp_mcp_servers(&specs);
        assert_eq!(acp_list.len(), 1);
        assert_eq!(acp_list[0]["name"], "wiki");
    }

    #[test]
    fn approvals_are_kept_per_harness_and_command() {
        let first = approval_key("b1", Harness::Command, "a.sh");
        assert_eq!(first, approval_key("b1", Harness::Command, " a.sh "));
        assert_ne!(first, approval_key("b1", Harness::Command, "b.sh"));
        assert_ne!(first, approval_key("b1", Harness::Codex, "a.sh"));
    }

    #[test]
    fn parses_claude_codex_opencode_and_gemini_events() {
        let claude = parse(
            Harness::ClaudeCode,
            &json!({
                "type": "assistant",
                "session_id": "s1",
                "message": {
                    "content": [
                        {"type": "thinking", "thinking": "先查一下目录结构"},
                        {"type": "text", "text": "先看一下"},
                        {"type": "tool_use", "id": "u1", "name": "Read", "input": {"file_path": "a.rs"}}
                    ]
                }
            }),
        );
        assert!(claude.contains(&Event::Session("s1".into())));
        assert!(claude.contains(&Event::Reasoning("先查一下目录结构".into())));
        assert!(claude.contains(&Event::Text("先看一下".into())));
        assert!(claude.contains(&Event::Started {
            id: "u1".into(),
            tool: "read".into(),
            title: "Read a.rs".into(),
        }));

        let claude_usage = parse(
            Harness::ClaudeCode,
            &json!({
                "type": "result",
                "usage": {
                    "input_tokens": 120,
                    "output_tokens": 45,
                    "cache_read_input_tokens": 30,
                    "cache_creation_input_tokens": 20
                }
            }),
        );
        assert!(matches!(
            claude_usage.as_slice(),
            [Event::Usage { usage, .. }]
                if usage.input_tokens == 170
                    && usage.output_tokens == 45
                    && usage.cached_tokens == 30
                    && usage.cache_write_tokens == 20
        ));

        let codex = parse(
            Harness::Codex,
            &json!({
                "type": "item.completed",
                "item": {"id": "c1", "type": "command_execution", "command": "cargo test", "exit_code": 1, "aggregated_output": "failed"}
            }),
        );
        assert_eq!(
            codex,
            vec![Event::Ended {
                id: "c1".into(),
                tool: "shell".into(),
                title: "cargo test".into(),
                ok: false,
                output: "failed".into(),
            }]
        );

        let codex_usage = parse(
            Harness::Codex,
            &json!({
                "type": "turn.completed",
                "usage": {"input_tokens": 500, "cached_input_tokens": 200, "output_tokens": 80}
            }),
        );
        assert!(matches!(
            codex_usage.as_slice(),
            [Event::Usage { usage, .. }] if usage.input_tokens == 500 && usage.cached_tokens == 200
        ));

        let opencode = parse(
            Harness::OpenCode,
            &json!({
                "type": "tool_use",
                "sessionID": "oc1",
                "part": {"callID": "p1", "tool": "bash", "state": {"status": "completed", "title": "ls", "output": "ok"}}
            }),
        );
        assert!(opencode.contains(&Event::Session("oc1".into())));
        assert!(opencode.contains(&Event::Ended {
            id: "p1".into(),
            tool: "bash".into(),
            title: "bash ls".into(),
            ok: true,
            output: "ok".into(),
        }));

        let gemini = parse(
            Harness::Gemini,
            &json!({
                "type": "message",
                "session_id": "g1",
                "role": "assistant",
                "content": "hello",
                "delta": true
            }),
        );
        assert!(gemini.contains(&Event::Session("g1".into())));
        assert!(gemini.contains(&Event::TextDelta("hello".into())));
    }

    #[test]
    fn extract_semver_finds_version_in_cli_output() {
        assert_eq!(
            extract_semver("claude 2.1.88 (Claude Code)"),
            Some("2.1.88".into())
        );
        assert_eq!(extract_semver("codex-cli v0.166.0"), Some("0.166.0".into()));
        assert_eq!(extract_semver("no version here"), None);
    }

    #[tokio::test]
    async fn missing_harness_fails_cleanly_with_its_name() {
        struct Null;
        #[async_trait::async_trait]
        impl Observer for Null {
            async fn text_delta(&self, _: &str) {}
            async fn tool_started(&self, _: &str, _: &str, _: &str, _: &str) {}
            async fn tool_finished(&self, _: &str, _: StepEnd, _: &str) {}
            async fn usage(&self, _: &str, _: sa_llm::Usage) {}
        }
        let dir = tempfile::tempdir().unwrap();
        let launch = Launch {
            harness: Harness::Command,
            command: "definitely-not-a-real-harness-binary",
            dir: dir.path(),
            prompt: "hi",
            session: None,
            permission: PermissionMode::Ask,
            mcp_servers: &[],
            acp_pool: None,
            codex_pool: None,
            claude_pool: None,
            acp_scope: None,
            interactive_approvals: true,
        };
        let done = run(&launch, &Null, &CancellationToken::new()).await;
        assert!(matches!(done.end, TurnEnd::Failed(msg) if msg.contains("找不到")));

        let codex = Launch {
            harness: Harness::Codex,
            command: "definitely-not-codex",
            dir: dir.path(),
            prompt: "hi",
            session: None,
            permission: PermissionMode::Ask,
            mcp_servers: &[],
            acp_pool: None,
            codex_pool: None,
            claude_pool: None,
            acp_scope: None,
            interactive_approvals: true,
        };
        let done = run(&codex, &Null, &CancellationToken::new()).await;
        assert!(matches!(done.end, TurnEnd::Failed(msg) if msg.contains("Codex")));
    }

    #[test]
    fn acp_mode_selection_switches_into_and_out_of_plan_mode() {
        let (modes, current) = parse_acp_modes(&json!({
            "modes": {
                "currentModeId": "code",
                "availableModes": [
                    { "id": "code", "name": "Code" },
                    { "id": "plan", "name": "Plan" }
                ]
            }
        }));
        assert_eq!(modes, vec!["code", "plan"]);
        assert_eq!(current.as_deref(), Some("code"));
        assert_eq!(
            desired_acp_mode(&modes, current.as_deref(), PermissionMode::Plan).as_deref(),
            Some("plan")
        );
        assert_eq!(
            desired_acp_mode(&modes, Some("plan"), PermissionMode::Plan),
            None
        );
        assert_eq!(
            desired_acp_mode(&modes, Some("plan"), PermissionMode::Ask).as_deref(),
            Some("code")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn acp_pool_reuses_live_process_across_turns_and_enforces_plan_mode() {
        use std::os::unix::fs::PermissionsExt;

        struct Null;
        #[async_trait::async_trait]
        impl Observer for Null {
            async fn text_delta(&self, _: &str) {}
            async fn tool_started(&self, _: &str, _: &str, _: &str, _: &str) {}
            async fn tool_finished(&self, _: &str, _: StepEnd, _: &str) {}
            async fn usage(&self, _: &str, _: sa_llm::Usage) {}
        }

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-acp.py");
        std::fs::write(
            &script,
            r#"#!/usr/bin/env python3
import json, os, sys
pid = os.getpid()
turn = 0
current_mode = "code"
for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    msg = json.loads(raw)
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        print(json.dumps({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": 1}}), flush=True)
    elif method in ("session/new", "session/load"):
        print(json.dumps({
            "jsonrpc": "2.0",
            "id": mid,
            "result": {
                "sessionId": f"sess-{pid}",
                "modes": {"currentModeId": "code", "availableModes": [{"id": "code"}, {"id": "plan"}]}
            }
        }), flush=True)
    elif method == "session/set_mode":
        current_mode = msg["params"]["modeId"]
        print(json.dumps({"jsonrpc": "2.0", "id": mid, "result": {}}), flush=True)
    elif method == "session/prompt":
        turn += 1
        text = msg["params"]["prompt"][0]["text"]
        if "self-plan" in text:
            current_mode = "plan"
            print(json.dumps({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {"update": {"sessionUpdate": "current_mode_update", "currentModeId": "plan"}}
            }), flush=True)
            out = f"pid={pid},turn={turn},mode={current_mode}"
        elif "try-write" in text:
            print(json.dumps({
                "jsonrpc": "2.0",
                "id": 999,
                "method": "session/request_permission",
                "params": {
                    "toolCall": {"toolCallId": "tc1", "kind": "edit", "title": "edit file"},
                    "options": [
                        {"optionId": "allow", "kind": "allow_once"},
                        {"optionId": "deny", "kind": "reject_once"}
                    ]
                }
            }), flush=True)
            resp = json.loads(sys.stdin.readline())
            chosen = resp["result"]["outcome"].get("optionId", "none")
            out = f"pid={pid},turn={turn},perm={chosen},mode={current_mode}"
        else:
            out = f"pid={pid},turn={turn},mode={current_mode}"
        print(json.dumps({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"update": {"sessionUpdate": "agent_message_chunk", "content": {"text": out}}}
        }), flush=True)
        print(json.dumps({"jsonrpc": "2.0", "id": mid, "result": {"stopReason": "end_turn"}}), flush=True)
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let pool = AcpPool::default();
        let cmd = script.display().to_string();
        let first = run(
            &Launch {
                harness: Harness::Acp,
                command: &cmd,
                dir: dir.path(),
                prompt: "hello",
                session: None,
                permission: PermissionMode::Ask,
                mcp_servers: &[],
                acp_pool: Some(&pool),
                codex_pool: None,
                claude_pool: None,
                acp_scope: Some("room:r1:bot:b1"),
                interactive_approvals: true,
            },
            &Null,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(first.end, TurnEnd::Completed);
        assert_eq!(pool.len(), 1);
        assert!(first.text.contains("turn=1"), "{}", first.text);

        // 第二轮复用同一个常驻进程（pid 相同、turn 递增），且 Plan 模式自动拒绝 edit 权限请求。
        let second = run(
            &Launch {
                harness: Harness::Acp,
                command: &cmd,
                dir: dir.path(),
                prompt: "try-write",
                session: first.session.as_deref(),
                permission: PermissionMode::Plan,
                mcp_servers: &[],
                acp_pool: Some(&pool),
                codex_pool: None,
                claude_pool: None,
                acp_scope: Some("room:r1:bot:b1"),
                interactive_approvals: true,
            },
            &Null,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(second.end, TurnEnd::Completed);
        assert_eq!(second.session, first.session);
        assert!(second.text.contains("turn=2"), "{}", second.text);
        assert!(second.text.contains("perm=deny"), "{}", second.text);
        assert!(second.text.contains("mode=plan"), "{}", second.text);

        // 第三轮回到 Ask 模式且 Agent 在本轮主动切回 plan（通过 current_mode_update 通知），
        // 第四轮 Ask 必须感知到该通知并重新发送 session/set_mode 切回 code。
        let third = run(
            &Launch {
                harness: Harness::Acp,
                command: &cmd,
                dir: dir.path(),
                prompt: "self-plan",
                session: second.session.as_deref(),
                permission: PermissionMode::Ask,
                mcp_servers: &[],
                acp_pool: Some(&pool),
                codex_pool: None,
                claude_pool: None,
                acp_scope: Some("room:r1:bot:b1"),
                interactive_approvals: true,
            },
            &Null,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(third.end, TurnEnd::Completed);
        assert!(third.text.contains("turn=3,mode=plan"), "{}", third.text);

        let fourth = run(
            &Launch {
                harness: Harness::Acp,
                command: &cmd,
                dir: dir.path(),
                prompt: "back-to-ask",
                session: third.session.as_deref(),
                permission: PermissionMode::Ask,
                mcp_servers: &[],
                acp_pool: Some(&pool),
                codex_pool: None,
                claude_pool: None,
                acp_scope: Some("room:r1:bot:b1"),
                interactive_approvals: true,
            },
            &Null,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(fourth.end, TurnEnd::Completed);
        assert!(fourth.text.contains("turn=4,mode=code"), "{}", fourth.text);

        // session: None 时不得复用旧会话（防止命令切换后复活旧上下文）。
        let fresh = run(
            &Launch {
                harness: Harness::Acp,
                command: &cmd,
                dir: dir.path(),
                prompt: "brand-new",
                session: None,
                permission: PermissionMode::Ask,
                mcp_servers: &[],
                acp_pool: Some(&pool),
                codex_pool: None,
                claude_pool: None,
                acp_scope: Some("room:r1:bot:b1"),
                interactive_approvals: true,
            },
            &Null,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(fresh.end, TurnEnd::Completed);
        assert_ne!(fresh.session, first.session);
        assert!(fresh.text.contains("turn=1"), "{}", fresh.text);

        // 主动空闲 TTL 回收：不发生新的 checkout 时也自动回收过期 ACP 进程。
        let (ttl_key, conn) = pool
            .entries
            .lock()
            .unwrap()
            .drain()
            .next()
            .expect("pooled acp connection should be present");
        pool.put_with_ttl(ttl_key, conn, Duration::from_millis(80));
        assert_eq!(pool.len(), 1);
        tokio::time::sleep(Duration::from_millis(160)).await;
        assert_eq!(
            pool.len(),
            0,
            "idle AcpPool entry must be actively evicted after TTL without another checkout"
        );

        pool.evict_all();
        assert_eq!(pool.len(), 0);
    }
}
