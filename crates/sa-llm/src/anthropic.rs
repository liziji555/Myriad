//! Anthropic Messages（`/v1/messages`）流式适配器：Claude 官方接口，以及兼容这个格式的中转服务。

use crate::openai::{status_error, transport_error};
use crate::{
    ChatMessage, ChatRequest, EventStream, LlmError, ModelCaps, Provider, Role, StopReason,
    StreamEvent, Usage,
};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures_util::{stream, Stream, StreamExt};
use reqwest::header::HeaderName;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const VERSION: &str = "2023-06-01";
/// 这个接口必须写输出上限；请求里没给时用它。
const DEFAULT_MAX_TOKENS: u32 = 16_000;

pub struct Anthropic {
    http: reqwest::Client,
    url: String,
    api_key: String,
    idle_timeout: Duration,
    session_header: Option<HeaderName>,
}

impl Anthropic {
    /// `base_url` 形如 `https://api.anthropic.com`（或者带 `/v1`），请求发往 `…/v1/messages`。
    pub fn new(base_url: &str, api_key: impl Into<String>) -> Result<Self, LlmError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .user_agent(concat!("superagent/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| LlmError::Network(e.to_string()))?;
        let base = base_url.trim_end_matches('/');
        let url = if base.ends_with("/v1") {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
        };
        Ok(Self {
            http,
            url,
            api_key: api_key.into(),
            idle_timeout: IDLE_TIMEOUT,
            session_header: None,
        })
    }

    /// 把会话 ID 放在这个请求头里发给服务端。
    pub fn with_session_header(mut self, name: &str) -> Result<Self, LlmError> {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| LlmError::BadRequest(format!("「{name}」不是合法的请求头名字")))?;
        self.session_header = Some(name);
        Ok(self)
    }

    pub fn with_idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = timeout;
        self
    }
}

#[async_trait]
impl Provider for Anthropic {
    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, LlmError> {
        let mut call = self
            .http
            .post(&self.url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", VERSION)
            .json(&request_body(&request));
        if let (Some(header), Some(session)) = (&self.session_header, &request.session_id) {
            call = call.header(header, session);
        }
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(LlmError::Cancelled),
            result = tokio::time::timeout(self.idle_timeout, call.send()) => match result {
                Err(_) => return Err(LlmError::Timeout),
                Ok(Err(e)) => return Err(transport_error(e)),
                Ok(Ok(response)) => response,
            },
        };
        let status = response.status();
        if !status.is_success() {
            let body = tokio::select! {
                _ = cancel.cancelled() => return Err(LlmError::Cancelled),
                body = tokio::time::timeout(self.idle_timeout, response.text()) => {
                    body.ok().and_then(Result::ok).unwrap_or_default()
                }
            };
            return Err(status_error(status.as_u16(), &body));
        }
        let chunks = response
            .bytes_stream()
            .eventsource()
            .map(|item| match item {
                Ok(event) => Ok(event.data),
                Err(e) => Err(LlmError::Network(e.to_string())),
            })
            .boxed();
        let state = Decoder {
            chunks,
            cancel,
            idle_timeout: self.idle_timeout,
            out: VecDeque::new(),
            calls: HashMap::new(),
            last_call: None,
            usage: Seen::default(),
            stop: None,
            finished: false,
        };
        Ok(stream::unfold(state, |mut state| async move {
            let item = state.next().await?;
            Some((item, state))
        })
        .boxed())
    }

    fn capabilities(&self, _model: &str) -> ModelCaps {
        // Claude 现在的模型都是 200K 上下文。别的模型在档位里用 context_window 改。
        ModelCaps {
            context_window: 200_000,
            supports_tools: true,
        }
    }
}

/// 请求体。思考模式不开：带工具接着聊时要原样交回思考内容的签名，这里没有记。
fn request_body(request: &ChatRequest) -> Value {
    let system: Vec<&str> = request
        .messages
        .iter()
        .filter(|m| m.role == Role::System && !m.content.is_empty())
        .map(|m| m.content.as_str())
        .collect();
    let mut body = json!({
        "model": request.model,
        "max_tokens": request.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "messages": messages_json(&request.messages),
        "stream": true,
    });
    if !system.is_empty() {
        body["system"] = json!(system.join("\n\n"));
    }
    if !request.tools.is_empty() {
        body["tools"] = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.parameters,
                })
            })
            .collect();
    }
    body
}

