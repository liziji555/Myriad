//! OpenAI 兼容的 `/chat/completions` 流式适配器。DeepSeek 用的就是这个协议。

use crate::{
    ChatMessage, ChatRequest, EventStream, LlmError, ModelCaps, Provider, Role, StopReason,
    StreamEvent, Usage,
};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures_util::{stream, Stream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 等响应头，以及流里相邻两块数据之间，最多等多久。
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

pub struct OpenAiCompat {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    idle_timeout: Duration,
}

impl OpenAiCompat {
    /// `base_url` 形如 `https://api.deepseek.com`，请求发往 `{base_url}/chat/completions`。
    pub fn new(base_url: &str, api_key: impl Into<String>) -> Result<Self, LlmError> {
        // 没有内置加密后端的 rustls 需要先选一个；已经选过时返回 Err，可以忽略。
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| LlmError::Network(e.to_string()))?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            idle_timeout: IDLE_TIMEOUT,
        })
    }

    pub fn with_idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = timeout;
        self
    }
}

#[async_trait]
impl Provider for OpenAiCompat {
    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, LlmError> {
        let send = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&request_body(&request))
            .send();
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(LlmError::Cancelled),
            result = tokio::time::timeout(self.idle_timeout, send) => match result {
                Err(_) => return Err(LlmError::Timeout),
                Ok(Err(e)) => return Err(transport_error(e)),
                Ok(Ok(response)) => response,
            },
        };
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
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
        // DeepSeek 当前两个模型都是 1M 上下文。其他网关的模型可能更小，M2 做压缩时改成可配置。
        ModelCaps {
            context_window: 1_000_000,
            supports_tools: true,
        }
    }
}

fn request_body(request: &ChatRequest) -> Value {
    let mut body = json!({
        "model": request.model,
        "messages": request.messages.iter().map(message_json).collect::<Vec<_>>(),
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if !request.tools.is_empty() {
        body["tools"] = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    },
                })
            })
            .collect();
    }
    if let Some(max_tokens) = request.max_tokens {
        body["max_tokens"] = json!(max_tokens);
    }
    if let Some(thinking) = request.thinking {
        body["thinking"] = json!({ "type": if thinking { "enabled" } else { "disabled" } });
    }
    body
}

fn message_json(message: &ChatMessage) -> Value {
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    let mut value = json!({ "role": role, "content": message.content });
    if let Some(reasoning) = &message.reasoning {
        value["reasoning_content"] = json!(reasoning);
    }
    if !message.tool_calls.is_empty() {
        value["tool_calls"] = message
            .tool_calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.id,
                    "type": "function",
                    "function": { "name": call.name, "arguments": call.arguments },
                })
            })
            .collect();
    }
    if let Some(id) = &message.tool_call_id {
        value["tool_call_id"] = json!(id);
    }
    value
}

fn transport_error(error: reqwest::Error) -> LlmError {
    if error.is_timeout() {
        LlmError::Timeout
    } else {
        LlmError::Network(error.to_string())
    }
}

fn status_error(status: u16, body: &str) -> LlmError {
    #[derive(Deserialize)]
    struct Envelope {
        error: Detail,
    }
    #[derive(Deserialize)]
    struct Detail {
        message: String,
    }
    let detail = serde_json::from_str::<Envelope>(body)
        .map(|e| e.error.message)
        .unwrap_or_else(|_| body.chars().take(300).collect());
    let message = format!("HTTP {status}：{detail}");
    match status {
        401 | 403 => LlmError::Auth(message),
        429 => LlmError::RateLimited(message),
        // 408 是网关等上游超时。
        408 => LlmError::Timeout,
        500..=599 => LlmError::Server(message),
        // 其余 4xx（参数错误、余额不足等）重试也不会好。
        _ => LlmError::BadRequest(message),
    }
}

#[derive(Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    content: Option<String>,
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireToolCall>,
}

#[derive(Deserialize)]
struct WireToolCall {
    #[serde(default)]
    index: usize,
    id: Option<String>,
    function: Option<WireFunction>,
}

#[derive(Deserialize)]
struct WireFunction {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    /// DeepSeek 的写法。
    prompt_cache_hit_tokens: Option<u64>,
    /// OpenAI 的写法。
    prompt_tokens_details: Option<PromptDetails>,
}

#[derive(Deserialize)]
struct PromptDetails {
    cached_tokens: Option<u64>,
}

