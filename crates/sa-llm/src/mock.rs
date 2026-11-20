//! 不联网的假模型：测试里按脚本回放，演示时按固定规则应答。

use crate::{
    ChatRequest, EventStream, LlmError, ModelCaps, Provider, Role, StopReason, StreamEvent, Usage,
};
use async_trait::async_trait;
use futures_util::{stream, StreamExt};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// 脚本里的一次回复。
#[derive(Debug, Clone)]
pub enum MockReply {
    /// 依次发出这些事件。脚本里没有 `Done` 时流会在最后一个事件之后直接结束。
    Events(Vec<StreamEvent>),
    /// 调用直接失败，一个事件也不发。
    Fail(LlmError),
    /// 发出这些事件之后中途失败。
    FailAfter(Vec<StreamEvent>, LlmError),
    /// 发出这些事件之后一直挂着，直到被取消。
    Hang(Vec<StreamEvent>),
}

impl MockReply {
    /// 一段纯文本回复，按字符拆成片段。
    pub fn text(text: &str) -> Self {
        let mut events: Vec<_> = text
            .chars()
            .map(|c| StreamEvent::TextDelta(c.to_string()))
            .collect();
        events.push(StreamEvent::Usage(Usage {
            input_tokens: 10,
            output_tokens: text.chars().count() as u64,
            cached_tokens: 0,
        }));
        events.push(StreamEvent::Done {
            stop: StopReason::EndTurn,
        });
        Self::Events(events)
    }

    /// 先说一句话（可为空），再调用若干工具。每项是 `(调用 id, 工具名, 参数 JSON)`。
    pub fn tool_calls(text: &str, calls: &[(&str, &str, &str)]) -> Self {
        let mut events = vec![];
        if !text.is_empty() {
            events.push(StreamEvent::TextDelta(text.to_string()));
        }
        for (id, name, arguments) in calls {
            events.push(StreamEvent::ToolCallStart {
                id: id.to_string(),
                name: name.to_string(),
            });
            // 拆成两段，覆盖「参数分片到达」的情况。
            let middle = arguments
                .char_indices()
                .nth(arguments.chars().count() / 2)
                .map_or(0, |(i, _)| i);
            for fragment in [&arguments[..middle], &arguments[middle..]] {
                events.push(StreamEvent::ToolCallArgsDelta {
                    id: id.to_string(),
                    json_fragment: fragment.to_string(),
                });
            }
        }
        events.push(StreamEvent::Usage(Usage {
            input_tokens: 10,
            output_tokens: 5,
            cached_tokens: 0,
        }));
        events.push(StreamEvent::Done {
            stop: StopReason::ToolUse,
        });
        Self::Events(events)
    }
}

enum Mode {
    Scripted(Mutex<VecDeque<MockReply>>),
    Demo,
}

pub struct MockProvider {
    mode: Mode,
    /// 两个片段之间的间隔。
    chunk_delay: Duration,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}

impl MockProvider {
    /// 按顺序回放脚本，每次调用用掉一条；脚本用完之后的调用会失败。
    pub fn scripted(replies: impl IntoIterator<Item = MockReply>) -> Self {
        Self {
            mode: Mode::Scripted(Mutex::new(replies.into_iter().collect())),
            chunk_delay: Duration::ZERO,
            requests: Arc::default(),
        }
    }

    /// 演示模式：不需要密钥就能把整条链路跑通。
    ///
    /// 规则：用户消息里有用反引号括起来的路径（例如 `` `README.md` ``）就调用 `read` 读它，
    /// 拿到结果后复述开头几行；否则回一段固定的话。
    pub fn demo() -> Self {
        Self {
            mode: Mode::Demo,
            chunk_delay: Duration::from_millis(25),
            requests: Arc::default(),
        }
    }

    pub fn with_chunk_delay(mut self, delay: Duration) -> Self {
        self.chunk_delay = delay;
        self
    }

