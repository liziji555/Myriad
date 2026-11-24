//! 通过网址连的 MCP 服务。
//!
//! 先按 Streamable HTTP 连：每条消息 POST 到那个网址，回应是一段 JSON 或者一串 SSE 事件。
//! 服务只会老的 SSE 办法时（POST 不认）改用它：GET 开一条事件流，服务在流里说往哪 POST，
//! 回应也都从这条流里来。
//!
//! 对上面来说它和本机启动的服务一样是一条 [`Pipe`]：写进去一行一条消息，读出来一行一条消息。

use super::{read_line, McpError, McpSpec, Pipe, Writer, MAX_LINE_BYTES};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use reqwest::{StatusCode, Url};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader, DuplexStream};
use tokio::task::{AbortHandle, JoinSet};

const SESSION: &str = "mcp-session-id";
const VERSION: &str = "mcp-protocol-version";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 断开时告诉服务会话结束了，最多等这么久。
const GOODBYE_TIMEOUT: Duration = Duration::from_secs(5);
/// 一条通知最多等这么久，免得一条发不出去的通知把后面的都堵住。
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_SHOWN_BODY_CHARS: usize = 200;

/// 服务说的话往这里放。拿掉就是线断了。
type Inbox = Arc<tokio::sync::Mutex<Option<Writer>>>;

struct Remote {
    client: reqwest::Client,
    url: Url,
    /// 老的 SSE 办法：服务说的往哪 POST。
    endpoint: Mutex<Option<Url>>,
    session: Mutex<Option<String>>,
    /// 握手时和服务说定的协议版本。
    version: Mutex<Option<String>>,
    inbox: Inbox,
    /// 老的 SSE 办法里一直开着的那条事件流。
    stream: Mutex<Option<AbortHandle>>,
}

