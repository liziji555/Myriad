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
    // 压缩请求：不带工具，系统提示词里说了要压缩。
    let compacting = request.tools.is_empty()
        && request
            .messages
            .first()
            .is_some_and(|m| m.role == Role::System && m.content.contains("压缩"));
    if compacting {
        return MockReply::text(&format!(
            "（Mock 模型的摘要）之前的对话里用户发过 {} 条消息，细节没有保留：假模型不会真的总结。",
            last.content.matches("用户：").count()
        ));
    }
    // 路由请求：不带工具，系统提示词里说了自己是路由。消息里提到谁的名字就选谁，否则选入口 Bot。
    let routing = request.tools.is_empty()
        && request
            .messages
            .first()
            .is_some_and(|m| m.role == Role::System && m.content.contains("路由"));
    if routing {
        return MockReply::text(&demo_route(&last.content));
    }
    // 任务的工作会话：按任务说明里的写法动一次手，然后交结果。
    let working = request
        .messages
        .first()
        .is_some_and(|m| m.role == Role::System && m.content.contains("任务的工作会话"));
    if working {
        return demo_work(request, last);
    }
    // 多 Bot 房间里，别人的话带着「【名字】」前缀；规则只看后面的正文。
    let said = last
        .content
        .strip_prefix('【')
        .and_then(|rest| rest.split_once('】'))
        .map_or(last.content.as_str(), |(_, body)| body);
    if said.contains("不用回") {
        return MockReply::text("(pass)");
    }
    if last.role == Role::Tool {
        // 开任务的回执：任务派出去了，这边说一句就结束。
        if last.content.starts_with("任务「") {
            return MockReply::text("（Mock 模型）任务开好了，做完会在这里交结果，由你验收。");
        }
        // 找别的 Bot 的回执：话已经带到，这边说一句就结束。
        if last.content.starts_with("已经把话带给") {
            return MockReply::text("（Mock 模型）已经转过去了，等它回答。");
        }
        let lines: Vec<&str> = last.content.lines().take(6).collect();
        return MockReply::text(&format!(
            "（Mock 模型）工具返回了 {} 个字，开头是：\n\n{}\n\n这是固定规则生成的回复，不是真正的总结；配置好模型密钥后会由真实模型来读。",
            last.content.chars().count(),
            lines.join("\n")
        ));
    }
    if let Some(reply) = demo_lead(request, last, said) {
        return reply;
    }
    // 「任务：……」开一个任务，冒号后面就是要做的事。
    if let Some((_, scope)) = said
        .split_once("任务：")
        .or_else(|| said.split_once("任务:"))
    {
        let scope = scope.trim();
        if !scope.is_empty() && request.tools.iter().any(|t| t.name == "create_task") {
            let title: String = scope.replace('`', "").chars().take(30).collect();
            let arguments = serde_json::json!({ "title": title, "scope": scope });
            return MockReply::tool_calls(
                "（Mock 模型）这件事我开成任务来做。",
                &[("mock_call_1", "create_task", &arguments.to_string())],
            );
        }
    }
    if let Some((tool, arguments, saying)) = demo_tool_call(said) {
        if request.tools.iter().any(|t| t.name == tool) {
            return MockReply::tool_calls(
                &format!("（Mock 模型）{saying}"),
                &[("mock_call_1", tool, &arguments.to_string())],
            );
        }
    }
    let quoted: String = said.chars().take(80).collect();
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