    /// 到目前为止收到的全部请求。
    pub fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// 脚本里还剩几条没用。
    pub fn remaining(&self) -> usize {
        match &self.mode {
            Mode::Scripted(replies) => replies.lock().unwrap().len(),
            Mode::Demo => 0,
        }
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, LlmError> {
        self.requests.lock().unwrap().push(request.clone());
        let reply = match &self.mode {
            Mode::Scripted(replies) => replies
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| LlmError::Protocol("Mock 脚本已经用完".into()))?,
            Mode::Demo => demo_reply(&request),
        };
        let (events, ending) = match reply {
            MockReply::Events(events) => (events, Ending::Finish),
            MockReply::Fail(error) => return Err(error),
            MockReply::FailAfter(events, error) => (events, Ending::Fail(error)),
            MockReply::Hang(events) => (events, Ending::Hang),
        };
        let delay = self.chunk_delay;
        let state = (events.into_iter(), Some(ending), cancel);
        Ok(
            stream::unfold(state, move |(mut events, mut ending, cancel)| async move {
                if cancel.is_cancelled() {
                    ending.take()?;
                    return Some((Err(LlmError::Cancelled), (events, None, cancel)));
                }
                if let Some(event) = events.next() {
                    if !delay.is_zero() {
                        tokio::select! {
                            _ = tokio::time::sleep(delay) => {}
                            _ = cancel.cancelled() => {
                                return Some((Err(LlmError::Cancelled), (events, None, cancel)));
                            }
                        }
                    }
                    return Some((Ok(event), (events, ending, cancel)));
                }
                match ending.take()? {
                    Ending::Finish => None,
                    Ending::Fail(error) => Some((Err(error), (events, None, cancel))),
                    Ending::Hang => {
                        cancel.cancelled().await;
                        Some((Err(LlmError::Cancelled), (events, None, cancel)))
                    }
                }
            })
            .boxed(),
        )
    }

    fn capabilities(&self, _model: &str) -> ModelCaps {
        ModelCaps {
            context_window: 1_000_000,
            supports_tools: true,
        }
    }
}

enum Ending {
    Finish,
    Fail(LlmError),
    Hang,
}

fn demo_reply(request: &ChatRequest) -> MockReply {
    let Some(last) = request.messages.last() else {
        return MockReply::text("（Mock 模型）没有收到消息。");
    };
    if last.role == Role::Tool {
        let lines: Vec<&str> = last.content.lines().take(6).collect();
        return MockReply::text(&format!(
            "（Mock 模型）工具返回了 {} 个字，开头是：\n\n{}\n\n这是固定规则生成的回复，不是真正的总结；配置好模型密钥后会由真实模型来读。",
            last.content.chars().count(),
            lines.join("\n")
        ));
    }
    if let Some((tool, arguments, saying)) = demo_tool_call(&last.content) {
        if request.tools.iter().any(|t| t.name == tool) {
            return MockReply::tool_calls(
                &format!("（Mock 模型）{saying}"),
                &[("mock_call_1", tool, &arguments.to_string())],
            );
        }
    }
    let quoted: String = last.content.chars().take(80).collect();
    // 报出上下文里有几条用户消息：重启之后数字接着涨，说明历史带上了。
    let turn = request
        .messages
        .iter()
        .filter(|m| m.role == Role::User)
        .count();
    MockReply::text(&format!(
        "你好！我是 Mock 模型，只会按固定规则回话，用来在没有密钥时把流程跑通。\n\n你刚才说的是：「{quoted}」（这是这个会话里你的第 {turn} 条消息）\n\n想看工具调用的话，发一条带反引号的消息，例如：读一下 `README.md`、写入 `笔记.md`：内容、修改 `笔记.md`：旧 => 新、运行 `git status`。"
    ))
}

