//! MCP 客户端：把外部 MCP 服务提供的工具接进来。
//!
//! 两种连法：本机启动一个进程，通过标准输入输出说话（一行一条 JSON-RPC 消息）；
//! 或者通过网址连一个远程的服务（见 [`http`]）。
//! 只用到「工具」这一部分（`tools/list`、`tools/call`）；服务提供的资源、提示词模板不管。
//!
//! 用法：[`McpServer::new`] 建一个服务，[`McpServer::connect`] 连上并拿到工具清单，
//! [`McpServer::tools`] 把清单变成可以注册的 [`McpTool`]。连接断了，下一次调用时自动重连一次。

mod http;

use crate::process::Tree;
use crate::{Permission, Tool, ToolCtx, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// 我们说的协议版本。
pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// 一条消息最多这么大。
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
/// 一个服务最多接这么多个工具。
pub const MAX_TOOLS_PER_SERVER: usize = 100;
/// 工具清单最多翻这么多页。
const MAX_PAGES: usize = 20;
const MAX_DESCRIPTION_CHARS: usize = 1024;
/// 告诉服务「不用做了」最多等这么久：它不读输入的时候不能一直卡着。
const CANCEL_NOTICE_TIMEOUT: Duration = Duration::from_secs(5);
/// 审批卡上最多显示这么多字的参数。
const MAX_SHOWN_ARGUMENT_CHARS: usize = 600;
/// 模型服务对工具名的限制。
const MAX_TOOL_NAME_CHARS: usize = 64;
/// 服务出错时，把它最后打到标准错误的这么多字带给人看。
const STDERR_TAIL_BYTES: usize = 2000;

/// 怎么连一个 MCP 服务：写了 `url` 的通过网址连，没写的在本机启动 `command`。
#[derive(Clone, Default, PartialEq, Eq)]
pub struct McpSpec {
    /// 服务的名字，工具名里用：`mcp__<名字>__<工具>`。
    pub name: String,
    /// 远程服务的网址。
    pub url: Option<String>,
    /// 连远程服务时每个请求带的请求头（令牌在这里）。
    pub headers: Vec<(String, String)>,
    pub command: String,
    pub args: Vec<String>,
    /// 给它的环境变量。除了这些和几个基本的（PATH、HOME 之类），别的不传给它。
    pub env: Vec<(String, String)>,
    /// 在哪个目录里启动。
    pub cwd: Option<PathBuf>,
    /// 这些工具不用问人，直接执行。写服务自己的工具名。
    pub auto_approve: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct McpError(pub String);

/// 等多久算没有回应。
#[derive(Debug, Clone, Copy)]
pub struct McpTimeouts {
    /// 启动、握手、取工具清单的每一步。
    pub start: Duration,
    /// 一次工具调用。
    pub call: Duration,
}

impl Default for McpTimeouts {
    fn default() -> Self {
        Self {
            start: Duration::from_secs(60),
            call: Duration::from_secs(120),
        }
    }
}

/// 服务报出来的一个工具。
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

type Reader = Box<dyn AsyncRead + Send + Unpin>;
type Writer = Box<dyn AsyncWrite + Send + Unpin>;

/// 和服务之间的一条线。
pub struct Pipe {
    pub reader: Reader,
    pub writer: Writer,
    /// 线那头的东西（进程）。线断开时一起丢掉。
    pub keep: Option<Box<dyn Send>>,
    /// 服务打到标准错误的最后一段。
    pub stderr: Arc<Mutex<String>>,
}

impl Pipe {
    pub fn new(
        reader: impl AsyncRead + Send + Unpin + 'static,
        writer: impl AsyncWrite + Send + Unpin + 'static,
    ) -> Self {
        Self {
            reader: Box::new(reader),
            writer: Box::new(writer),
            keep: None,
            stderr: Arc::default(),
        }
    }
}

/// 接通一条线的办法。连接断了要重连时会再调一次。
pub type Dialer =
    Box<dyn Fn() -> Pin<Box<dyn Future<Output = Result<Pipe, McpError>> + Send>> + Send + Sync>;

type Answer = Result<Value, String>;
type Waiting = Arc<Mutex<HashMap<u64, oneshot::Sender<Answer>>>>;

struct Connection {
    writer: Arc<tokio::sync::Mutex<Writer>>,
    waiting: Waiting,
    next_id: AtomicU64,
    closed: Arc<AtomicBool>,
    stderr: Arc<Mutex<String>>,
    reader: tokio::task::JoinHandle<()>,
    _keep: Mutex<Option<Box<dyn Send>>>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl Connection {
    fn open(pipe: Pipe) -> Self {
        let writer = Arc::new(tokio::sync::Mutex::new(pipe.writer));
        let waiting = Waiting::default();
        let closed = Arc::new(AtomicBool::new(false));
        let reader = tokio::spawn(read_loop(
            pipe.reader,
            writer.clone(),
            waiting.clone(),
            closed.clone(),
        ));
        Self {
            writer,
            waiting,
            next_id: AtomicU64::new(1),
            closed,
            stderr: pipe.stderr,
            reader,
            _keep: Mutex::new(pipe.keep),
        }
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn gone(&self) -> McpError {
        let tail = self.stderr.lock().unwrap().trim().to_string();
        McpError(if tail.is_empty() {
            "MCP 服务退出了，或者连接断了。".into()
        } else {
            format!("MCP 服务退出了，或者连接断了。它最后说的是：{tail}")
        })
    }

    async fn send(&self, message: &Value) -> Result<(), McpError> {
        send_line(&self.writer, message)
            .await
            .map_err(|_| self.gone())
    }

    async fn notify(&self, method: &str, params: Value) {
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        // 服务不读输入的时候不能一直卡在这里。
        let _ = tokio::time::timeout(CANCEL_NOTICE_TIMEOUT, self.send(&message)).await;
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value, McpError> {
        if self.is_closed() {
            return Err(self.gone());
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.waiting.lock().unwrap().insert(id, tx);
        // 没等到回应就不等了的（超时、人点了停止、整个调用被上层丢弃），都要告诉服务不用做了。
        let mut abandon = Abandon {
            id,
            waiting: self.waiting.clone(),
            writer: self.writer.clone(),
            closed: self.closed.clone(),
            sent: false,
            tell: true,
        };
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        // 发出去也算在时限里：服务不读输入的时候，写会一直卡着。
        let sent = &mut abandon.sent;
        let exchange = async {
            self.send(&message).await?;
            *sent = true;
            match rx.await {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(message)) => Err(McpError(message)),
                Err(_) => Err(self.gone()),
            }
        };
        tokio::select! {
            outcome = exchange => {
                // 有结果了，或者线断了：没有什么要取消的。
                abandon.tell = false;
                outcome
            }
            _ = tokio::time::sleep(timeout) => Err(McpError(format!(
                "MCP 服务等了 {} 秒没有回应。",
                timeout.as_secs_f32().ceil()
            ))),
            _ = cancel.cancelled() => Err(McpError("已停止。".into())),
        }
    }
}

/// 一次请求不再等了。丢弃时把等回应的位置收掉；需要的话告诉服务取消（另起任务去发，最多等几秒）。
struct Abandon {
    id: u64,
    waiting: Waiting,
    writer: Arc<tokio::sync::Mutex<Writer>>,
    closed: Arc<AtomicBool>,
    /// 请求整条发出去了没有。
    sent: bool,
    tell: bool,
}

impl Drop for Abandon {
    fn drop(&mut self) {
        self.waiting.lock().unwrap().remove(&self.id);
        if !self.tell {
            return;
        }
        if !self.sent {
            // 请求可能只写了半行：这条线上后面的话服务都会读错，下一次调用重新连。
            self.closed.store(true, Ordering::SeqCst);
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let writer = self.writer.clone();
        let params = json!({ "requestId": self.id, "reason": "cancelled" });
        let message =
            json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": params });
        runtime.spawn(async move {
            let _ = tokio::time::timeout(CANCEL_NOTICE_TIMEOUT, send_line(&writer, &message)).await;
        });
    }
}

async fn send_line(writer: &tokio::sync::Mutex<Writer>, message: &Value) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(message).expect("JSON 值总能序列化");
    line.push(b'\n');
    let mut writer = writer.lock().await;
    writer.write_all(&line).await?;
    writer.flush().await
}

/// 读一行（不含换行）。读到头返回 `false`；一行太长算出错。
async fn read_line(
    reader: &mut (impl AsyncBufReadExt + Unpin),
    line: &mut Vec<u8>,
) -> std::io::Result<bool> {
    line.clear();
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            return Ok(!line.is_empty());
        }
        let (taken, done) = match buffer.iter().position(|byte| *byte == b'\n') {
            Some(at) => (at + 1, true),
            None => (buffer.len(), false),
        };
        line.extend_from_slice(&buffer[..taken]);
        reader.consume(taken);
        if line.len() > MAX_LINE_BYTES {
            return Err(std::io::Error::other("一条消息太长了"));
        }
        if done {
            line.pop();
            return Ok(true);
        }
    }
}

async fn read_loop(
    reader: Reader,
    writer: Arc<tokio::sync::Mutex<Writer>>,
    waiting: Waiting,
    closed: Arc<AtomicBool>,
) {
    let mut reader = BufReader::new(reader);
    let mut line = vec![];
    while let Ok(true) = read_line(&mut reader, &mut line).await {
        // 有的服务会往标准输出打别的东西：不是 JSON 的行跳过。
        let Ok(message) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        let id = message.get("id").filter(|id| !id.is_null());
        match (message.get("method").and_then(Value::as_str), id) {
            // 服务问我们的。只会答 ping，别的都说不会。
            (Some(method), Some(id)) => {
                let reply = if method == "ping" {
                    json!({ "jsonrpc": "2.0", "id": id, "result": {} })
                } else {
                    let error = json!({ "code": -32601, "message": "Method not found" });
                    json!({ "jsonrpc": "2.0", "id": id, "error": error })
                };
                if send_line(&writer, &reply).await.is_err() {
                    break;
                }
            }
            // 通知：不用管。
            (Some(_), None) => {}
            (None, Some(id)) => {
                let Some(tx) = id
                    .as_u64()
                    .and_then(|id| waiting.lock().unwrap().remove(&id))
                else {
                    continue;
                };
                let answer = match message.get("error") {
                    Some(error) => Err(error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("MCP 服务报错了")
                        .to_string()),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = tx.send(answer);
            }
            (None, None) => {}
        }
    }
    closed.store(true, Ordering::SeqCst);
    // 丢掉发送端，等着回应的那一头就知道线断了。
    waiting.lock().unwrap().clear();
}

/// 握手，然后把工具清单取回来。
async fn handshake(conn: &Connection, timeout: Duration) -> Result<Vec<McpToolInfo>, McpError> {
    let never = CancellationToken::new();
    let hello = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": { "name": "superagent", "version": env!("CARGO_PKG_VERSION") },
    });
    let welcome = conn.request("initialize", hello, timeout, &never).await?;
    conn.notify("notifications/initialized", json!({})).await;
    if welcome.pointer("/capabilities/tools").is_none() {
        return Ok(vec![]);
    }
    let mut tools = vec![];
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let params = match &cursor {
            Some(cursor) => json!({ "cursor": cursor }),
            None => json!({}),
        };
        let page = conn.request("tools/list", params, timeout, &never).await?;
        for tool in page["tools"].as_array().into_iter().flatten() {
            let Some(name) = tool["name"].as_str().filter(|name| !name.is_empty()) else {
                continue;
            };
            tools.push(McpToolInfo {
                name: name.to_string(),
                description: tool["description"].as_str().unwrap_or("").to_string(),
                input_schema: tool["inputSchema"].clone(),
            });
        }
        cursor = page["nextCursor"].as_str().map(str::to_string);
        if cursor.is_none() || tools.len() >= MAX_TOOLS_PER_SERVER {
            break;
        }
    }
    tools.truncate(MAX_TOOLS_PER_SERVER);
    Ok(tools)
}

/// 把一次工具调用的结果变成给模型的文字。服务说失败了的算失败。
fn render(result: &Value) -> Result<String, McpError> {
    let mut parts = vec![];
    for item in result["content"].as_array().into_iter().flatten() {
        let part = match item["type"].as_str().unwrap_or("") {
            "text" => item["text"].as_str().unwrap_or("").to_string(),
            "image" => "[图片]".to_string(),
            "audio" => "[音频]".to_string(),
            "resource" => match item.pointer("/resource/text").and_then(Value::as_str) {
                Some(text) => text.to_string(),
                None => format!(
                    "[资源 {}]",
                    item.pointer("/resource/uri")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                ),
            },
            "resource_link" => format!("[资源 {}]", item["uri"].as_str().unwrap_or("")),
            other => format!("[{other}]"),
        };
        parts.push(part);
    }
    let mut text = parts.join("\n");
    if text.trim().is_empty() {
        if let Some(structured) = result.get("structuredContent").filter(|v| !v.is_null()) {
            text = structured.to_string();
        }
    }
    if result["isError"].as_bool().unwrap_or(false) {
        return Err(McpError(if text.trim().is_empty() {
            "工具说它没做成，但是没有说原因。".into()
        } else {
            text
        }));
    }
    Ok(if text.trim().is_empty() {
        "（工具没有返回内容）".into()
    } else {
        text
    })
}

/// 一个 MCP 服务。
pub struct McpServer {
    name: String,
    auto_approve: Vec<String>,
    dialer: Dialer,
    timeouts: McpTimeouts,
    conn: tokio::sync::Mutex<Option<Arc<Connection>>>,
    /// 上一次重新连接为什么没成。连上了就清掉。
    trouble: Mutex<Option<String>>,
}

impl McpServer {
    /// 配置里写的一个服务。这里还不启动、不连，[`McpServer::connect`] 时才动。
    pub fn new(spec: McpSpec) -> Self {
        let (name, auto_approve) = (spec.name.clone(), spec.auto_approve.clone());
        let dialer: Dialer = Box::new(move || {
            let spec = spec.clone();
            Box::pin(async move {
                match &spec.url {
                    Some(url) => http::dial(&spec, url),
                    None => spawn_pipe(&spec),
                }
            })
        });
        Self::with_dialer(&name, auto_approve, dialer)
    }

    /// 用给定的办法接线的服务。测试用它接一个进程内的假服务。
    pub fn with_dialer(name: &str, auto_approve: Vec<String>, dialer: Dialer) -> Self {
        Self {
            name: name.to_string(),
            auto_approve,
            dialer,
            timeouts: McpTimeouts::default(),
            conn: tokio::sync::Mutex::new(None),
            trouble: Mutex::new(None),
        }
    }

    pub fn timeouts(mut self, timeouts: McpTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    async fn dial(&self) -> Result<(Arc<Connection>, Vec<McpToolInfo>), McpError> {
        let conn = Connection::open((self.dialer)().await?);
        let tools = handshake(&conn, self.timeouts.start).await?;
        Ok((Arc::new(conn), tools))
    }

    /// 连上（已经连着的先断开），返回它提供的工具。
    pub async fn connect(&self) -> Result<Vec<McpToolInfo>, McpError> {
        let mut slot = self.conn.lock().await;
        *slot = None;
        let (conn, tools) = self.dial().await?;
        *slot = Some(conn);
        *self.trouble.lock().unwrap() = None;
        Ok(tools)
    }

    /// 连上之后又出了什么事：服务退出了还没重新启动，或者重新启动没成。一切正常是 `None`。
    pub fn problem(&self) -> Option<String> {
        if let Some(trouble) = self.trouble.lock().unwrap().clone() {
            return Some(format!("重新连接没成：{trouble}"));
        }
        let slot = self.conn.try_lock().ok()?;
        let conn = slot.as_ref().filter(|conn| conn.is_closed())?;
        Some(format!("{}下一次调用时会重新启动。", conn.gone()))
    }

    /// 断开。进程一起结束。
    pub async fn close(&self) {
        *self.conn.lock().await = None;
    }

    /// 一条能用的连接。断了的重连一次。
    async fn live(&self) -> Result<Arc<Connection>, McpError> {
        let mut slot = self.conn.lock().await;
        if let Some(conn) = slot.as_ref().filter(|conn| !conn.is_closed()) {
            return Ok(conn.clone());
        }
        *slot = None;
        let dialed = self.dial().await;
        *self.trouble.lock().unwrap() = dialed.as_ref().err().map(|error| error.0.clone());
        let (conn, _) = dialed?;
        *slot = Some(conn.clone());
        Ok(conn)
    }

    /// 调用它的一个工具。`tool` 是服务自己的工具名。
    pub async fn call(
        &self,
        tool: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<String, McpError> {
        let conn = self.live().await?;
        let params = json!({ "name": tool, "arguments": arguments });
        let result = conn
            .request("tools/call", params, self.timeouts.call, cancel)
            .await?;
        render(&result)
    }

    /// 把服务报出来的工具变成可以注册的工具。名字整理成模型服务认的样子，重名的只留第一个。
    pub fn tools(self: &Arc<Self>, infos: &[McpToolInfo]) -> Vec<McpTool> {
        let mut tools: Vec<McpTool> = vec![];
        for info in infos {
            let name = tool_name(&self.name, &info.name);
            if tools.iter().any(|tool| tool.name == name) {
                continue;
            }
            let description: String = info
                .description
                .trim()
                .chars()
                .take(MAX_DESCRIPTION_CHARS)
                .collect();
            let schema = match &info.input_schema {
                schema if schema["type"] == "object" => schema.clone(),
                _ => json!({ "type": "object" }),
            };
            tools.push(McpTool {
                name,
                remote: info.name.clone(),
                description: format!("（外部 MCP 服务「{}」提供的工具）{description}", self.name),
                schema,
                auto: self.auto_approve.iter().any(|auto| auto == &info.name),
                server: self.clone(),
            });
        }
        tools
    }
}

/// `mcp__<服务>__<工具>`，只留字母、数字、下划线和横线，最多 64 个字符。
fn tool_name(server: &str, tool: &str) -> String {
    let clean = |text: &str| -> String {
        text.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let mut name = format!("mcp__{}__{}", clean(server), clean(tool));
    name.truncate(MAX_TOOL_NAME_CHARS);
    name
}

/// 启动服务的进程。环境变量只给基本的几个和配置里写明的。
fn spawn_pipe(spec: &McpSpec) -> Result<Pipe, McpError> {
    // Windows 上 npx、uvx 这些其实是 `.cmd`：不带后缀找不到时挨个试。
    let mut programs = vec![spec.command.clone()];
    if cfg!(windows) && std::path::Path::new(&spec.command).extension().is_none() {
        for extension in ["cmd", "bat", "exe"] {
            programs.push(format!("{}.{extension}", spec.command));
        }
    }
    for program in programs {
        let mut command = Command::new(&program);
        command
            .args(&spec.args)
            .env_clear()
            .envs(base_env())
            .envs(spec.env.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        // 断开时连同它启动的子进程一起结束。
        let mut child = match Tree::spawn(command) {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(McpError(format!("启动不了「{}」：{error}", spec.command)));
            }
        };
        let stdin = child.stdin().expect("上面设置了管道");
        let stdout = child.stdout().expect("上面设置了管道");
        let mut errors = child.stderr().expect("上面设置了管道");
        let stderr = Arc::new(Mutex::new(String::new()));
        let tail = stderr.clone();
        tokio::spawn(async move {
            let mut buffer = [0u8; 1024];
            while let Ok(read @ 1..) = errors.read(&mut buffer).await {
                let mut tail = tail.lock().unwrap();
                tail.push_str(&String::from_utf8_lossy(&buffer[..read]));
                if tail.len() > STDERR_TAIL_BYTES {
                    let mut cut = tail.len() - STDERR_TAIL_BYTES;
                    while !tail.is_char_boundary(cut) {
                        cut += 1;
                    }
                    tail.drain(..cut);
                }
            }
        });
        return Ok(Pipe {
            reader: Box::new(stdout),
            writer: Box::new(stdin),
            // 先丢守卫（杀进程树），再丢进程本身。
            keep: Some(Box::new(child)),
            stderr,
        });
    }
    Err(McpError(format!(
        "找不到命令「{}」。先确认它装好了，并且在 PATH 里。",
        spec.command
    )))
}

/// 传给服务的基本环境变量：找得到程序、知道家目录和临时目录在哪就够了。
/// 别的（模型的密钥、GitHub 令牌……）不给，要给的在配置里写明。
fn base_env() -> Vec<(String, std::ffi::OsString)> {
    const NAMES: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TERM",
        "TMPDIR",
        "TZ",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
        // Windows
        "Path",
        "PATHEXT",
        "SystemRoot",
        "SystemDrive",
        "windir",
        "COMSPEC",
        "USERPROFILE",
        "USERNAME",
        "HOMEDRIVE",
        "HOMEPATH",
        "APPDATA",
        "LOCALAPPDATA",
        "ProgramData",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "TEMP",
        "TMP",
    ];
    NAMES
        .iter()
        .filter_map(|name| Some((name.to_string(), std::env::var_os(name)?)))
        .collect()
}

/// 接进来的一个 MCP 工具。
pub struct McpTool {
    name: String,
    /// 服务自己的工具名。
    remote: String,
    description: String,
    schema: Value,
    /// 不用问人。
    auto: bool,
    server: Arc<McpServer>,
}

impl McpTool {
    /// 服务自己的工具名。
    pub fn remote_name(&self) -> &str {
        &self.remote
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> String {
        self.description.clone()
    }

    fn input_schema(&self) -> Value {
        self.schema.clone()
    }

    fn title(&self, _input: &Value) -> String {
        format!("调用 {} 的 {}", self.server.name, self.remote)
    }

    async fn check_permission(&self, input: &Value, _ctx: &ToolCtx) -> Permission {
        if self.auto {
            return Permission::Allow;
        }
        // 批准之前要看得到交给它的是什么：路径、命令、发到哪去都在参数里。
        let shown = match input {
            Value::Null => "{}".to_string(),
            other => other.to_string(),
        };
        let mut arguments: String = shown.chars().take(MAX_SHOWN_ARGUMENT_CHARS).collect();
        if shown.chars().count() > MAX_SHOWN_ARGUMENT_CHARS {
            arguments.push_str("…（太长，后面的没有显示）");
        }
        Permission::Ask(format!(
            "这是外部 MCP 服务「{}」的工具，下面的参数会交给它去执行：{arguments}",
            self.server.name
        ))
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolResult {
        let arguments = match input {
            Value::Null => json!({}),
            Value::Object(_) => input,
            _ => return Err(ToolError::new("参数要是一个对象。")),
        };
        self.server
            .call(&self.remote, arguments, &ctx.cancel)
            .await
            .map_err(|error| ToolError(error.0))
    }
}

/// 一个最小的 MCP 服务，给测试和试用（`sa mcp demo-server`）。一次处理一个请求。
///
/// 工具：`echo`（原样说回来）、`add`（两个数相加）、`fail`（总是失败）、
/// `wait`（等一会儿再回答）、`quit`（不回答，直接退出）。
pub async fn run_demo_server(
    reader: impl AsyncRead + Unpin,
    mut writer: impl AsyncWrite + Unpin,
) -> std::io::Result<()> {
    let text = |text: String| json!({ "content": [{ "type": "text", "text": text }] });
    let tool = |name: &str, description: &str, properties: Value| {
        json!({
            "name": name,
            "description": description,
            "inputSchema": { "type": "object", "properties": properties },
        })
    };
    let mut reader = BufReader::new(reader);
    let mut line = vec![];
    while read_line(&mut reader, &mut line).await? {
        let Ok(message) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        // 通知和回应不用答。
        let (Some(method), Some(id)) = (message["method"].as_str(), message.get("id")) else {
            continue;
        };
        let params = &message["params"];
        let outcome = match method {
            "initialize" => Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "superagent-demo", "version": env!("CARGO_PKG_VERSION") },
            })),
            "ping" => Ok(json!({})),
            // 分两页给，客户端要会翻页。
            "tools/list" if params["cursor"].is_null() => Ok(json!({
                "tools": [
                    tool("echo", "把 text 原样说回来", json!({ "text": { "type": "string" } })),
                    tool("add", "把 a 和 b 加起来", json!({ "a": { "type": "number" }, "b": { "type": "number" } })),
                    tool("fail", "总是失败", json!({})),
                ],
                "nextCursor": "2",
            })),
            "tools/list" => Ok(json!({
                "tools": [
                    tool("wait", "等 ms 毫秒再回答", json!({ "ms": { "type": "integer" } })),
                    tool("quit", "不回答，直接退出", json!({})),
                ],
            })),
            "tools/call" => {
                let arguments = &params["arguments"];
                match params["name"].as_str().unwrap_or("") {
                    "echo" => Ok(text(arguments["text"].as_str().unwrap_or("").to_string())),
                    "add" => {
                        let number = |key: &str| arguments[key].as_f64().unwrap_or(0.0);
                        Ok(text((number("a") + number("b")).to_string()))
                    }
                    "fail" => Ok(json!({
                        "content": [{ "type": "text", "text": "这个工具总是失败" }],
                        "isError": true,
                    })),
                    "wait" => {
                        let ms = arguments["ms"].as_u64().unwrap_or(0);
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                        Ok(text(format!("等了 {ms} 毫秒")))
                    }
                    "quit" => return Ok(()),
                    other => Err((-32602, format!("没有 {other} 这个工具"))),
                }
            }
            other => Err((-32601, format!("不会 {other}"))),
        };
        let reply = match outcome {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
            }
        };
        let mut out = serde_json::to_vec(&reply).expect("JSON 值总能序列化");
        out.push(b'\n');
        writer.write_all(&out).await?;
        writer.flush().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Fence;
    use std::sync::atomic::AtomicUsize;

    /// 每接一次线，就在进程内起一个假服务。返回服务和接过几次线。
    fn demo(auto_approve: &[&str]) -> (Arc<McpServer>, Arc<AtomicUsize>) {
        let dials = Arc::new(AtomicUsize::new(0));
        let count = dials.clone();
        let dialer: Dialer = Box::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                let (ours, theirs) = tokio::io::duplex(64 * 1024);
                let (server_in, server_out) = tokio::io::split(theirs);
                tokio::spawn(run_demo_server(server_in, server_out));
                let (reader, writer) = tokio::io::split(ours);
                Ok(Pipe::new(reader, writer))
            })
        });
        let auto = auto_approve.iter().map(|name| name.to_string()).collect();
        let server = McpServer::with_dialer("demo", auto, dialer);
        (Arc::new(server), dials)
    }

    fn ctx() -> ToolCtx {
        let dir = tempfile::tempdir().unwrap().keep();
        ToolCtx::new(Fence::new(&dir, &[]).unwrap())
    }

    #[tokio::test]
    async fn tools_are_listed_across_pages_and_called() {
        let (server, _) = demo(&["echo"]);
        let infos = server.connect().await.unwrap();
        let tools = server.tools(&infos);
        let names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
        assert_eq!(
            names,
            [
                "mcp__demo__echo",
                "mcp__demo__add",
                "mcp__demo__fail",
                "mcp__demo__wait",
                "mcp__demo__quit"
            ]
        );
        let ctx = ctx();
        let echo = &tools[0];
        assert!(echo
            .description()
            .starts_with("（外部 MCP 服务「demo」提供的工具）把 text"));
        assert_eq!(echo.input_schema()["properties"]["text"]["type"], "string");
        assert_eq!(echo.title(&json!({})), "调用 demo 的 echo");
        assert_eq!(
            echo.call(json!({ "text": "你好" }), &ctx).await.unwrap(),
            "你好"
        );
        assert_eq!(
            tools[1]
                .call(json!({ "a": 2, "b": 3 }), &ctx)
                .await
                .unwrap(),
            "5"
        );
        // 工具自己说失败了：算失败，原因带给模型。
        let failed = tools[2].call(json!({}), &ctx).await.unwrap_err();
        assert_eq!(failed.0, "这个工具总是失败");
        let wrong = echo.call(json!("不是对象"), &ctx).await.unwrap_err();
        assert!(wrong.0.contains("对象"), "{wrong}");

        // 配置里写了不用问的直接执行，别的每次都问。
        assert_eq!(
            echo.check_permission(&json!({}), &ctx).await,
            Permission::Allow
        );
        // 审批卡上看得到交给它的参数；太长的截短并说明。
        let ask = tools[1]
            .check_permission(&json!({ "a": 2, "b": 3 }), &ctx)
            .await;
        let Permission::Ask(reason) = ask else {
            panic!("应该先问人");
        };
        assert!(
            reason.contains("demo") && reason.contains(r#"{"a":2,"b":3}"#),
            "{reason}"
        );
        let long = json!({ "text": "字".repeat(5_000) });
        let Permission::Ask(reason) = tools[1].check_permission(&long, &ctx).await else {
            panic!("应该先问人");
        };
        assert!(
            reason.chars().count() < 800 && reason.contains("没有显示"),
            "{reason}"
        );
    }

    #[tokio::test]
    async fn a_call_can_be_stopped_and_does_not_wait_forever() {
        let (server, _) = demo(&[]);
        server.connect().await.unwrap();
        let cancel = CancellationToken::new();
        let stopper = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            stopper.cancel();
        });
        let started = std::time::Instant::now();
        let stopped = server.call("wait", json!({ "ms": 60_000 }), &cancel).await;
        assert_eq!(stopped.unwrap_err().0, "已停止。");
        assert!(started.elapsed() < Duration::from_secs(5));

        let (server, _) = demo(&[]);
        let quick = McpTimeouts {
            start: Duration::from_secs(5),
            call: Duration::from_millis(100),
        };
        let server = Arc::try_unwrap(server).ok().unwrap().timeouts(quick);
        server.connect().await.unwrap();
        let slow = server
            .call("wait", json!({ "ms": 60_000 }), &CancellationToken::new())
            .await;
        assert!(slow.unwrap_err().0.contains("没有回应"));
    }

    #[tokio::test]
    async fn a_server_that_went_away_is_dialled_again_on_the_next_call() {
        let (server, dials) = demo(&[]);
        server.connect().await.unwrap();
        let never = CancellationToken::new();
        let gone = server.call("quit", json!({}), &never).await.unwrap_err();
        assert!(gone.0.contains("退出了"), "{gone}");
        assert_eq!(dials.load(Ordering::SeqCst), 1);
        let back = server
            .call("echo", json!({ "text": "还在吗" }), &never)
            .await;
        assert_eq!(back.unwrap(), "还在吗");
        assert_eq!(dials.load(Ordering::SeqCst), 2);
        // 没出事的时候不重连。
        server
            .call("echo", json!({ "text": "在" }), &never)
            .await
            .unwrap();
        assert_eq!(dials.load(Ordering::SeqCst), 2);
    }

    /// 一个不太守规矩的服务：往输出里打日志、反过来问客户端、没有工具能力。
    #[tokio::test]
    async fn noise_and_questions_from_the_server_do_not_break_the_line() {
        let asked = Arc::new(Mutex::new(vec![]));
        let seen = asked.clone();
        let dialer: Dialer = Box::new(move || {
            let seen = seen.clone();
            Box::pin(async move {
                let (ours, theirs) = tokio::io::duplex(64 * 1024);
                tokio::spawn(async move {
                    let (reader, mut writer) = tokio::io::split(theirs);
                    let mut reader = BufReader::new(reader);
                    let mut line = vec![];
                    writer.write_all(b"starting up...\n").await.unwrap();
                    while let Ok(true) = read_line(&mut reader, &mut line).await {
                        let message: Value = serde_json::from_slice(&line).unwrap();
                        if message["method"] == "initialize" {
                            // 先反过来问两句，再回答。
                            let ping = json!({ "jsonrpc": "2.0", "id": "p1", "method": "ping" });
                            let roots =
                                json!({ "jsonrpc": "2.0", "id": "r1", "method": "roots/list" });
                            let note = json!({ "jsonrpc": "2.0", "method": "notifications/message", "params": {} });
                            let reply = json!({
                                "jsonrpc": "2.0", "id": message["id"],
                                "result": { "protocolVersion": "2024-11-05", "capabilities": {} },
                            });
                            for out in [ping, roots, note, reply] {
                                writer
                                    .write_all(format!("{out}\n").as_bytes())
                                    .await
                                    .unwrap();
                            }
                        } else if message.get("method").is_none() {
                            seen.lock().unwrap().push(message);
                        }
                    }
                });
                let (reader, writer) = tokio::io::split(ours);
                Ok(Pipe::new(reader, writer))
            })
        });
        let server = McpServer::with_dialer("odd", vec![], dialer);
        // 没说自己有工具：不去问清单，当作没有工具。
        assert_eq!(server.connect().await.unwrap(), vec![]);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let answers = asked.lock().unwrap().clone();
        assert_eq!(answers.len(), 2, "{answers:?}");
        assert_eq!(answers[0]["id"], "p1");
        assert_eq!(answers[0]["result"], json!({}));
        assert_eq!(answers[1]["error"]["code"], -32601);
    }

    /// 握手照常、收到工具调用不回答的服务。收到的每条消息都记下来。
    fn silent(seen: Arc<Mutex<Vec<Value>>>) -> Dialer {
        Box::new(move || {
            let seen = seen.clone();
            Box::pin(async move {
                let (ours, theirs) = tokio::io::duplex(64 * 1024);
                tokio::spawn(async move {
                    let (reader, mut writer) = tokio::io::split(theirs);
                    let mut reader = BufReader::new(reader);
                    let mut line = vec![];
                    while let Ok(true) = read_line(&mut reader, &mut line).await {
                        let message: Value = serde_json::from_slice(&line).unwrap();
                        let result = match message["method"].as_str() {
                            Some("initialize") => json!({ "capabilities": { "tools": {} } }),
                            Some("tools/list") => json!({ "tools": [] }),
                            _ => {
                                seen.lock().unwrap().push(message);
                                continue;
                            }
                        };
                        let reply =
                            json!({ "jsonrpc": "2.0", "id": message["id"], "result": result });
                        writer
                            .write_all(format!("{reply}\n").as_bytes())
                            .await
                            .unwrap();
                    }
                });
                let (reader, writer) = tokio::io::split(ours);
                Ok(Pipe::new(reader, writer))
            })
        })
    }

    #[tokio::test]
    async fn a_call_dropped_by_its_caller_still_tells_the_server_to_stop() {
        let seen = Arc::new(Mutex::new(vec![]));
        let server = McpServer::with_dialer("quiet", vec![], silent(seen.clone()));
        server.connect().await.unwrap();
        // 人点了停止：上层不再等这个工具，整个调用被丢弃，轮不到它自己去发取消。
        let never = CancellationToken::new();
        let call = server.call("slow", json!({}), &never);
        assert!(tokio::time::timeout(Duration::from_millis(50), call)
            .await
            .is_err());
        tokio::time::sleep(Duration::from_millis(100)).await;
        let seen = seen.lock().unwrap().clone();
        let methods: Vec<&str> = seen.iter().filter_map(|m| m["method"].as_str()).collect();
        assert_eq!(
            methods,
            [
                "notifications/initialized",
                "tools/call",
                "notifications/cancelled"
            ]
        );
        assert_eq!(seen[2]["params"]["requestId"], seen[1]["id"]);
    }

    #[tokio::test]
    async fn a_server_that_stops_reading_cannot_hold_a_call_past_its_limit() {
        // 管道只有几个字节，那一头从来不读：写会卡住。
        let dialer: Dialer = Box::new(|| {
            Box::pin(async {
                let (ours, theirs) = tokio::io::duplex(8);
                std::mem::forget(theirs);
                let (reader, writer) = tokio::io::split(ours);
                Ok(Pipe::new(reader, writer))
            })
        });
        let quick = McpTimeouts {
            start: Duration::from_millis(100),
            call: Duration::from_millis(100),
        };
        let server = McpServer::with_dialer("deaf", vec![], dialer).timeouts(quick);
        let started = std::time::Instant::now();
        let error = server.connect().await.unwrap_err();
        assert!(error.0.contains("没有回应"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn a_command_that_does_not_exist_says_so() {
        let spec = McpSpec {
            name: "nope".into(),
            command: "sa-no-such-mcp-server".into(),
            ..McpSpec::default()
        };
        let error = McpServer::new(spec).connect().await.unwrap_err();
        assert!(
            error.0.contains("找不到命令「sa-no-such-mcp-server」"),
            "{error}"
        );
    }

    #[test]
    fn tool_names_fit_what_model_services_accept() {
        assert_eq!(tool_name("fs", "read_file"), "mcp__fs__read_file");
        assert_eq!(tool_name("my.fs", "读 file/1"), "mcp__my_fs____file_1");
        let long = tool_name("server", &"x".repeat(100));
        assert_eq!(long.len(), 64);

        let (server, _) = demo(&[]);
        let info = |name: &str, schema: Value| McpToolInfo {
            name: name.into(),
            description: "说明".repeat(1000),
            input_schema: schema,
        };
        let tools = server.tools(&[
            info("a b", json!({ "type": "object", "properties": {} })),
            // 整理之后重名的只留第一个。
            info("a/b", Value::Null),
            info("c", json!("不是对象")),
        ]);
        let names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
        assert_eq!(names, ["mcp__demo__a_b", "mcp__demo__c"]);
        assert_eq!(tools[0].remote_name(), "a b");
        assert_eq!(tools[1].input_schema(), json!({ "type": "object" }));
        assert!(tools[0].description().chars().count() < 1100);
    }

    #[test]
    fn results_are_turned_into_text() {
        let ok = json!({ "content": [
            { "type": "text", "text": "第一段" },
            { "type": "image", "data": "…", "mimeType": "image/png" },
            { "type": "resource", "resource": { "uri": "file:///a", "text": "文件内容" } },
            { "type": "resource_link", "uri": "file:///b" },
        ]});
        assert_eq!(
            render(&ok).unwrap(),
            "第一段\n[图片]\n文件内容\n[资源 file:///b]"
        );
        let structured = json!({ "content": [], "structuredContent": { "n": 1 } });
        assert_eq!(render(&structured).unwrap(), r#"{"n":1}"#);
        assert_eq!(render(&json!({})).unwrap(), "（工具没有返回内容）");
        let failed = json!({ "content": [], "isError": true });
        assert!(render(&failed).unwrap_err().0.contains("没有说原因"));
    }
}