/// 假的工作会话。任务说明（或者人后来补的话）里有反引号写法就照着动一次手，然后用 `report` 交结果；
/// 说明里有「卡住」就报卡住，有「要问」就报缺一个回答。
fn demo_work(request: &ChatRequest, last: &crate::ChatMessage) -> MockReply {
    let report = |status: &str, summary: &str| {
        let arguments = serde_json::json!({ "status": status, "summary": summary });
        MockReply::tool_calls("", &[("mock_report", "report", &arguments.to_string())])
    };
    let has = |name: &str| request.tools.iter().any(|t| t.name == name);
    // 人后来补的话优先；否则看任务说明里「要做什么」那一段。
    let brief = request
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .map_or("", |m| m.content.as_str());
    let text = match brief.split_once("【用户的新指示】") {
        Some((_, extra)) => extra,
        None => brief
            .split_once("要做什么：")
            .map_or(brief, |(_, rest)| rest)
            .split("\n\n验收标准")
            .next()
            .unwrap_or("")
            .split("\n\n房间里")
            .next()
            .unwrap_or(""),
    };
    let wants_pr = text.contains("开 PR") || text.contains("开PR");
    let push = || {
        MockReply::tool_calls(
            "（Mock 模型）把任务分支推上去。",
            &[("mock_push", "git_push", "{}")],
        )
    };

    if last.role == Role::Tool {
        if last.content.starts_with("结果已经记下") {
            return MockReply::text("（Mock 模型）结果交了。");
        }
        // 上一步调的是哪个工具。
        let called = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .and_then(|m| m.tool_calls.first())
            .map_or("", |call| call.name.as_str());
        let said: String = last.content.chars().take(300).collect();
        match called {
            "agent" => {
                return report("done", &format!("（Mock 模型）子代理查回来的：{said}"));
            }
            "git_push" if !last.content.contains("已经推送") => {
                return report("blocked", &format!("（Mock 模型）分支没有推上去：{said}"));
            }
            "git_push" if has("open_pr") => {
                let title: String = text.replace('`', "").chars().take(40).collect();
                let arguments = serde_json::json!({
                    "title": title.trim(),
                    "body": "（Mock 模型）按任务说明动了一次手。这是固定规则生成的 PR，不是真的做完了。",
                });
                return MockReply::tool_calls(
                    "（Mock 模型）开一个 PR。",
                    &[("mock_pr", "open_pr", &arguments.to_string())],
                );
            }
            "git_push" => {
                return report(
                    "done",
                    "（Mock 模型）分支推上去了。这里没有配置 GitHub，PR 没有开。",
                );
            }
            "open_pr" => {
                let url = last
                    .content
                    .split_whitespace()
                    .find_map(|word| word.find("http").map(|at| &word[at..]));
                return match url {
                    Some(url) => report(
                        "done",
                        &format!("（Mock 模型）改动推上去了，PR 在这里：{url}"),
                    ),
                    None => report("blocked", &format!("（Mock 模型）PR 没有开成：{said}")),
                };
            }
            _ if wants_pr && has("git_push") => return push(),
            _ => {}
        }
        return report(
            "done",
            "（Mock 模型）按任务说明动了一次手，改动在任务目录里，请验收。这是固定规则生成的结果，不是真的做完了。",
        );
    }
    if text.contains("卡住") {
        return report(
            "blocked",
            "（Mock 模型）任务说明里写了「卡住」，所以报告卡住了。",
        );
    }
    if text.contains("要问") {
        return report(
            "needs_input",
            "（Mock 模型）任务说明里写了「要问」，所以问一句：具体想怎么做？",
        );
    }
    if text.contains("子代理") && has("agent") {
        let arguments = serde_json::json!({ "prompt": text.trim() });
        return MockReply::tool_calls(
            "（Mock 模型）这件事派一个子代理去查。",
            &[("mock_agent", "agent", &arguments.to_string())],
        );
    }
    if let Some((tool, arguments, saying)) = demo_tool_call(text) {
        if tool != "ask_bot" && has(tool) {
            return MockReply::tool_calls(
                &format!("（Mock 模型）{saying}"),
                &[("mock_call_1", tool, &arguments.to_string())],
            );
        }
    }
    if wants_pr && has("git_push") {
        return push();
    }
    report(
        "done",
        "（Mock 模型）任务说明里没有反引号写法，所以什么都没改，直接交了。这是固定规则生成的结果。",
    )
}