/// 按消息的写法决定调哪个工具：反引号前面的动词决定工具，反引号里面是路径或命令，
/// 反引号后面（去掉冒号）是要写的内容。
fn demo_tool_call(text: &str) -> Option<(&'static str, serde_json::Value, String)> {
    let start = text.find('`')?;
    let len = text[start + 1..].find('`')?;
    let inner = text[start + 1..start + 1 + len].trim();
    if inner.is_empty() {
        return None;
    }
    let verb = &text[..start];
    let rest = text[start + len + 2..]
        .trim_start_matches(|c: char| c.is_whitespace() || c == '：' || c == ':')
        .trim_end();
    use serde_json::json;
    Some(if verb.contains("运行") || verb.contains("执行") {
        (
            "shell",
            json!({ "command": inner }),
            format!("我来执行 {inner}。"),
        )
    } else if verb.contains("写入") {
        let content = if rest.is_empty() {
            "这是 Mock 模型写的一行。".to_string()
        } else {
            rest.to_string()
        };
        (
            "write",
            json!({ "path": inner, "content": format!("{content}\n") }),
            format!("我来写 {inner}。"),
        )
    } else if let (true, Some((old, new))) = (verb.contains("修改"), rest.split_once("=>")) {
        (
            "edit",
            json!({ "path": inner, "old_string": old.trim(), "new_string": new.trim() }),
            format!("我来改 {inner}。"),
        )
    } else {
        (
            "read",
            json!({ "path": inner }),
            format!("我先读一下 {inner}。"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Accumulator, ChatMessage, ToolSpec};

    fn request(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            model: "mock".into(),
            messages,
            tools: vec![ToolSpec {
                name: "read".into(),
                description: String::new(),
                parameters: serde_json::json!({}),
            }],
            max_tokens: None,
            thinking: None,
            session_id: None,
        }
    }

    async fn collect(provider: &MockProvider, req: ChatRequest) -> Accumulator {
        let mut stream = provider
            .stream(req, CancellationToken::new())
            .await
            .unwrap();
        let mut acc = Accumulator::default();
        while let Some(event) = stream.next().await {
            acc.push(&event.unwrap());
        }
        acc
    }

    #[tokio::test]
    async fn scripted_replies_play_in_order_then_run_out() {
        let provider = MockProvider::scripted([
            MockReply::text("一"),
            MockReply::Fail(LlmError::Auth("坏密钥".into())),
        ]);
        let req = request(vec![ChatMessage::user("嗨")]);
        assert_eq!(collect(&provider, req.clone()).await.text, "一");
        let stream = |req| provider.stream(req, CancellationToken::new());
        assert_eq!(
            stream(req.clone()).await.err(),
            Some(LlmError::Auth("坏密钥".into()))
        );
        assert!(matches!(
            stream(req).await.err(),
            Some(LlmError::Protocol(_))
        ));
        assert_eq!(provider.requests().len(), 3);
    }

    #[tokio::test]
    async fn tool_call_arguments_arrive_in_fragments() {
        let args = r#"{"path":"说明.md"}"#;
        let provider =
            MockProvider::scripted([MockReply::tool_calls("看看", &[("c1", "read", args)])]);
        let acc = collect(&provider, request(vec![ChatMessage::user("x")])).await;
        assert_eq!(acc.tool_calls[0].arguments, args);
        assert_eq!(acc.stop, Some(StopReason::ToolUse));
    }

    #[tokio::test]
    async fn hang_ends_with_cancelled() {
        let provider = MockProvider::scripted([MockReply::Hang(vec![StreamEvent::TextDelta(
            "写到一半".into(),
        )])]);
        let cancel = CancellationToken::new();
        let mut stream = provider
            .stream(request(vec![ChatMessage::user("x")]), cancel.clone())
            .await
            .unwrap();
        assert!(matches!(
            stream.next().await,
            Some(Ok(StreamEvent::TextDelta(_)))
        ));
        cancel.cancel();
        assert_eq!(stream.next().await, Some(Err(LlmError::Cancelled)));
        assert_eq!(stream.next().await, None);
    }

    #[tokio::test]
    async fn demo_reads_backticked_path_then_reports() {
        let provider = MockProvider::demo().with_chunk_delay(Duration::ZERO);
        let first = collect(
            &provider,
            request(vec![ChatMessage::user("读一下 `docs/a.md` 并总结")]),
        )
        .await;
        assert_eq!(first.tool_calls.len(), 1);
        assert_eq!(first.tool_calls[0].name, "read");
        assert_eq!(first.tool_calls[0].arguments, r#"{"path":"docs/a.md"}"#);

        let second = collect(
            &provider,
            request(vec![
                ChatMessage::user("读一下 `docs/a.md` 并总结"),
                ChatMessage::tool_result("mock_call_1", "第一行\n第二行"),
            ]),
        )
        .await;
        assert!(second.tool_calls.is_empty());
        assert!(second.text.contains("第一行"));

        // 其他工具：动词决定调哪个；请求里没有这个工具时退回普通回复。
        let tools = ["read", "write", "edit", "shell"];
        let call = |text: &str| {
            let mut req = request(vec![ChatMessage::user(text)]);
            req.tools = tools
                .iter()
                .map(|name| ToolSpec {
                    name: name.to_string(),
                    description: String::new(),
                    parameters: serde_json::json!({}),
                })
                .collect();
            let provider = &provider;
            async move {
                let acc = collect(provider, req).await;
                let call = &acc.tool_calls[0];
                (call.name.clone(), call.arguments.clone())
            }
        };
        assert_eq!(
            call("运行 `cargo test -q`").await,
            ("shell".into(), r#"{"command":"cargo test -q"}"#.into())
        );
        assert_eq!(
            call("写入 `笔记.md`：第一行").await,
            (
                "write".into(),
                r#"{"content":"第一行\n","path":"笔记.md"}"#.into()
            )
        );
        assert_eq!(
            call("修改 `笔记.md`：第一行 => 改过了").await,
            (
                "edit".into(),
                r#"{"new_string":"改过了","old_string":"第一行","path":"笔记.md"}"#.into()
            )
        );
        let no_shell = collect(&provider, request(vec![ChatMessage::user("运行 `ls`")])).await;
        assert!(no_shell.tool_calls.is_empty());

        let plain = collect(&provider, request(vec![ChatMessage::user("你好")])).await;
        assert!(plain.text.contains("你好"));
        assert!(plain.tool_calls.is_empty());
    }
}