/// 对话记录换成这个接口的格式：工具结果算人说的话，同一方连着说的合成一条（工具结果必须紧跟在调用后面那一条里）。
fn messages_json(messages: &[ChatMessage]) -> Vec<Value> {
    let mut out: Vec<(&str, Vec<Value>)> = vec![];
    for message in messages {
        let (role, blocks) = match message.role {
            Role::System => continue,
            Role::User => ("user", text_block(&message.content)),
            Role::Tool => (
                "user",
                vec![json!({
                    "type": "tool_result",
                    "tool_use_id": message.tool_call_id.as_deref().unwrap_or_default(),
                    "content": message.content,
                })],
            ),
            Role::Assistant => {
                let mut blocks = text_block(&message.content);
                for call in &message.tool_calls {
                    // 模型给的参数可能不是合法 JSON：这个接口要对象，给个空的。
                    let input = serde_json::from_str::<Value>(&call.arguments)
                        .ok()
                        .filter(Value::is_object)
                        .unwrap_or_else(|| json!({}));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": input,
                    }));
                }
                ("assistant", blocks)
            }
        };
        if blocks.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some((last, merged)) if *last == role => merged.extend(blocks),
            _ => out.push((role, blocks)),
        }
    }
    out.into_iter()
        .map(|(role, content)| json!({ "role": role, "content": content }))
        .collect()
}

/// 空的文字块这个接口不收。
fn text_block(text: &str) -> Vec<Value> {
    if text.is_empty() {
        vec![]
    } else {
        vec![json!({ "type": "text", "text": text })]
    }
}

#[derive(Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    index: usize,
    message: Option<StartMessage>,
    content_block: Option<Block>,
    delta: Option<Delta>,
    usage: Option<WireUsage>,
    error: Option<WireError>,
}

#[derive(Deserialize)]
struct StartMessage {
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: String,
    id: Option<String>,
    name: Option<String>,
    text: Option<String>,
    thinking: Option<String>,
}

#[derive(Deserialize)]
struct Delta {
    #[serde(rename = "type")]
    kind: Option<String>,
    text: Option<String>,
    partial_json: Option<String>,
    thinking: Option<String>,
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
struct WireUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct WireError {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    message: String,
}

struct Decoder {
    chunks: Pin<Box<dyn Stream<Item = Result<String, LlmError>> + Send>>,
    cancel: CancellationToken,
    idle_timeout: Duration,
    out: VecDeque<Result<StreamEvent, LlmError>>,
    /// 第几块是哪次工具调用。
    calls: HashMap<usize, String>,
    /// 最近开始的那次工具调用。有的中转把参数的最后一段标错了块号，就算给它。
    last_call: Option<String>,
    usage: Seen,
    stop: Option<StopReason>,
    finished: bool,
}

/// 流里报过的用量。一项都没报过时不交用量，让上层按估算记账。
#[derive(Default)]
struct Seen {
    input: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    output: Option<u64>,
}

impl Seen {
    fn usage(&self) -> Option<Usage> {
        if self.input.is_none() && self.output.is_none() {
            return None;
        }
        let cached = self.cache_read.unwrap_or(0);
        Some(Usage {
            input_tokens: self.input.unwrap_or(0) + cached + self.cache_write.unwrap_or(0),
            output_tokens: self.output.unwrap_or(0),
            cached_tokens: cached,
        })
    }
}

impl Decoder {
    async fn next(&mut self) -> Option<Result<StreamEvent, LlmError>> {
        loop {
            if let Some(item) = self.out.pop_front() {
                return Some(item);
            }
            if self.finished {
                return None;
            }
            let chunk = tokio::select! {
                _ = self.cancel.cancelled() => Some(Err(LlmError::Cancelled)),
                result = tokio::time::timeout(self.idle_timeout, self.chunks.next()) => {
                    result.unwrap_or(Some(Err(LlmError::Timeout)))
                }
            };
            match chunk {
                Some(Ok(data)) => self.decode(&data),
                Some(Err(error)) => self.fail(error),
                None => self.fail(LlmError::Network("连接在回复结束之前断开了".into())),
            }
        }
    }