struct Decoder {
    chunks: Pin<Box<dyn Stream<Item = Result<String, LlmError>> + Send>>,
    cancel: CancellationToken,
    idle_timeout: Duration,
    out: VecDeque<Result<StreamEvent, LlmError>>,
    /// 流里的工具调用用下标区分，只有第一块带 id。
    calls: HashMap<usize, String>,
    stop: Option<StopReason>,
    finished: bool,
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
                // 连接断了却没有收到结束标记：回复不完整。
                None => self.fail(LlmError::Network("连接在回复结束之前断开了".into())),
            }
        }
    }

    fn fail(&mut self, error: LlmError) {
        self.finished = true;
        self.out.push_back(Err(error));
    }

    fn decode(&mut self, data: &str) {
        if data.trim() == "[DONE]" {
            self.finished = true;
            match self.stop.take() {
                Some(stop) => self.out.push_back(Ok(StreamEvent::Done { stop })),
                None => self.out.push_back(Err(LlmError::Protocol(
                    "流结束了，但没有说明结束原因".into(),
                ))),
            }
            return;
        }
        let chunk: Chunk = match serde_json::from_str(data) {
            Ok(chunk) => chunk,
            Err(e) => {
                return self.fail(LlmError::Protocol(format!(
                    "{e}（收到：{}）",
                    preview(data)
                )))
            }
        };
        for choice in chunk.choices {
            if let Some(text) = choice.delta.reasoning_content.filter(|t| !t.is_empty()) {
                self.out.push_back(Ok(StreamEvent::ReasoningDelta(text)));
            }
            if let Some(text) = choice.delta.content.filter(|t| !t.is_empty()) {
                self.out.push_back(Ok(StreamEvent::TextDelta(text)));
            }
            for call in choice.delta.tool_calls {
                let (name, arguments) = call
                    .function
                    .map_or((None, None), |f| (f.name, f.arguments));
                if !self.calls.contains_key(&call.index) {
                    let (Some(id), Some(name)) = (call.id.filter(|id| !id.is_empty()), name) else {
                        return self
                            .fail(LlmError::Protocol("工具调用的第一块缺少 id 或名字".into()));
                    };
                    self.calls.insert(call.index, id.clone());
                    self.out
                        .push_back(Ok(StreamEvent::ToolCallStart { id, name }));
                }
                if let Some(fragment) = arguments.filter(|a| !a.is_empty()) {
                    self.out.push_back(Ok(StreamEvent::ToolCallArgsDelta {
                        id: self.calls[&call.index].clone(),
                        json_fragment: fragment,
                    }));
                }
            }
            if let Some(reason) = choice.finish_reason {
                self.stop = Some(match reason.as_str() {
                    "stop" => StopReason::EndTurn,
                    "tool_calls" => StopReason::ToolUse,
                    "length" => StopReason::MaxTokens,
                    _ => StopReason::Other(reason),
                });
            }
        }
        if let Some(usage) = chunk.usage {
            let cached = usage
                .prompt_cache_hit_tokens
                .or(usage.prompt_tokens_details.and_then(|d| d.cached_tokens))
                .unwrap_or(0);
            self.out.push_back(Ok(StreamEvent::Usage(Usage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
                cached_tokens: cached,
            })));
        }
    }
}