pub(super) fn dial(spec: &McpSpec, url: &str) -> Result<Pipe, McpError> {
    let url = Url::parse(url)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https"))
        .ok_or_else(|| McpError("MCP 服务的 url 要是一个 http(s) 地址。".into()))?;
    let mut headers = HeaderMap::new();
    for (name, value) in &spec.headers {
        let bad = || McpError(format!("MCP 服务的请求头「{name}」写得不对。"));
        let key = HeaderName::from_bytes(name.as_bytes()).map_err(|_| bad())?;
        let mut value = HeaderValue::from_str(value).map_err(|_| bad())?;
        value.set_sensitive(true);
        headers.insert(key, value);
    }
    // 没有内置加密后端的 rustls 需要先选一个；已经选过时返回 Err，可以忽略。
    let _ = rustls::crypto::ring::default_provider().install_default();
    // 请求头里可能有令牌：只跟着跳到同一个地方，不带着它去别处。
    let stay = reqwest::redirect::Policy::custom(|attempt| {
        let same = attempt
            .previous()
            .first()
            .is_some_and(|first| first.origin() == attempt.url().origin());
        if same && attempt.previous().len() < 5 {
            attempt.follow()
        } else {
            attempt.stop()
        }
    });
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(stay)
        .user_agent(concat!("SuperAgent/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| McpError(format!("发不出请求：{error}")))?;
    let (writer, outbox) = tokio::io::duplex(64 * 1024);
    let (inbox, reader) = tokio::io::duplex(64 * 1024);
    let inbox: Writer = Box::new(inbox);
    let remote = Arc::new(Remote {
        client,
        url,
        endpoint: Mutex::default(),
        session: Mutex::default(),
        version: Mutex::default(),
        inbox: Arc::new(tokio::sync::Mutex::new(Some(inbox))),
        stream: Mutex::default(),
    });
    tokio::spawn(pump(remote, outbox));
    Ok(Pipe::new(reader, writer))
}

/// 上面写进来的每一行发给服务。线那头丢掉了（断开）就收工：没发完的不发了，告诉服务会话结束。
async fn pump(remote: Arc<Remote>, outbox: DuplexStream) {
    let mut outbox = BufReader::new(outbox);
    let mut line = vec![];
    let mut posts = JoinSet::new();
    let mut running: HashMap<u64, AbortHandle> = HashMap::new();
    while let Ok(true) = read_line(&mut outbox, &mut line).await {
        while posts.try_join_next().is_some() {}
        running.retain(|_, post| !post.is_finished());
        let Ok(message) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        // 不等了的那次请求：通知发出去之后，它那条还开着等回应的连接也关掉。
        let given_up = (message["method"] == "notifications/cancelled")
            .then(|| message["params"]["requestId"].as_u64())
            .flatten()
            .and_then(|id| running.remove(&id));
        let asking = message.get("method").and(message["id"].as_u64());
        let Some(id) = asking else {
            // 通知按顺序发完再往下：握手完成的通知要在后面的请求之前到。
            let _ = tokio::time::timeout(NOTIFY_TIMEOUT, remote.post(message)).await;
            if let Some(given_up) = given_up {
                given_up.abort();
            }
            continue;
        };
        let remote = remote.clone();
        let post = posts.spawn(async move { remote.post(message).await });
        running.insert(id, post);
    }
    drop(posts);
    if let Some(stream) = remote.stream.lock().unwrap().take() {
        stream.abort();
    }
    let session = remote.session.lock().unwrap().take();
    if let Some(session) = session {
        let mut bye = remote
            .client
            .delete(remote.url.clone())
            .header(SESSION, session);
        if let Some(version) = remote.version.lock().unwrap().clone() {
            bye = bye.header(VERSION, version);
        }
        let _ = tokio::time::timeout(GOODBYE_TIMEOUT, bye.send()).await;
    }
}

impl Remote {
    /// 发一条消息。没发成、又有人在等它的回应的，替服务回一条出错。
    async fn post(&self, message: Value) {
        let asking = message
            .get("method")
            .and(message.get("id"))
            .filter(|id| !id.is_null())
            .cloned();
        let outcome = self.exchange(&message, asking.as_ref()).await;
        if let (Err(error), Some(id)) = (outcome, asking) {
            let error = json!({ "code": -32000, "message": error });
            self.deliver(&json!({ "jsonrpc": "2.0", "id": id, "error": error }))
                .await;
        }
    }

    async fn exchange(&self, message: &Value, asking: Option<&Value>) -> Result<(), String> {
        let first = message["method"] == "initialize";
        let response = self.send(message).await?;
        let response = match response.status() {
            StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
                if first && self.listen().await.is_ok() =>
            {
                self.send(message).await?
            }
            _ => response,
        };
        let status = response.status();
        if !status.is_success() {
            if status == StatusCode::NOT_FOUND && self.session.lock().unwrap().is_some() {
                // 服务不认这个会话了：断开，下一次调用重新握手。
                if let Some(id) = asking {
                    let error = json!({ "code": -32000, "message": "MCP 服务那边的会话过期了，再试一次会重新连。" });
                    self.deliver(&json!({ "jsonrpc": "2.0", "id": id, "error": error }))
                        .await;
                }
                self.hang_up().await;
                return Ok(());
            }
            return Err(refusal(status, &body(response).await.unwrap_or_default()));
        }
        if self.endpoint.lock().unwrap().is_some() {
            // 老的 SSE 办法：回应从事件流里来。
            return Ok(());
        }
        if first {
            let session = response.headers().get(SESSION);
            *self.session.lock().unwrap() = session
                .and_then(|session| session.to_str().ok())
                .map(str::to_string);
        }
        let kind = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|kind| kind.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let mut answered = asking.is_none();
        if kind.starts_with("text/event-stream") {
            let mut events = events(response);
            while let Some(Ok(event)) = events.next().await {
                answered |= self.take(&event.data, asking, first).await;
                if answered && asking.is_some() {
                    break;
                }
            }
        } else if kind.starts_with("application/json") {
            answered |= self.take(&body(response).await?, asking, first).await;
        }
        match answered {
            true => Ok(()),
            false => Err("MCP 服务没有回应这次请求，或者连接断了。".into()),
        }
    }

    async fn send(&self, message: &Value) -> Result<reqwest::Response, String> {
        let endpoint = self.endpoint.lock().unwrap().clone();
        let mut request = self
            .client
            .post(endpoint.unwrap_or_else(|| self.url.clone()))
            .header(ACCEPT, "application/json, text/event-stream")
            .json(message);
        if let Some(session) = self.session.lock().unwrap().clone() {
            request = request.header(SESSION, session);
        }
        if let Some(version) = self.version.lock().unwrap().clone() {
            request = request.header(VERSION, version);
        }
        request
            .send()
            .await
            .map_err(|error| self.unreachable(error))
    }

    /// 服务发来的一段 JSON（一条消息，或者一组）。返回里面有没有 `asking` 这次请求的回应。
    async fn take(&self, text: &str, asking: Option<&Value>, first: bool) -> bool {
        if text.len() > MAX_LINE_BYTES {
            return false;
        }
        let messages = match serde_json::from_str::<Value>(text) {
            Ok(Value::Array(messages)) => messages,
            Ok(message) => vec![message],
            Err(_) => return false,
        };
        let mut answered = false;
        for message in messages {
            if message.get("method").is_none() && message.get("id") == asking {
                answered = true;
                if let (true, Some(version)) =
                    (first, message["result"]["protocolVersion"].as_str())
                {
                    *self.version.lock().unwrap() = Some(version.to_string());
                }
            }
            self.deliver(&message).await;
        }
        answered
    }

    async fn deliver(&self, message: &Value) {
        deliver(&self.inbox, message).await;
    }

    async fn hang_up(&self) {
        *self.inbox.lock().await = None;
    }

    /// 老的 SSE 办法：开事件流，等服务说往哪 POST，之后流里来的消息都往上交。
    async fn listen(&self) -> Result<(), String> {
        let request = self
            .client
            .get(self.url.clone())
            .header(ACCEPT, "text/event-stream");
        let response = request.send().await.map_err(|error| error.to_string())?;
        let kind = response.headers().get(CONTENT_TYPE);
        let streaming = kind
            .and_then(|kind| kind.to_str().ok())
            .is_some_and(|kind| kind.to_ascii_lowercase().starts_with("text/event-stream"));
        if !response.status().is_success() || !streaming {
            return Err("不是事件流".into());
        }
        let base = response.url().clone();
        let mut events = events(response);
        let endpoint = loop {
            match tokio::time::timeout(CONNECT_TIMEOUT, events.next()).await {
                Ok(Some(Ok(event))) if event.event == "endpoint" => break event.data,
                Ok(Some(Ok(_))) => continue,
                _ => return Err("服务没有说往哪发".into()),
            }
        };
        // 相对的地址按事件流实际所在的网址来算（可能跳转过）。
        let endpoint = base
            .join(endpoint.trim())
            .map_err(|error| error.to_string())?;
        // 请求头里可能有令牌：不带着它去别的地方。
        if endpoint.origin() != self.url.origin() {
            return Err("服务让把请求发到别的地方".into());
        }
        *self.endpoint.lock().unwrap() = Some(endpoint);
        let inbox = self.inbox.clone();
        let stream = tokio::spawn(async move {
            while let Some(Ok(event)) = events.next().await {
                if let Ok(message) = serde_json::from_str::<Value>(&event.data) {
                    deliver(&inbox, &message).await;
                }
            }
            // 流断了，后面的回应收不到了。
            *inbox.lock().await = None;
        });
        *self.stream.lock().unwrap() = Some(stream.abort_handle());
        Ok(())
    }

    /// 网址里可能带着密钥：出错时只说到主机。
    fn unreachable(&self, error: reqwest::Error) -> String {
        let error = error.without_url();
        let mut cause: &dyn std::error::Error = &error;
        while let Some(deeper) = cause.source() {
            cause = deeper;
        }
        format!(
            "连不上 MCP 服务 {}：{cause}",
            self.url.host_str().unwrap_or_default()
        )
    }
}

async fn deliver(inbox: &Inbox, message: &Value) {
    let mut line = serde_json::to_vec(message).expect("JSON 值总能序列化");
    line.push(b'\n');
    let mut inbox = inbox.lock().await;
    let Some(writer) = inbox.as_mut() else { return };
    if writer.write_all(&line).await.is_err() {
        *inbox = None;
    }
}

/// 回应里的 SSE 事件。一个事件攒到太长还没完的，当成流断了，不再往内存里放。
fn events(
    response: reqwest::Response,
) -> impl futures_util::Stream<Item = Result<eventsource_stream::Event, ()>> + Unpin {
    // 还没完的那个事件有多长；是不是在一行的开头；上一个字节是不是 \r。
    let (mut pending, mut blank, mut after_cr) = (0usize, true, false);
    let capped = response.bytes_stream().map(move |chunk| {
        let chunk = chunk.map_err(|_| ())?;
        for byte in chunk.iter() {
            let crlf = *byte == b'\n' && after_cr;
            after_cr = *byte == b'\r';
            match byte {
                b'\n' if crlf => {}
                // 空行是一个事件的结尾。
                b'\n' | b'\r' if blank => pending = 0,
                b'\n' | b'\r' => blank = true,
                _ => {
                    blank = false;
                    pending += 1;
                    if pending > MAX_LINE_BYTES {
                        return Err(());
                    }
                }
            }
        }
        Ok::<_, ()>(chunk)
    });
    Box::pin(capped.eventsource().map(|event| event.map_err(|_| ())))
}

/// 回应的正文，太长的不要。
async fn body(response: reqwest::Response) -> Result<String, String> {
    let mut bytes = vec![];
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        bytes.extend_from_slice(&chunk.map_err(|_| "MCP 服务的回应没收全。")?);
        if bytes.len() > MAX_LINE_BYTES {
            return Err("MCP 服务的回应太长了。".into());
        }
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn refusal(status: StatusCode, body: &str) -> String {
    let code = status.as_u16();
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return format!(
            "MCP 服务不让进（HTTP {code}）。它要令牌的话，在配置里写 token_env 或者 headers；\
             要在网页上登录（OAuth）的还不支持。"
        );
    }
    let shown: String = body.trim().chars().take(MAX_SHOWN_BODY_CHARS).collect();
    match shown.is_empty() {
        true => format!("MCP 服务返回了 HTTP {code}。"),
        false => format!("MCP 服务返回了 HTTP {code}：{shown}"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{run_demo_server, McpServer, McpTimeouts};
    use super::*;
    use axum::body::Body;
    use axum::extract::State;
    use axum::http::{HeaderMap as Headers, Response};
    use axum::routing::{get, post};
    use axum::Router;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    /// 假服务看到的事。
    #[derive(Default)]
    struct Seen {
        /// 握手过几次。
        sessions: AtomicUsize,
        /// 现在认的会话。
        session: Mutex<String>,
        deleted: AtomicUsize,
        cancelled: AtomicUsize,
        /// 老办法里往事件流里放消息的那一头。
        stream: Mutex<Option<mpsc::UnboundedSender<String>>>,
    }

    /// 让进程内的演示服务答一条消息。通知没有回应。
    async fn answer(message: &Value) -> Option<Value> {
        let mut line = serde_json::to_vec(message).unwrap();
        line.push(b'\n');
        let mut out = vec![];
        run_demo_server(line.as_slice(), &mut out).await.unwrap();
        serde_json::from_slice(&out).ok()
    }

    fn reply(status: u16, kind: &str, body: String) -> Response<Body> {
        Response::builder()
            .status(status)
            .header("content-type", kind)
            .body(Body::from(body))
            .unwrap()
    }

    /// Streamable HTTP：要令牌、要会话；工具清单用 SSE 回，别的用 JSON 回。
    async fn modern(
        State(seen): State<Arc<Seen>>,
        headers: Headers,
        body: String,
    ) -> Response<Body> {
        if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer s3cret") {
            return reply(401, "text/plain", "no".into());
        }
        let message: Value = serde_json::from_str(&body).unwrap();
        let method = message["method"].as_str().unwrap_or("");
        if method == "initialize" {
            let session = format!("s{}", seen.sessions.fetch_add(1, Ordering::SeqCst) + 1);
            *seen.session.lock().unwrap() = session.clone();
            let mut response = reply(
                200,
                "application/json",
                answer(&message).await.unwrap().to_string(),
            );
            response
                .headers_mut()
                .insert(SESSION, session.parse().unwrap());
            return response;
        }
        let session = headers.get(SESSION).and_then(|v| v.to_str().ok());
        if session != Some(seen.session.lock().unwrap().as_str()) || headers.get(VERSION).is_none()
        {
            return reply(404, "text/plain", "no such session".into());
        }
        if method == "notifications/cancelled" {
            seen.cancelled.fetch_add(1, Ordering::SeqCst);
        }
        match answer(&message).await {
            None => reply(202, "text/plain", String::new()),
            Some(result) if method == "tools/list" => {
                let noise = json!({ "jsonrpc": "2.0", "method": "notifications/message" });
                let body = format!("event: message\ndata: {noise}\n\ndata: {result}\n\n");
                reply(200, "text/event-stream", body)
            }
            Some(result) => reply(200, "application/json", result.to_string()),
        }
    }

    async fn goodbye(State(seen): State<Arc<Seen>>) -> Response<Body> {
        seen.deleted.fetch_add(1, Ordering::SeqCst);
        reply(200, "text/plain", String::new())
    }

    /// 老的 SSE 办法：GET 开流，流里先说往哪 POST。
    async fn old_stream(State(seen): State<Arc<Seen>>) -> Response<Body> {
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        tx.send("event: endpoint\ndata: /messages?s=1\n\n".into())
            .unwrap();
        *seen.stream.lock().unwrap() = Some(tx);
        let events = futures_util::stream::unfold(rx, |mut rx| async move {
            let event = rx.recv().await?;
            Some((Ok::<_, std::io::Error>(event), rx))
        });
        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(events))
            .unwrap()
    }

    async fn old_post(State(seen): State<Arc<Seen>>, body: String) -> Response<Body> {
        let message: Value = serde_json::from_str(&body).unwrap();
        if message["params"]["name"] == "quit" {
            // 服务把流关了。
            *seen.stream.lock().unwrap() = None;
        } else if let Some(result) = answer(&message).await {
            let stream = seen.stream.lock().unwrap();
            let _ = stream
                .as_ref()
                .unwrap()
                .send(format!("event: message\ndata: {result}\n\n"));
        }
        reply(202, "text/plain", String::new())
    }

    async fn serve() -> (String, Arc<Seen>) {
        let seen = Arc::new(Seen::default());
        let app = Router::new()
            .route("/mcp", post(modern).delete(goodbye))
            .route("/sse", get(old_stream))
            .route("/messages", post(old_post))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        (base, seen)
    }

    fn server(url: String, token: Option<&str>) -> McpServer {
        McpServer::new(McpSpec {
            name: "remote".into(),
            url: Some(url),
            headers: token
                .map(|token| ("Authorization".to_string(), format!("Bearer {token}")))
                .into_iter()
                .collect(),
            ..McpSpec::default()
        })
    }

    #[tokio::test]
    async fn a_remote_server_is_listed_called_and_told_goodbye() {
        let (base, seen) = serve().await;
        let server = server(format!("{base}/mcp"), Some("s3cret"));
        let tools = server.connect().await.unwrap();
        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(names, ["echo", "add", "fail", "wait", "quit"]);
        let never = CancellationToken::new();
        let sum = server.call("add", json!({ "a": 40, "b": 2 }), &never).await;
        assert_eq!(sum.unwrap(), "42");
        let failed = server.call("fail", json!({}), &never).await.unwrap_err();
        assert!(failed.0.contains("总是失败"), "{failed}");

        // 服务不认这个会话了：这一次失败，下一次调用重新握手。
        *seen.session.lock().unwrap() = "gone".into();
        let expired = server.call("echo", json!({ "text": "a" }), &never).await;
        assert!(expired.unwrap_err().0.contains("过期"));
        let back = server
            .call("echo", json!({ "text": "回来了" }), &never)
            .await;
        assert_eq!(back.unwrap(), "回来了");
        assert_eq!(seen.sessions.load(Ordering::SeqCst), 2);

        server.close().await;
        for _ in 0..100 {
            if seen.deleted.load(Ordering::SeqCst) > 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("断开时没有告诉服务会话结束");
    }

    #[tokio::test]
    async fn a_call_that_is_stopped_tells_the_remote_server_and_lets_go() {
        let (base, seen) = serve().await;
        let server = server(format!("{base}/mcp"), Some("s3cret"));
        server.connect().await.unwrap();
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            stop.cancel();
        });
        let stopped = server.call("wait", json!({ "ms": 60_000 }), &cancel).await;
        assert_eq!(stopped.unwrap_err().0, "已停止。");
        for _ in 0..100 {
            if seen.cancelled.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(seen.cancelled.load(Ordering::SeqCst), 1);
        // 线没坏：接着能用。
        let never = CancellationToken::new();
        let echoed = server.call("echo", json!({ "text": "还在" }), &never).await;
        assert_eq!(echoed.unwrap(), "还在");
    }

    #[tokio::test]
    async fn a_server_that_only_speaks_the_old_sse_way_still_works() {
        let (base, _seen) = serve().await;
        let server = server(format!("{base}/sse"), None);
        let tools = server.connect().await.unwrap();
        assert_eq!(tools.len(), 5);
        let never = CancellationToken::new();
        let sum = server.call("add", json!({ "a": 1, "b": 2 }), &never).await;
        assert_eq!(sum.unwrap(), "3");
        // 流断了：这一次失败，下一次调用重新连。
        assert!(server.call("quit", json!({}), &never).await.is_err());
        let back = server
            .call("echo", json!({ "text": "回来了" }), &never)
            .await;
        assert_eq!(back.unwrap(), "回来了");
    }

    #[tokio::test]
    async fn a_remote_server_that_refuses_or_is_not_there_says_why() {
        let (base, _seen) = serve().await;
        let refused = server(format!("{base}/mcp"), Some("wrong"));
        let error = refused.connect().await.unwrap_err();
        assert!(
            error.0.contains("HTTP 401") && error.0.contains("token_env"),
            "{error}"
        );

        let missing = server(format!("{base}/nowhere"), None);
        let error = missing.connect().await.unwrap_err();
        assert!(error.0.contains("HTTP 404"), "{error}");

        // 网址里的密钥不出现在报错里。
        let nobody = server("http://127.0.0.1:1/mcp?key=s3cret".into(), None)
            .timeouts(McpTimeouts::default());
        let error = nobody.connect().await.unwrap_err();
        assert!(error.0.contains("连不上 MCP 服务 127.0.0.1"), "{error}");
        assert!(!error.0.contains("s3cret"), "{error}");
    }
}