    fn fail(&mut self, error: LlmError) {
        self.finished = true;
        self.out.push_back(Err(error));
    }

    /// 用量是累计的：后来报的数字盖掉前面的，没报的那项留着前面的。
    fn count(&mut self, usage: WireUsage) {
        let seen = &mut self.usage;
        for (value, slot) in [
            (usage.input_tokens, &mut seen.input),
            (usage.cache_read_input_tokens, &mut seen.cache_read),
            (usage.cache_creation_input_tokens, &mut seen.cache_write),
            (usage.output_tokens, &mut seen.output),
        ] {
            if let Some(value) = value {
                *slot = Some(value);
            }
        }
    }

    fn decode(&mut self, data: &str) {
        let event: Event = match serde_json::from_str(data) {
            Ok(event) => event,
            Err(e) => {
                let seen: String = data.chars().take(120).collect();
                return self.fail(LlmError::Protocol(format!("{e}（收到：{seen}）")));
            }
        };
        match event.kind.as_str() {
            "message_start" => {
                if let Some(usage) = event.message.and_then(|m| m.usage) {
                    self.count(usage);
                }
            }
            "content_block_start" => {
                let Some(block) = event.content_block else {
                    return;
                };
                match block.kind.as_str() {
                    "tool_use" => {
                        let (Some(id), Some(name)) =
                            (block.id.filter(|id| !id.is_empty()), block.name)
                        else {
                            return self.fail(LlmError::Protocol("工具调用缺少 id 或名字".into()));
                        };
                        self.calls.insert(event.index, id.clone());
                        self.last_call = Some(id.clone());
                        self.out
                            .push_back(Ok(StreamEvent::ToolCallStart { id, name }));
                    }
                    "text" => self.text(block.text),
                    "thinking" => self.thinking(block.thinking),
                    _ => {}
                }
            }
            "content_block_delta" => {
                let Some(delta) = event.delta else { return };
                match delta.kind.as_deref() {
                    Some("text_delta") => self.text(delta.text),
                    Some("thinking_delta") => self.thinking(delta.thinking),
                    Some("input_json_delta") => {
                        let fragment = delta.partial_json.filter(|f| !f.is_empty());
                        let id = self.calls.get(&event.index).or(self.last_call.as_ref());
                        if let (Some(id), Some(fragment)) = (id, fragment) {
                            self.out.push_back(Ok(StreamEvent::ToolCallArgsDelta {
                                id: id.clone(),
                                json_fragment: fragment,
                            }));
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(reason) = event.delta.and_then(|d| d.stop_reason) {
                    self.stop = Some(match reason.as_str() {
                        "end_turn" | "stop_sequence" => StopReason::EndTurn,
                        "tool_use" => StopReason::ToolUse,
                        // 输出到了上限，或者上下文窗口放不下了：都是没说完就被截断。
                        "max_tokens" | "model_context_window_exceeded" => StopReason::MaxTokens,
                        _ => StopReason::Other(reason),
                    });
                }
                if let Some(usage) = event.usage {
                    self.count(usage);
                }
            }
            "message_stop" => {
                self.finished = true;
                if let Some(usage) = self.usage.usage() {
                    self.out.push_back(Ok(StreamEvent::Usage(usage)));
                }
                match self.stop.take() {
                    Some(stop) => self.out.push_back(Ok(StreamEvent::Done { stop })),
                    None => self.out.push_back(Err(LlmError::Protocol(
                        "流结束了，但没有说明结束原因".into(),
                    ))),
                }
            }
            "error" => {
                let error = event.error.unwrap_or(WireError {
                    kind: String::new(),
                    message: String::new(),
                });
                let message = format!("{}：{}", error.kind, error.message);
                self.fail(match error.kind.as_str() {
                    "overloaded_error" | "api_error" => LlmError::Server(message),
                    "rate_limit_error" => LlmError::RateLimited(message),
                    "authentication_error" | "permission_error" => LlmError::Auth(message),
                    _ => LlmError::BadRequest(message),
                });
            }
            _ => {}
        }
    }

    fn text(&mut self, text: Option<String>) {
        if let Some(text) = text.filter(|t| !t.is_empty()) {
            self.out.push_back(Ok(StreamEvent::TextDelta(text)));
        }
    }

    fn thinking(&mut self, text: Option<String>) {
        if let Some(text) = text.filter(|t| !t.is_empty()) {
            self.out.push_back(Ok(StreamEvent::ReasoningDelta(text)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Accumulator, ToolCall, ToolSpec};
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::post;
    use axum::{Json, Router};
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    enum Canned {
        Sse(Vec<&'static str>),
        Status(u16, &'static str),
    }

    #[derive(Clone, Default)]
    struct Server {
        replies: Arc<Mutex<VecDeque<Canned>>>,
        seen: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
    }

    async fn handle(
        State(server): State<Server>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> axum::response::Response {
        server.seen.lock().unwrap().push((headers, body));
        match server
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("没准备这么多应答")
        {
            Canned::Sse(lines) => {
                let body: String = lines
                    .iter()
                    .map(|l| format!("event: x\ndata: {l}\n\n"))
                    .collect();
                ([("content-type", "text/event-stream")], body).into_response()
            }
            Canned::Status(code, body) => {
                (StatusCode::from_u16(code).unwrap(), body).into_response()
            }
        }
    }

    async fn start(base: &str, replies: Vec<Canned>) -> (Anthropic, Server) {
        let server = Server::default();
        server.replies.lock().unwrap().extend(replies);
        let app = Router::new()
            .route("/v1/messages", post(handle))
            .with_state(server.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}{base}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (Anthropic::new(&url, "sk-ant-test").unwrap(), server)
    }

    fn request() -> ChatRequest {
        ChatRequest {
            model: "claude-sonnet".into(),
            messages: vec![ChatMessage::system("你是助手"), ChatMessage::user("你好")],
            tools: vec![],
            max_tokens: None,
            thinking: Some(true),
            session_id: None,
        }
    }

    async fn collect(provider: &Anthropic, request: ChatRequest) -> Result<Accumulator, LlmError> {
        let mut stream = provider.stream(request, CancellationToken::new()).await?;
        let mut acc = Accumulator::default();
        while let Some(event) = stream.next().await {
            acc.push(&event?);
        }
        Ok(acc)
    }

    #[tokio::test]
    async fn streams_text_and_usage() {
        let (provider, server) = start("/", vec![Canned::Sse(vec![
            r#"{"type":"message_start","message":{"usage":{"input_tokens":4,"output_tokens":1,"cache_read_input_tokens":8,"cache_creation_input_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"ping"}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"你"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"好"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
            r#"{"type":"message_stop"}"#,
        ])])
        .await;
        let acc = collect(&provider, request()).await.unwrap();
        assert_eq!(acc.text, "你好");
        assert_eq!(acc.stop, Some(StopReason::EndTurn));
        assert_eq!(
            acc.usage,
            Some(Usage {
                input_tokens: 12,
                output_tokens: 3,
                cached_tokens: 8
            })
        );
        let (headers, body) = server.seen.lock().unwrap()[0].clone();
        assert_eq!(headers["x-api-key"], "sk-ant-test");
        assert_eq!(headers["anthropic-version"], VERSION);
        assert_eq!(body["system"], "你是助手");
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(body["stream"], true);
        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": [{"type": "text", "text": "你好"}]}])
        );
        assert!(body.get("thinking").is_none() && body.get("tools").is_none());
    }

    #[tokio::test]
    async fn assembles_tool_calls_and_sends_history_back() {
        let (provider, server) = start("/v1", vec![Canned::Sse(vec![
            r#"{"type":"message_start","message":{"usage":{"input_tokens":5,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"想"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"x"}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_a","name":"read","input":{}}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"a.md\""}}"#,
            // 有的中转把最后一段标成了不存在的块号。
            r#"{"type":"content_block_delta","index":5,"delta":{"type":"input_json_delta","partial_json":"}"}}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_b","name":"list_dir","input":{}}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}"#,
            r#"{"type":"message_stop"}"#,
        ])])
        .await;
        let mut req = request();
        req.max_tokens = Some(4096);
        req.tools = vec![ToolSpec {
            name: "read".into(),
            description: "读文件".into(),
            parameters: json!({"type": "object"}),
        }];
        let calls = vec![
            ToolCall {
                id: "toolu_0".into(),
                name: "read".into(),
                arguments: r#"{"path":"x"}"#.into(),
            },
            ToolCall {
                id: "toolu_1".into(),
                name: "grep".into(),
                arguments: "不是 JSON".into(),
            },
        ];
        req.messages.push(ChatMessage {
            reasoning: Some("先读".into()),
            tool_calls: calls,
            ..ChatMessage::assistant("我看看")
        });
        req.messages
            .push(ChatMessage::tool_result("toolu_0", "内容"));
        req.messages
            .push(ChatMessage::tool_result("toolu_1", "没找到"));

        let acc = collect(&provider, req).await.unwrap();
        assert_eq!(acc.reasoning, "想");
        assert_eq!(
            acc.tool_calls,
            vec![
                ToolCall {
                    id: "toolu_a".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"a.md"}"#.into()
                },
                ToolCall {
                    id: "toolu_b".into(),
                    name: "list_dir".into(),
                    arguments: String::new()
                },
            ]
        );
        assert_eq!(acc.stop, Some(StopReason::ToolUse));

        let body = server.seen.lock().unwrap()[0].1.clone();
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["tools"][0]["input_schema"], json!({"type": "object"}));
        assert_eq!(
            body["messages"][1],
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "我看看"},
                {"type": "tool_use", "id": "toolu_0", "name": "read", "input": {"path": "x"}},
                {"type": "tool_use", "id": "toolu_1", "name": "grep", "input": {}},
            ]})
        );
        assert_eq!(
            body["messages"][2],
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_0", "content": "内容"},
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "没找到"},
            ]})
        );
    }

    #[tokio::test]
    async fn usage_keeps_what_was_reported_and_is_left_out_when_never_reported() {
        let (provider, _) = start("/", vec![
            Canned::Sse(vec![
                r#"{"type":"message_start","message":{"usage":{"input_tokens":4,"cache_creation_input_tokens":6,"output_tokens":1}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"好"}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"model_context_window_exceeded"},"usage":{"input_tokens":4,"output_tokens":9}}"#,
                r#"{"type":"message_stop"}"#,
            ]),
            Canned::Sse(vec![
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"好"}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
                r#"{"type":"message_stop"}"#,
            ]),
        ])
        .await;
        let acc = collect(&provider, request()).await.unwrap();
        assert_eq!(acc.stop, Some(StopReason::MaxTokens));
        assert_eq!(
            acc.usage,
            Some(Usage {
                input_tokens: 10,
                output_tokens: 9,
                cached_tokens: 0
            })
        );
        let acc = collect(&provider, request()).await.unwrap();
        assert_eq!(acc.usage, None);
    }

    #[tokio::test]
    async fn maps_errors_and_incomplete_streams() {
        let (provider, _) = start("", vec![
            Canned::Status(401, r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#),
            Canned::Status(529, r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#),
            Canned::Sse(vec![
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"半"}}"#,
                r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            ]),
            Canned::Sse(vec![r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"半"}}"#]),
        ])
        .await;
        assert_eq!(
            collect(&provider, request()).await.unwrap_err(),
            LlmError::Auth("HTTP 401：invalid x-api-key".into())
        );
        let overloaded = collect(&provider, request()).await.unwrap_err();
        assert!(overloaded.is_retryable(), "{overloaded:?}");
        assert!(matches!(
            collect(&provider, request()).await,
            Err(LlmError::Server(m)) if m.contains("Overloaded")
        ));
        assert!(matches!(
            collect(&provider, request()).await,
            Err(LlmError::Network(_))
        ));
    }
}