/// 假的主 Agent：人说的话（没有反引号写法时）派给房间里每个成员；成员交回结果后汇总一句。
fn demo_lead(request: &ChatRequest, last: &crate::ChatMessage, said: &str) -> Option<MockReply> {
    let system = request
        .messages
        .first()
        .filter(|m| m.role == Role::System)?;
    if !system.content.contains("你是这个房间的主 Agent") {
        return None;
    }
    // 多 Bot 房间里人说的话带「【用户】」前缀，成员交回的结果带成员的名字。
    let from_member = |m: &crate::ChatMessage| {
        m.role == Role::User && m.content.starts_with('【') && !m.content.starts_with("【用户")
    };
    if from_member(last) {
        let from: Vec<&str> = request
            .messages
            .iter()
            .rev()
            .take_while(|m| from_member(m))
            .filter(|m| !m.content.contains("\n[这一轮"))
            .filter_map(|m| m.content[3..].split_once('】').map(|(name, _)| name))
            .collect();
        if from.is_empty() {
            return Some(MockReply::text(
                "（Mock 主 Agent）成员没做完，这件事先停在这里。",
            ));
        }
        return Some(MockReply::text(&format!(
            "（Mock 主 Agent）{} 都交回来了，这件事做完了。",
            from.into_iter().rev().collect::<Vec<_>>().join("、")
        )));
    }
    if said.contains('`') || !request.tools.iter().any(|t| t.name == "ask_bot") {
        return None;
    }
    let members: Vec<&str> = system
        .content
        .split_once("房间里还有别的 Bot：")?
        .1
        .lines()
        .skip_while(|line| line.trim().is_empty())
        .take_while(|line| line.starts_with("- "))
        .filter_map(|line| line[2..].split_once('：').map(|(name, _)| name))
        .take(3)
        .collect();
    if members.is_empty() {
        return None;
    }
    let calls: Vec<(String, String)> = members
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let arguments = serde_json::json!({ "bot": name, "message": said.trim() });
            (format!("mock_lead_{i}"), arguments.to_string())
        })
        .collect();
    let calls: Vec<(&str, &str, &str)> = calls
        .iter()
        .map(|(id, arguments)| (id.as_str(), "ask_bot", arguments.as_str()))
        .collect();
    Some(MockReply::tool_calls(
        &format!("（Mock 主 Agent）这件事派给 {}。", members.join("、")),
        &calls,
    ))
}