fn preview(data: &str) -> String {
    data.chars().take(120).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{open_with_retry, Accumulator, RetryPolicy, ToolCall, ToolSpec};
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::post;
    use axum::{Json, Router};
    use std::sync::{Arc, Mutex};

    /// 假服务端的一次应答。
    #[derive(Clone)]
    enum Canned {
        Sse(Vec<&'static str>),
        /// 发完这些数据后连接保持打开、不再发任何东西。
        SseThenStall(Vec<&'static str>),
        Status(u16, &'static str),
    }

    type Seen = (Option<String>, Value);

    #[derive(Clone, Default)]
    struct Server {
        replies: Arc<Mutex<VecDeque<Canned>>>,
        /// 每个请求的 `Authorization` 头和请求体。
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    async fn handle(
        State(server): State<Server>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> axum::response::Response {
        let auth = headers
            .get("authorization")
            .map(|v| v.to_str().unwrap().to_string());
        server.seen.lock().unwrap().push((auth, body));
        let reply = server.replies.lock().unwrap().pop_front();
        let sse = |lines: Vec<&'static str>, stall: bool| {
            let body: String = lines.iter().map(|l| format!("data: {l}\n\n")).collect();
            let head = stream::once(async move { Ok::<_, std::io::Error>(body) });
            let tail = stream::pending().take(usize::from(stall));
            (
                [("content-type", "text/event-stream")],
                axum::body::Body::from_stream(head.chain(tail)),
            )
                .into_response()
        };
        match reply.expect("测试没有准备这么多应答") {
            Canned::Sse(lines) => sse(lines, false),
            Canned::SseThenStall(lines) => sse(lines, true),
            Canned::Status(code, body) => {
                (StatusCode::from_u16(code).unwrap(), body).into_response()
            }
        }
    }

    async fn start(replies: Vec<Canned>) -> (OpenAiCompat, Server) {
        let server = Server::default();
        server.replies.lock().unwrap().extend(replies);
        let app = Router::new()
            .route("/chat/completions", post(handle))
            .with_state(server.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (OpenAiCompat::new(&url, "sk-test").unwrap(), server)
    }

    fn request() -> ChatRequest {
        ChatRequest {
            model: "deepseek-flash".into(),
            messages: vec![ChatMessage::system("你是助手"), ChatMessage::user("你好")],
            tools: vec![],
            max_tokens: None,
            thinking: None,
        }
    }

    async fn collect(
        provider: &OpenAiCompat,
        request: ChatRequest,
    ) -> Result<Accumulator, LlmError> {
        let mut stream = provider.stream(request, CancellationToken::new()).await?;
        let mut acc = Accumulator::default();
        while let Some(event) = stream.next().await {
            acc.push(&event?);
        }
        Ok(acc)
    }

    #[tokio::test]
    async fn streams_text_reasoning_and_usage() {
        let (provider, server) = start(vec![Canned::Sse(vec![
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}],"usage":null}"#,
            r#"{"choices":[{"index":0,"delta":{"reasoning_content":"想一下"},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"你"},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"好"},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15,"prompt_cache_hit_tokens":8,"prompt_cache_miss_tokens":4}}"#,
            "[DONE]",
        ])])
        .await;
        let acc = collect(&provider, request()).await.unwrap();
        assert_eq!(acc.text, "你好");
        assert_eq!(acc.reasoning, "想一下");
        assert_eq!(acc.stop, Some(StopReason::EndTurn));
        assert_eq!(
            acc.usage,
            Some(Usage {
                input_tokens: 12,
                output_tokens: 3,
                cached_tokens: 8
            })
        );

        let (auth, body) = server.seen.lock().unwrap()[0].clone();
        assert_eq!(auth.as_deref(), Some("Bearer sk-test"));
        assert_eq!(body["model"], "deepseek-flash");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(
            body["messages"][1],
            json!({"role": "user", "content": "你好"})
        );
        assert!(body.get("tools").is_none());
        assert!(body.get("thinking").is_none());
    }

    #[tokio::test]
    async fn assembles_parallel_tool_calls_and_sends_history_back() {
        let (provider, server) = start(vec![Canned::Sse(vec![
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"read","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_b","type":"function","function":{"name":"list_dir","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.md\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":2}}}"#,
            "[DONE]",
        ])])
        .await;
        let mut req = request();
        req.thinking = Some(false);
        req.max_tokens = Some(4096);
        req.tools = vec![ToolSpec {
            name: "read".into(),
            description: "读文件".into(),
            parameters: json!({"type": "object"}),
        }];
        let call = ToolCall {
            id: "call_0".into(),
            name: "read".into(),
            arguments: "{}".into(),
        };
        req.messages.push(ChatMessage {
            reasoning: Some("先读".into()),
            tool_calls: vec![call],
            ..ChatMessage::assistant("")
        });
        req.messages
            .push(ChatMessage::tool_result("call_0", "内容"));

        let acc = collect(&provider, req).await.unwrap();
        assert_eq!(
            acc.tool_calls,
            vec![
                ToolCall {
                    id: "call_a".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"a.md"}"#.into()
                },
                ToolCall {
                    id: "call_b".into(),
                    name: "list_dir".into(),
                    arguments: "{}".into()
                },
            ]
        );
        assert_eq!(acc.stop, Some(StopReason::ToolUse));
        assert_eq!(acc.usage.unwrap().cached_tokens, 2);

        let body = server.seen.lock().unwrap()[0].1.clone();
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["tools"][0]["function"]["name"], "read");
        assert_eq!(
            body["messages"][2],
            json!({
                "role": "assistant",
                "content": "",
                "reasoning_content": "先读",
                "tool_calls": [{"id": "call_0", "type": "function", "function": {"name": "read", "arguments": "{}"}}],
            })
        );
        assert_eq!(
            body["messages"][3],
            json!({"role": "tool", "content": "内容", "tool_call_id": "call_0"})
        );
    }

    #[tokio::test]
    async fn maps_http_errors_to_kinds() {
        let (provider, _) = start(vec![
            Canned::Status(401, r#"{"error":{"message":"Authentication Fails"}}"#),
            Canned::Status(402, r#"{"error":{"message":"Insufficient Balance"}}"#),
            Canned::Status(429, "slow down"),
            Canned::Status(503, ""),
        ])
        .await;
        let auth = collect(&provider, request()).await.unwrap_err();
        assert_eq!(
            auth,
            LlmError::Auth("HTTP 401：Authentication Fails".into())
        );
        assert!(matches!(
            collect(&provider, request()).await,
            Err(LlmError::BadRequest(m)) if m.contains("Insufficient Balance")
        ));
        assert!(matches!(
            collect(&provider, request()).await,
            Err(LlmError::RateLimited(m)) if m.contains("slow down")
        ));
        assert!(matches!(
            collect(&provider, request()).await,
            Err(LlmError::Server(_))
        ));
    }

    #[tokio::test]
    async fn truncated_stream_is_an_error_not_a_clean_finish() {
        let (provider, _) = start(vec![
            Canned::Sse(vec![r#"{"choices":[{"delta":{"content":"半句"}}]}"#]),
            Canned::Sse(vec!["这不是 JSON"]),
        ])
        .await;
        let mut stream = provider
            .stream(request(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            stream.next().await,
            Some(Ok(StreamEvent::TextDelta("半句".into())))
        );
        assert!(matches!(
            stream.next().await,
            Some(Err(LlmError::Network(_)))
        ));
        assert_eq!(stream.next().await, None);

        assert!(matches!(
            collect(&provider, request()).await,
            Err(LlmError::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn stalled_stream_times_out_and_cancel_interrupts() {
        let stall = || Canned::SseThenStall(vec![r#"{"choices":[{"delta":{"content":"等"}}]}"#]);
        let (provider, _) = start(vec![stall(), stall()]).await;
        let provider = provider.with_idle_timeout(Duration::from_millis(200));
        assert_eq!(
            collect(&provider, request()).await.unwrap_err(),
            LlmError::Timeout
        );

        let provider = provider.with_idle_timeout(Duration::from_secs(30));
        let cancel = CancellationToken::new();
        let mut stream = provider.stream(request(), cancel.clone()).await.unwrap();
        assert!(matches!(stream.next().await, Some(Ok(_))));
        cancel.cancel();
        assert_eq!(stream.next().await, Some(Err(LlmError::Cancelled)));
    }

    #[tokio::test]
    async fn retries_transient_failures_but_not_auth() {
        let ok = || {
            Canned::Sse(vec![
                r#"{"choices":[{"delta":{"content":"好"},"finish_reason":"stop"}]}"#,
                "[DONE]",
            ])
        };
        let (provider, server) = start(vec![
            Canned::Status(429, ""),
            Canned::Status(500, ""),
            ok(),
            Canned::Status(401, ""),
            ok(),
        ])
        .await;
        let policy = RetryPolicy {
            max_retries: 3,
            base_delay: Duration::from_millis(1),
        };
        let cancel = CancellationToken::new();
        let stream = open_with_retry(&provider, &request(), &cancel, policy).await;
        assert!(stream.is_ok());
        assert_eq!(server.seen.lock().unwrap().len(), 3);

        let denied = open_with_retry(&provider, &request(), &cancel, policy).await;
        assert!(matches!(denied.err(), Some(LlmError::Auth(_))));
        assert_eq!(server.seen.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn gives_up_after_max_retries() {
        let (provider, server) = start(vec![Canned::Status(503, ""); 3]).await;
        let policy = RetryPolicy {
            max_retries: 2,
            base_delay: Duration::from_millis(1),
        };
        let result =
            open_with_retry(&provider, &request(), &CancellationToken::new(), policy).await;
        assert!(matches!(result.err(), Some(LlmError::Server(_))));
        assert_eq!(server.seen.lock().unwrap().len(), 3);
    }
}