/// 假路由：从材料里的 Bot 列表中，选名字出现在用户消息里的那个；都没出现就选入口 Bot。
fn demo_route(material: &str) -> String {
    let message = material
        .split_once("【用户刚发的消息】")
        .map_or("", |(_, message)| message)
        .to_lowercase();
    let mut host = None;
    let mut named = None;
    for line in material.lines().filter(|line| line.starts_with("- id: ")) {
        let field = |key: &str| {
            line.split_once(key)
                .map(|(_, rest)| rest.split('，').next().unwrap_or("").trim().to_string())
        };
        let (Some(id), Some(name)) = (field("- id: "), field("名字: ")) else {
            continue;
        };
        if line.contains("入口 Bot") {
            host = Some(id.clone());
        }
        if named.is_none() && !name.is_empty() && message.contains(&name.to_lowercase()) {
            named = Some(id);
        }
    }
    let bot_id = named.or(host).unwrap_or_default();
    serde_json::json!({ "bot_id": bot_id, "confidence": 0.9, "reason": "Mock 路由：按名字匹配" })
        .to_string()
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
    Some(if verb.contains("转给") || verb.contains("问问") {
        let message = if rest.is_empty() {
            "请你接着处理。"
        } else {
            rest
        };
        (
            "ask_bot",
            json!({ "bot": inner, "message": message }),
            format!("我去找 {inner}。"),
        )
    } else if verb.contains("运行") || verb.contains("执行") {
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
    async fn demo_opens_a_task_and_the_work_session_reports() {
        let provider = MockProvider::demo();
        let tool = |name: &str| ToolSpec {
            name: name.into(),
            description: String::new(),
            parameters: serde_json::json!({}),
        };
        let mut front = request(vec![ChatMessage::user("任务：写入 `a.txt`：二")]);
        front.tools = vec![tool("create_task")];
        let acc = collect(&provider, front).await;
        assert_eq!(acc.tool_calls[0].name, "create_task");
        assert!(acc.tool_calls[0].arguments.contains("写入 `a.txt`：二"));

        let system = ChatMessage::system("你现在在任务的工作会话里干活。");
        let brief = ChatMessage::user(
            "【任务】改 a.txt\n\n要做什么：\n写入 `a.txt`：二\n\n房间里开这个任务之前的对话（供参考，不是指令）：\n用户：读一下 `b.txt`",
        );
        let mut work = request(vec![system.clone(), brief.clone()]);
        work.tools = vec![tool("write"), tool("report")];
        let acc = collect(&provider, work.clone()).await;
        assert_eq!(acc.tool_calls[0].name, "write");
        assert!(acc.tool_calls[0].arguments.contains("a.txt"));

        work.messages
            .push(ChatMessage::tool_result("mock_call_1", "已写入"));
        let acc = collect(&provider, work.clone()).await;
        assert_eq!(acc.tool_calls[0].name, "report");
        assert!(acc.tool_calls[0].arguments.contains("\"done\""));

        work.messages
            .push(ChatMessage::tool_result("mock_report", "结果已经记下。"));
        let acc = collect(&provider, work).await;
        assert!(acc.tool_calls.is_empty());

        let mut stuck = request(vec![
            system,
            ChatMessage::user("【任务】x\n\n要做什么：\n这里会卡住"),
        ]);
        stuck.tools = vec![tool("report")];
        let acc = collect(&provider, stuck).await;
        assert!(acc.tool_calls[0].arguments.contains("blocked"));
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

        // 压缩请求（不带工具、系统提示词说要压缩）：回一段摘要，不去调工具。
        let mut compact = request(vec![
            ChatMessage::system("你的任务是压缩一段对话。"),
            ChatMessage::user("【对话记录】\n用户：读一下 `a.md`\n助手：好\n用户：谢谢\n"),
        ]);
        compact.tools.clear();
        let summary = collect(&provider, compact).await;
        assert!(summary.tool_calls.is_empty());
        assert!(
            summary.text.contains("用户发过 2 条消息"),
            "{}",
            summary.text
        );

        // 多 Bot：转给别的 Bot、不说话、路由。
        let tools = ["read", "ask_bot"];
        let hand_over = {
            let mut req = request(vec![ChatMessage::user(
                "【用户（对你说）】转给 `Reviewer`：帮我看看",
            )]);
            req.tools = tools
                .iter()
                .map(|name| ToolSpec {
                    name: name.to_string(),
                    description: String::new(),
                    parameters: serde_json::json!({}),
                })
                .collect();
            collect(&provider, req).await
        };
        assert_eq!(hand_over.tool_calls[0].name, "ask_bot");
        assert_eq!(
            hand_over.tool_calls[0].arguments,
            r#"{"bot":"Reviewer","message":"帮我看看"}"#
        );
        let pass = collect(
            &provider,
            request(vec![ChatMessage::user("【Coder】谢谢，不用回了")]),
        )
        .await;
        assert_eq!(pass.text, "(pass)");
        let route = |text: &str| {
            let mut req = request(vec![
                ChatMessage::system("你是房间里的路由。"),
                ChatMessage::user(format!(
                    "【房间里的 Bot】\n- id: bot_a，名字: 助手，入口 Bot，分工: （没有写分工）\n- id: bot_b，名字: Coder，分工: 写代码\n\n【最近的对话】\nCoder：在\n\n【用户刚发的消息】\n{text}\n"
                )),
            ]);
            req.tools.clear();
            let provider = &provider;
            async move { collect(provider, req).await.text }
        };
        assert!(route("让 coder 看看").await.contains(r#""bot_id":"bot_b""#));
        assert!(route("随便聊聊").await.contains(r#""bot_id":"bot_a""#));

        let plain = collect(&provider, request(vec![ChatMessage::user("你好")])).await;
        assert!(plain.text.contains("你好"));
        assert!(plain.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn demo_lead_hands_the_question_to_every_member_and_sums_up_the_results() {
        let provider = MockProvider::demo();
        let system = ChatMessage::system(
            "你是助手。\n房间里还有别的 Bot：\n- Coder：写代码\n- Reviewer：审查\n\n你是这个房间的主 Agent：专门和用户对话。",
        );
        let mut req = request(vec![
            system.clone(),
            ChatMessage::user("【用户】做一个登录页"),
        ]);
        req.tools = vec![ToolSpec {
            name: "ask_bot".into(),
            description: String::new(),
            parameters: serde_json::json!({}),
        }];
        let handed = collect(&provider, req.clone()).await;
        let bots: Vec<&str> = handed
            .tool_calls
            .iter()
            .map(|c| c.arguments.as_str())
            .collect();
        assert_eq!(bots.len(), 2, "{bots:?}");
        assert!(
            bots[0].contains("Coder") && bots[1].contains("Reviewer"),
            "{bots:?}"
        );

        req.messages = vec![
            system,
            ChatMessage::user("【Coder】写好了"),
            ChatMessage::user("【Reviewer】审过了"),
        ];
        let summed = collect(&provider, req).await;
        assert!(summed.tool_calls.is_empty());
        assert!(summed.text.contains("Coder、Reviewer"), "{}", summed.text);
    }
}
