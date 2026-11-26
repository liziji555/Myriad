//! 一轮对话：调模型 → 执行它要的工具 → 把结果交回去，直到模型不再要工具。

use crate::compact::{self, Limits, COMPACT_PROMPT};
use crate::transcript::{EntryBody, Transcript, TranscriptError};
use async_trait::async_trait;
use futures_util::stream::{self, StreamExt};
use sa_llm::{
    open_with_retry, Accumulator, ChatMessage, ChatRequest, LlmError, Provider, RetryPolicy,
    StopReason, StreamEvent, ToolCall, ToolSpec, Usage,
};
use sa_tools::{HookOutcome, HookRunner, Permission, ToolCtx, ToolRegistry};
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// 同时最多跑几个工具。
const MAX_PARALLEL_TOOLS: usize = 10;
const DENIED_RESULT: &str =
    "用户没有批准这次操作，它没有执行。不要换个办法绕过去；告诉用户你原本想做什么，问他想怎么办。";
/// 压缩连续失败这么多次就不再试了，免得每一步都白花一次模型调用。
const MAX_COMPACT_FAILURES: u32 = 3;
const CHANGED_RESULT: &str =
    "等用户批准的这段时间里，这个操作的目标变了，和批准的不是同一件事，所以没有执行。请重新调用一次。";
const CANCELLED_RESULT: &str = "（用户在这个工具返回之前点了停止，结果未知。）";

#[derive(Debug, Clone)]
pub struct TurnConfig {
    pub model: String,
    pub system_prompt: String,
    /// 一轮里最多调用几次模型。防止模型没完没了地调工具。
    pub max_steps: usize,
    pub max_tokens: Option<u32>,
    pub thinking: Option<bool>,
    /// 会话 ID，原样带在每次模型请求上。
    pub session_id: Option<String>,
    /// 模型的上下文窗口有多大（token）。对话快占满时自动压缩前面的部分。`None` 表示不压缩。
    pub context_window: Option<usize>,
    /// 工具结果太长被截断时，完整内容存到这个目录里。`None` 表示不存。
    pub spill_dir: Option<std::path::PathBuf>,
    pub retry: RetryPolicy,
    /// 回复因单次输出长度上限被截断时，自动让模型续写的最大次数（0 表示不续写直接报截断）。
    pub max_truncated_recoveries: usize,
    /// 可选的生命周期钩子（PreToolUse / PostToolUse / Stop）。
    pub hooks: Option<Arc<HookRunner>>,
}

/// 人发来的一条输入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInput {
    pub content: String,
    pub message_id: Option<String>,
    pub message_seq: Option<i64>,
}

/// 这一轮的输入从哪来。每次调模型之前都会取一次，所以人在 Bot 干活时插的话下一步就能看到。
#[async_trait]
pub trait Inbox: Send {
    async fn drain(&mut self) -> Vec<UserInput>;
}

/// 一步是怎么结束的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepEnd {
    Ok,
    /// 这一步自己没做成，原因交给了模型。
    Failed,
    /// 人点了停止，这一步没做完。
    Stopped,
}

/// 这一轮的过程报给谁。
#[async_trait]
pub trait Observer: Send + Sync {
    /// 模型又说了一小段话。
    async fn text_delta(&self, text: &str);
    /// 一步开始了。`arguments` 是模型给这个工具的参数原文（JSON），内部的步骤（压缩）是空的。
    async fn tool_started(&self, call_id: &str, tool: &str, title: &str, arguments: &str);
    /// 一步结束了。`output` 是工具交回来的结果（失败时是原因）；被停下的、内部的步骤是空的。
    async fn tool_finished(&self, call_id: &str, end: StepEnd, output: &str);
    /// 一次模型调用结束，报告用量。
    async fn usage(&self, model: &str, usage: Usage);
    /// 这次调用要人点头才能执行。返回 `true` 表示批准。默认不批准。
    ///
    /// 等多久、要不要记住「总是允许」都由实现决定；这一轮被取消时这个等待会被丢弃。
    async fn request_approval(&self, _request: ApprovalRequest<'_>) -> bool {
        false
    }
}

/// 一次等人批准的工具调用。
#[derive(Debug, Clone, Copy)]
pub struct ApprovalRequest<'a> {
    pub call_id: &'a str,
    pub tool: &'a str,
    /// 给人看的一句话，例如「执行 cargo test」。
    pub title: &'a str,
    /// 为什么要问。
    pub reason: &'a str,
    pub input: &'a Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEnd {
    /// 模型说完了。
    Completed,
    /// 回复到了模型的输出长度上限，被截断了。已经写出来的部分在记录里。
    Truncated,
    /// 被取消（人点了停止，或者服务在关闭）。
    Cancelled,
    /// 调模型的次数到了上限。
    MaxSteps,
    /// 对话已经放不进模型的上下文窗口，压缩之后还是放不下（或者压缩一直失败）。
    ContextLimit,
    /// 出错了。文字是给人看的原因。
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOutcome {
    pub end: TurnEnd,
    /// 调了几次模型。
    pub steps: usize,
    pub usage: Usage,
}

/// 跑一轮。
///
/// 记录先落盘再做下一步：人的输入在调模型之前写好，模型的回复在执行工具之前写好。
/// 这样不管在哪一步中断，重新打开记录都能接着聊。
#[allow(clippy::too_many_arguments)]
pub async fn run_turn(
    transcript: &mut Transcript,
    provider: &dyn Provider,
    tools: &ToolRegistry,
    tool_ctx: &ToolCtx,
    config: &TurnConfig,
    inbox: &mut dyn Inbox,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> TurnOutcome {
    let mut state = Turn {
        steps: 0,
        usage: Usage::default(),
        said_something: false,
        compactions: 0,
        compact_failures: 0,
        micro_compacted: false,
        reactive_compacted: false,
        truncated_recoveries: 0,
        stop_hook_retries: 0,
        last_batch_sig: None,
        repeat_streak: 0,
    };
    let end = match state
        .run(
            transcript, provider, tools, tool_ctx, config, inbox, observer, cancel,
        )
        .await
    {
        Ok(end) => end,
        Err(error) => TurnEnd::Failed(error.to_string()),
    };
    TurnOutcome {
        end,
        steps: state.steps,
        usage: state.usage,
    }
}

fn tool_specs(tools: &ToolRegistry) -> Vec<ToolSpec> {
    tools
        .iter()
        .map(|tool| ToolSpec {
            name: tool.name().to_string(),
            description: tool.description(),
            parameters: tool.input_schema(),
        })
        .collect()
}

/// 每次调用模型都要带、压缩不掉的那部分（系统提示词和工具说明）有多大，估算的 token。
///
/// 和 [`run_turn`] 判断要不要压缩时用的是同一个算法。
pub fn fixed_context(system_prompt: &str, tools: &ToolRegistry) -> usize {
    fixed_of(system_prompt, &tool_specs(tools))
}

fn fixed_of(system_prompt: &str, specs: &[ToolSpec]) -> usize {
    compact::estimate(system_prompt) + compact::estimate_tools(specs)
}

struct Turn {
    steps: usize,
    usage: Usage,
    /// 这一轮里模型已经说过话。之后每次调用的第一段文字前面补一个空行，
    /// 这样多次调用的文字连在一起时还是分段的。
    said_something: bool,
    /// 这一轮里试过几次压缩。
    compactions: u32,
    /// 压缩连续失败了几次。
    compact_failures: u32,
    /// 是否通过零成本微压缩折叠了旧工具结果。
    micro_compacted: bool,
    /// 是否已触发过 413 / 上下文超限应急压缩重试（每轮最多 1 次）。
    reactive_compacted: bool,
    /// 连续因 MaxTokens 截断自动续写的次数。
    truncated_recoveries: usize,
    /// Stop 钩子拦截重试次数（最多 2 次）。
    stop_hook_retries: usize,
    /// 上一步工具调用批次指纹，用于检测死循环重试。
    last_batch_sig: Option<String>,
    /// 连续重复同一工具调用的次数。
    repeat_streak: usize,
}

impl Turn {
    #[allow(clippy::too_many_arguments)]
    async fn run(
        &mut self,
        transcript: &mut Transcript,
        provider: &dyn Provider,
        tools: &ToolRegistry,
        tool_ctx: &ToolCtx,
        config: &TurnConfig,
        inbox: &mut dyn Inbox,
        observer: &dyn Observer,
        cancel: &CancellationToken,
    ) -> Result<TurnEnd, TranscriptError> {
        let specs = tool_specs(tools);

        // 头一批是叫它起来的那几句，上层已经取走了，总要记下；之后停下了就先收手：
        // 停下之后进来的话留在收件箱里，下一轮再答，不能在这里读掉。
        let mut first = true;
        loop {
            if !first && cancel.is_cancelled() {
                return Ok(TurnEnd::Cancelled);
            }
            first = false;
            for input in inbox.drain().await {
                transcript.append(EntryBody::User {
                    content: input.content,
                    message_id: input.message_id,
                    message_seq: input.message_seq,
                })?;
            }
            if cancel.is_cancelled() {
                return Ok(TurnEnd::Cancelled);
            }
            if self.steps >= config.max_steps {
                return Ok(TurnEnd::MaxSteps);
            }
            if let Some(window) = config.context_window {
                let limits = Limits::new(window, config.max_tokens);
                // 系统提示词和工具说明每次都要带，压缩不掉。
                let fixed = fixed_of(&config.system_prompt, &specs);
                if let Some(end) = self
                    .fit_context(
                        transcript, provider, config, limits, fixed, observer, cancel,
                    )
                    .await?
                {
                    return Ok(end);
                }
            }
            self.steps += 1;

            let mut messages = vec![ChatMessage::system(config.system_prompt.clone())];
            let history = if self.micro_compacted {
                compact::micro_compact_messages(transcript.to_chat_messages()).0
            } else {
                transcript.to_chat_messages()
            };
            messages.extend(history);
            // 快到步数上限时温柔提醒收尾，避免模型还在探索时突然被硬切。
            if config.max_steps >= 6 && self.steps == config.max_steps {
                messages.push(ChatMessage::user(
                    "[系统提示] 这是本轮的最后一步模型调用，请直接总结已完成的工作、结论和下一步建议，不要再发起新的工具调用。",
                ));
            }
            let request = ChatRequest {
                model: config.model.clone(),
                messages,
                tools: specs.clone(),
                max_tokens: config.max_tokens,
                thinking: config.thinking,
                session_id: config.session_id.clone(),
            };

            let (reply, failure) = self
                .call_model(provider, &request, config, observer, cancel)
                .await;
            if let Some(usage) = reply.usage {
                self.usage.add(usage);
                observer.usage(&config.model, usage).await;
            }
            if let Some(error) = failure {
                // 应急恢复：如果估算偏乐观导致模型服务报上下文超限，主动压缩一次再重试，不直接失败。
                if reply.text.is_empty() && !self.reactive_compacted && is_context_overflow(&error)
                {
                    if let Some(window) = config.context_window {
                        self.reactive_compacted = true;
                        let limits = Limits::new(window, config.max_tokens);
                        let fixed = fixed_of(&config.system_prompt, &specs);
                        if self
                            .force_compact(
                                transcript, provider, config, limits, fixed, observer, cancel,
                            )
                            .await?
                        {
                            continue;
                        }
                    }
                }
                // 人已经看到的那部分文字留在记录里，下次接着聊时模型知道自己说到哪了。
                // 没收完整的工具调用丢掉：参数不全，执行不了。
                if !reply.text.is_empty() {
                    transcript.append(EntryBody::Assistant {
                        content: reply.text,
                        reasoning: none_if_empty(reply.reasoning),
                        tool_calls: vec![],
                    })?;
                }
                return Ok(match error {
                    LlmError::Cancelled => TurnEnd::Cancelled,
                    other => TurnEnd::Failed(other.to_string()),
                });
            }

            let truncated = reply.stop == Some(StopReason::MaxTokens);
            let calls = reply.tool_calls;
            let reply_text = reply.text.clone();
            transcript.append(EntryBody::Assistant {
                content: reply.text,
                reasoning: none_if_empty(reply.reasoning),
                tool_calls: calls.clone(),
            })?;
            // 要不要继续只看有没有工具调用，不看模型自己报的结束原因。
            if calls.is_empty() {
                if truncated && self.truncated_recoveries < config.max_truncated_recoveries {
                    self.truncated_recoveries += 1;
                    transcript.append(EntryBody::User {
                        content: "刚才的回复因单次输出长度上限被截断，请紧接着上一句继续写完，不要重复已输出的内容。".into(),
                        message_id: None,
                        message_seq: None,
                    })?;
                    continue;
                }
                if !truncated {
                    if let Some(hooks) = config.hooks.as_deref() {
                        if let HookOutcome::Block(feedback) =
                            hooks.run_stop(&reply_text, cancel).await
                        {
                            if self.stop_hook_retries < 2 {
                                self.stop_hook_retries += 1;
                                transcript.append(EntryBody::User {
                                    content: format!(
                                        "[Stop 钩子验证未通过，请根据以下反馈修正后再完成回复]\n{feedback}"
                                    ),
                                    message_id: None,
                                    message_seq: None,
                                })?;
                                continue;
                            }
                            return Ok(TurnEnd::Failed(format!("Stop 钩子验证未通过：{feedback}")));
                        }
                    }
                }
                return Ok(if truncated {
                    TurnEnd::Truncated
                } else {
                    TurnEnd::Completed
                });
            }

            self.truncated_recoveries = 0;
            let batch_sig = batch_fingerprint(&calls);
            if self.last_batch_sig.as_deref() == Some(batch_sig.as_str()) {
                self.repeat_streak += 1;
            } else {
                self.last_batch_sig = Some(batch_sig);
                self.repeat_streak = 1;
            }

            let results = run_tools(
                &calls,
                tools,
                tool_ctx,
                config.spill_dir.as_deref(),
                config.hooks.as_deref(),
                observer,
                cancel,
            )
            .await;
            let cancelled = results.iter().any(Option::is_none);
            let last_idx = calls.len().saturating_sub(1);
            for (idx, (call, result)) in calls.iter().zip(results).enumerate() {
                let (mut content, is_error) = match result {
                    Some(Ok(text)) => (text, false),
                    Some(Err(text)) => (text, true),
                    None => (CANCELLED_RESULT.to_string(), true),
                };
                if idx == last_idx && self.repeat_streak >= 3 && !cancelled {
                    content.push_str(
                        "\n\n[系统提醒] 你已经连续 3 次执行了完全相同的工具调用。请不要继续原地重复，先分析卡点、换一种方案，或直接向用户说明当前进展与问题。",
                    );
                }
                transcript.append(EntryBody::ToolResult {
                    tool_call_id: call.id.clone(),
                    content,
                    is_error,
                })?;
            }
            if cancelled {
                return Ok(TurnEnd::Cancelled);
            }
        }
    }

    /// 对话快占满窗口时压缩前面的部分。返回 `Some` 表示这一轮到此为止。
    #[allow(clippy::too_many_arguments)]
    async fn fit_context(
        &mut self,
        transcript: &mut Transcript,
        provider: &dyn Provider,
        config: &TurnConfig,
        limits: Limits,
        fixed: usize,
        observer: &dyn Observer,
        cancel: &CancellationToken,
    ) -> Result<Option<TurnEnd>, TranscriptError> {
        let size = |transcript: &Transcript, micro: bool| {
            let msgs = transcript.to_chat_messages();
            let msgs = if micro {
                compact::micro_compact_messages(msgs).0
            } else {
                msgs
            };
            fixed + compact::estimate_messages(&msgs)
        };
        if size(transcript, self.micro_compacted) <= limits.threshold {
            return Ok(None);
        }
        // 先尝试零成本微压缩（只折叠早期大块工具输出，不调模型）；能降到阈值以下就直接继续。
        if !self.micro_compacted {
            let (folded_msgs, changed) =
                compact::micro_compact_messages(transcript.to_chat_messages());
            if changed && fixed + compact::estimate_messages(&folded_msgs) <= limits.threshold {
                self.micro_compacted = true;
                return Ok(None);
            }
        }
        let plan = (self.compact_failures < MAX_COMPACT_FAILURES)
            .then(|| {
                compact::plan(
                    transcript,
                    limits.keep(),
                    // 人刚说的话只要不会单独把对话顶过压缩线，就原样留下。
                    limits.threshold.saturating_sub(fixed),
                    limits
                        .hard
                        .saturating_sub(compact::estimate(COMPACT_PROMPT)),
                )
            })
            .flatten();
        if let Some(plan) = plan {
            self.compactions += 1;
            // 压缩也是一步过程，和工具调用一样显示出来。
            let step = format!(
                "compact-{}-{}",
                transcript.entries().len(),
                self.compactions
            );
            observer
                .tool_started(&step, "compact", "对话太长，压缩前面的内容", "")
                .await;
            let summary = self
                .summarise(provider, config, plan.text, observer, cancel)
                .await;
            let end = match &summary {
                Ok(_) => StepEnd::Ok,
                Err(LlmError::Cancelled) => StepEnd::Stopped,
                Err(_) => StepEnd::Failed,
            };
            observer.tool_finished(&step, end, "").await;
            match summary {
                Ok(content) => {
                    transcript.append(EntryBody::Summary {
                        content,
                        kept: plan.kept,
                    })?;
                    self.compact_failures = 0;
                    self.micro_compacted = false;
                }
                Err(LlmError::Cancelled) => return Ok(Some(TurnEnd::Cancelled)),
                Err(_) => self.compact_failures += 1,
            }
        }
        // 没压下来但还放得下：照常继续，下一步再试。真的放不下了才停。
        Ok((size(transcript, self.micro_compacted) > limits.hard).then_some(TurnEnd::ContextLimit))
    }

    /// 遇到模型服务 413 / context_length_exceeded 报错时强制触发一次摘要压缩。
    #[allow(clippy::too_many_arguments)]
    async fn force_compact(
        &mut self,
        transcript: &mut Transcript,
        provider: &dyn Provider,
        config: &TurnConfig,
        limits: Limits,
        fixed: usize,
        observer: &dyn Observer,
        cancel: &CancellationToken,
    ) -> Result<bool, TranscriptError> {
        let Some(plan) = compact::plan(
            transcript,
            limits.keep() / 2,
            limits.threshold.saturating_sub(fixed),
            limits
                .hard
                .saturating_sub(compact::estimate(COMPACT_PROMPT)),
        ) else {
            return Ok(false);
        };
        self.compactions += 1;
        let step = format!(
            "compact-{}-{}",
            transcript.entries().len(),
            self.compactions
        );
        observer
            .tool_started(&step, "compact", "上下文超限，应急压缩历史对话", "")
            .await;
        let summary = self
            .summarise(provider, config, plan.text, observer, cancel)
            .await;
        let end = match &summary {
            Ok(_) => StepEnd::Ok,
            Err(LlmError::Cancelled) => StepEnd::Stopped,
            Err(_) => StepEnd::Failed,
        };
        observer.tool_finished(&step, end, "").await;
        match summary {
            Ok(content) => {
                transcript.append(EntryBody::Summary {
                    content,
                    kept: plan.kept,
                })?;
                self.micro_compacted = false;
                Ok(true)
            }
            Err(_) => Ok(false),
        }
    }

    /// 让模型把一段对话写成摘要。不带工具，文字不报给人看。
    async fn summarise(
        &mut self,
        provider: &dyn Provider,
        config: &TurnConfig,
        material: String,
        observer: &dyn Observer,
        cancel: &CancellationToken,
    ) -> Result<String, LlmError> {
        let request = ChatRequest {
            model: config.model.clone(),
            messages: vec![
                ChatMessage::system(COMPACT_PROMPT.to_string()),
                ChatMessage::user(material),
            ],
            tools: vec![],
            max_tokens: config.max_tokens,
            thinking: config.thinking,
            session_id: config.session_id.clone(),
        };
        let mut reply = Accumulator::default();
        let mut stream = open_with_retry(provider, &request, cancel, config.retry).await?;
        let failure = loop {
            let event = tokio::select! {
                biased;
                _ = cancel.cancelled() => break Some(LlmError::Cancelled),
                event = stream.next() => event,
            };
            match event {
                Some(Ok(event)) => reply.push(&event),
                Some(Err(error)) => break Some(error),
                None => break None,
            }
        };
        if let Some(usage) = reply.usage {
            self.usage.add(usage);
            observer.usage(&config.model, usage).await;
        }
        if let Some(error) = failure {
            return Err(error);
        }
        let summary = reply.text.trim();
        // 没写完的摘要不能用：后面的对话全靠它。
        if summary.is_empty() || matches!(reply.stop, None | Some(StopReason::MaxTokens)) {
            return Err(LlmError::Protocol("模型没有给出完整的摘要".into()));
        }
        Ok(summary.to_string())
    }

    /// 调一次模型，边收边把文字报出去。返回收到的内容，以及中途失败的原因（如果有）。
    async fn call_model(
        &mut self,
        provider: &dyn Provider,
        request: &ChatRequest,
        config: &TurnConfig,
        observer: &dyn Observer,
        cancel: &CancellationToken,
    ) -> (Accumulator, Option<LlmError>) {
        let mut reply = Accumulator::default();
        let mut stream = match open_with_retry(provider, request, cancel, config.retry).await {
            Ok(stream) => stream,
            Err(error) => return (reply, Some(error)),
        };
        loop {
            let event = tokio::select! {
                // 取消优先：不等模型那边回应。
                biased;
                _ = cancel.cancelled() => {
                    charge_unreported(request, &mut reply);
                    return (reply, Some(LlmError::Cancelled));
                }
                event = stream.next() => event,
            };
            match event {
                Some(Ok(event)) => {
                    if let StreamEvent::TextDelta(text) = &event {
                        if reply.text.is_empty() && self.said_something {
                            observer.text_delta("\n\n").await;
                        }
                        self.said_something = true;
                        observer.text_delta(text).await;
                    }
                    reply.push(&event);
                }
                Some(Err(error)) => {
                    charge_unreported(request, &mut reply);
                    return (reply, Some(error));
                }
                None => break,
            }
        }
        charge_unreported(request, &mut reply);
        if reply.stop.is_none() {
            return (reply, Some(LlmError::Protocol("回复没有正常结束".into())));
        }
        (reply, None)
    }
}

/// 请求已经发出去了，但模型那边没来得及报用量（被停下、中途断了）：这次调用照样花了钱，
/// 按字数估一个数记上，不然花费的上限会被少算。
fn charge_unreported(request: &ChatRequest, reply: &mut Accumulator) {
    if reply.usage.is_some() {
        return;
    }
    let input =
        compact::estimate_messages(&request.messages) + compact::estimate_tools(&request.tools);
    let output = compact::estimate(&reply.text)
        + compact::estimate(&reply.reasoning)
        + reply
            .tool_calls
            .iter()
            .map(|call| compact::estimate(&call.name) + compact::estimate(&call.arguments))
            .sum::<usize>();
    reply.usage = Some(Usage {
        input_tokens: input as u64,
        output_tokens: output as u64,
        cached_tokens: 0,
    });
}

fn none_if_empty(text: String) -> Option<String> {
    (!text.is_empty()).then_some(text)
}

fn is_context_overflow(error: &LlmError) -> bool {
    let text = error.to_string().to_ascii_lowercase();
    text.contains("（413）")
        || text.contains("(413)")
        || text.contains("413")
        || text.contains("payload too large")
        || text.contains("request entity too large")
        || text.contains("context_length")
        || text.contains("maximum context")
        || text.contains("prompt is too long")
        || text.contains("too many tokens")
        || text.contains("context window")
        || text.contains("token limit")
}

fn batch_fingerprint(calls: &[ToolCall]) -> String {
    calls
        .iter()
        .map(|c| format!("{}:{}", c.name, c.arguments.trim()))
        .collect::<Vec<_>>()
        .join("|")
}

/// 执行一批工具调用，结果和 `calls` 一一对应。`None` 表示还没返回就被取消了。
///
/// 相邻的、可以并发的调用一起跑，其余的一个一个来，保持模型给出的先后顺序。
async fn run_tools(
    calls: &[ToolCall],
    tools: &ToolRegistry,
    ctx: &ToolCtx,
    spill_dir: Option<&std::path::Path>,
    hooks: Option<&HookRunner>,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> Vec<Option<Result<String, String>>> {
    let ctx = ToolCtx {
        cancel: cancel.clone(),
        ..ctx.clone()
    };
    let prepared: Vec<Prepared> = calls.iter().map(|call| prepare(call, tools)).collect();
    let mut results = Vec::with_capacity(calls.len());
    let mut start = 0;
    while start < prepared.len() {
        let mut end = start + 1;
        if prepared[start].parallel {
            while end < prepared.len() && prepared[end].parallel {
                end += 1;
            }
        }
        let batch = stream::iter(start..end)
            .map(|index| {
                run_one(
                    &calls[index],
                    &prepared[index],
                    &ctx,
                    spill_dir,
                    hooks,
                    observer,
                    cancel,
                )
            })
            .buffered(MAX_PARALLEL_TOOLS)
            .collect::<Vec<_>>()
            .await;
        results.extend(batch);
        start = end;
    }
    results
}

struct Prepared {
    title: String,
    parallel: bool,
    /// 能执行时是工具和解析好的参数；否则是要交还给模型的错误。
    runnable: Result<(std::sync::Arc<dyn sa_tools::Tool>, Value), String>,
}

fn prepare(call: &ToolCall, tools: &ToolRegistry) -> Prepared {
    let Some(tool) = tools.get(&call.name) else {
        let known: Vec<_> = tools.iter().map(|t| t.name()).collect();
        return Prepared {
            title: format!("调用 {}", call.name),
            parallel: true,
            runnable: Err(format!(
                "没有叫 {} 的工具。可用的工具：{}。",
                call.name,
                known.join("、")
            )),
        };
    };
    // 有的模型在没有参数时给空字符串。
    let raw = if call.arguments.trim().is_empty() {
        "{}"
    } else {
        &call.arguments
    };
    match serde_json::from_str::<Value>(raw) {
        Ok(input) => Prepared {
            title: tool.title(&input),
            parallel: tool.is_concurrency_safe(&input),
            runnable: Ok((tool.clone(), input)),
        },
        Err(e) => Prepared {
            title: format!("调用 {}", call.name),
            parallel: true,
            runnable: Err(format!(
                "参数不是合法的 JSON（{e}）。请重新调用，参数要是一个完整的 JSON 对象。"
            )),
        },
    }
}

async fn run_one(
    call: &ToolCall,
    prepared: &Prepared,
    ctx: &ToolCtx,
    spill_dir: Option<&std::path::Path>,
    hooks: Option<&HookRunner>,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> Option<Result<String, String>> {
    if cancel.is_cancelled() {
        return None;
    }
    observer
        .tool_started(&call.id, &call.name, &prepared.title, &call.arguments)
        .await;
    let result = match &prepared.runnable {
        Err(message) => Some(Err(message.clone())),
        Ok((tool, input)) => {
            let allowed = async {
                if let Some(hooks) = hooks {
                    if let HookOutcome::Block(reason) =
                        hooks.run_pre_tool(&call.name, input, cancel).await
                    {
                        return Err(format!("[PreToolUse 钩子拦截] {reason}"));
                    }
                }
                match tool.check_permission(input, ctx).await {
                    Permission::Allow => {}
                    Permission::Deny(reason) => return Err(reason),
                    Permission::Ask(reason) => {
                        let request = ApprovalRequest {
                            call_id: &call.id,
                            tool: &call.name,
                            title: &prepared.title,
                            reason: &reason,
                            input,
                        };
                        if !observer.request_approval(request).await {
                            return Err(DENIED_RESULT.to_string());
                        }
                        // 人批准的是卡片上写的那件事。等的时候情况变了（比如路径上的链接被改指向别处），
                        // 就不能拿这次批准去做另一件事。
                        if tool.check_permission(input, ctx).await != Permission::Ask(reason) {
                            return Err(CHANGED_RESULT.to_string());
                        }
                    }
                }
                Ok(())
            };
            let allowed = tokio::select! {
                biased;
                _ = cancel.cancelled() => None,
                allowed = allowed => Some(allowed),
            };
            match allowed {
                None => None,
                Some(Err(reason)) => Some(Err(reason)),
                Some(Ok(())) => {
                    let max = tool.max_result_chars();
                    let work = async {
                        let res = match tool.call(input.clone(), ctx).await {
                            Ok(text) => Ok(clip_and_spill(text, max, spill_dir, &call.id).await),
                            Err(e) => Err(clip_and_spill(e.0, max, spill_dir, &call.id).await),
                        };
                        if let Some(hooks) = hooks {
                            let (ok, out_ref) = match &res {
                                Ok(t) => (true, t.as_str()),
                                Err(t) => (false, t.as_str()),
                            };
                            if let Some(note) = hooks
                                .run_post_tool(&call.name, input, ok, out_ref, cancel)
                                .await
                            {
                                return match res {
                                    Ok(mut t) => {
                                        t.push_str(&format!("\n\n[PostToolUse 钩子反馈]\n{note}"));
                                        Ok(t)
                                    }
                                    Err(mut t) => {
                                        t.push_str(&format!("\n\n[PostToolUse 钩子反馈]\n{note}"));
                                        Err(t)
                                    }
                                };
                            }
                        }
                        res
                    };
                    if tool.finishes_once_started() {
                        // 做完了照实报；是因为停止才没做的，算停止。
                        let result = work.await;
                        (result.is_ok() || !cancel.is_cancelled()).then_some(result)
                    } else {
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => None,
                            result = work => Some(result),
                        }
                    }
                }
            }
        }
    };
    let (end, output) = match &result {
        Some(Ok(text)) => (StepEnd::Ok, text.as_str()),
        Some(Err(text)) => (StepEnd::Failed, text.as_str()),
        None => (StepEnd::Stopped, ""),
    };
    observer.tool_finished(&call.id, end, output).await;
    result
}

/// 超长的结果截短后交给模型，完整内容另存一份，并在结果末尾注明存在哪。
async fn clip_and_spill(
    text: String,
    max_chars: usize,
    spill_dir: Option<&std::path::Path>,
    call_id: &str,
) -> String {
    let clipped = clip(&text, max_chars);
    let Some(dir) = spill_dir.filter(|_| clipped.len() != text.len()) else {
        return clipped;
    };
    // 文件名里只留安全的字符；加上时间，同一个调用 id 出现两次也不会互相覆盖。
    let safe: String = call_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(40)
        .collect();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros());
    let name = format!("{stamp}-{safe}.txt");
    let path = dir.join(&name);
    let dir = dir.to_path_buf();
    let saved = tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&path, text)
    })
    .await;
    match saved {
        Ok(Ok(())) => {
            format!("{clipped}\n\n（完整结果已另存为 spill/{name}，用户可以在数据目录里查看。）")
        }
        _ => clipped,
    }
}

/// 超长的结果保留开头和结尾，中间换成一句说明。
fn clip(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    let head = max_chars * 4 / 5;
    let tail = max_chars - head;
    let head_text: String = text.chars().take(head).collect();
    let tail_text: String = text.chars().skip(total - tail).collect();
    format!(
        "{head_text}\n\n…（结果共 {total} 字，中间省略了 {} 字）…\n\n{tail_text}",
        total - max_chars
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sa_llm::{MockProvider, MockReply, Role};
    use sa_tools::{Fence, Tool, ToolError, ToolResult};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Default)]
    struct Recorder {
        log: Mutex<Vec<String>>,
        /// 收到第几段文字时触发取消。
        cancel_after_deltas: Option<(usize, CancellationToken)>,
        /// 有人问要不要批准时怎么答；`None` 表示一直不答。
        approve: Option<bool>,
    }

    impl Recorder {
        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }

        fn text(&self) -> String {
            self.log()
                .iter()
                .filter_map(|line| line.strip_prefix("text:"))
                .collect()
        }
    }

    #[async_trait]
    impl Observer for Recorder {
        async fn text_delta(&self, text: &str) {
            let mut log = self.log.lock().unwrap();
            log.push(format!("text:{text}"));
            if let Some((count, cancel)) = &self.cancel_after_deltas {
                if log.iter().filter(|l| l.starts_with("text:")).count() >= *count {
                    cancel.cancel();
                }
            }
        }
        async fn tool_started(&self, call_id: &str, tool: &str, title: &str, _arguments: &str) {
            self.log
                .lock()
                .unwrap()
                .push(format!("start:{call_id}:{tool}:{title}"));
        }
        async fn tool_finished(&self, call_id: &str, end: StepEnd, _output: &str) {
            let end = match end {
                StepEnd::Ok => "true",
                StepEnd::Failed => "false",
                StepEnd::Stopped => "stopped",
            };
            self.log
                .lock()
                .unwrap()
                .push(format!("finish:{call_id}:{end}"));
        }
        async fn usage(&self, model: &str, usage: Usage) {
            self.log
                .lock()
                .unwrap()
                .push(format!("usage:{model}:{}", usage.output_tokens));
        }
        async fn request_approval(&self, request: ApprovalRequest<'_>) -> bool {
            self.log.lock().unwrap().push(format!(
                "ask:{}:{}:{}:{}",
                request.call_id, request.tool, request.title, request.reason
            ));
            match self.approve {
                Some(answer) => answer,
                None => std::future::pending().await,
            }
        }
    }

    /// 每次 drain 给出队列里的下一批。
    struct Queue(Vec<Vec<UserInput>>);

    #[async_trait]
    impl Inbox for Queue {
        async fn drain(&mut self) -> Vec<UserInput> {
            if self.0.is_empty() {
                vec![]
            } else {
                self.0.remove(0)
            }
        }
    }

    fn says(text: &str, seq: i64) -> UserInput {
        UserInput {
            content: text.into(),
            message_id: Some(format!("msg_{seq}")),
            message_seq: Some(seq),
        }
    }

    /// 测试用工具：记录同时在跑的个数；`wait` 为真时一直等到被取消。
    struct Probe {
        name: &'static str,
        parallel: bool,
        running: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Tool for Probe {
        fn name(&self) -> &'static str {
            self.name
        }
        fn description(&self) -> String {
            "探针".into()
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn title(&self, input: &Value) -> String {
            format!("探测 {}", input["tag"].as_str().unwrap_or(""))
        }
        fn is_concurrency_safe(&self, _: &Value) -> bool {
            self.parallel
        }
        fn max_result_chars(&self) -> usize {
            50
        }
        fn finishes_once_started(&self) -> bool {
            self.name == "atomic"
        }
        async fn check_permission(&self, input: &Value, _: &ToolCtx) -> Permission {
            match input["permission"].as_str() {
                Some("ask") => Permission::Ask("探针要问一下".into()),
                // 每问一次，要做的事都不一样：模拟等批准的时候目标变了。
                Some("shifting") => {
                    static TARGET: AtomicUsize = AtomicUsize::new(0);
                    Permission::Ask(format!("目标 {}", TARGET.fetch_add(1, Ordering::SeqCst)))
                }
                Some("deny") => Permission::Deny("探针不让做".into()),
                _ => Permission::Allow,
            }
        }
        async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolResult {
            let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            if input["wait"] == true {
                ctx.cancel.cancelled().await;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            self.running.fetch_sub(1, Ordering::SeqCst);
            if input["fail"] == true {
                return Err(ToolError::new("探针失败"));
            }
            if input["long"] == true {
                return Ok("长".repeat(200));
            }
            Ok(format!("结果 {}", input["tag"].as_str().unwrap_or("")))
        }
    }

    struct Rig {
        _dir: tempfile::TempDir,
        transcript: Transcript,
        tools: ToolRegistry,
        ctx: ToolCtx,
        peak: Arc<AtomicUsize>,
        config: TurnConfig,
    }

    fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("说明.md"), "第一行\n第二行\n").unwrap();
        let transcript = Transcript::open(&dir.path().join("sessions/s.jsonl")).unwrap();
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut tools = ToolRegistry::read_only();
        for (name, parallel) in [("probe", true), ("serial", false), ("atomic", false)] {
            tools.register(Probe {
                name,
                parallel,
                running: running.clone(),
                peak: peak.clone(),
            });
        }
        let ctx = ToolCtx::new(Fence::new(dir.path(), &[dir.path().join("sessions")]).unwrap());
        Rig {
            _dir: dir,
            transcript,
            tools,
            ctx,
            peak,
            config: TurnConfig {
                model: "mock-model".into(),
                system_prompt: "你是测试助手".into(),
                max_steps: 8,
                max_tokens: None,
                thinking: None,
                session_id: Some("ses_test".into()),
                context_window: None,
                spill_dir: None,
                retry: RetryPolicy {
                    max_retries: 2,
                    base_delay: Duration::from_millis(1),
                },
                max_truncated_recoveries: 0,
                hooks: None,
            },
        }
    }

    impl Rig {
        async fn turn(
            &mut self,
            provider: &MockProvider,
            inputs: Vec<Vec<UserInput>>,
            observer: &Recorder,
            cancel: &CancellationToken,
        ) -> TurnOutcome {
            run_turn(
                &mut self.transcript,
                provider,
                &self.tools,
                &self.ctx,
                &self.config,
                &mut Queue(inputs),
                observer,
                cancel,
            )
            .await
        }

        fn kinds(&self) -> Vec<&'static str> {
            self.transcript
                .entries()
                .iter()
                .map(|e| match e.body {
                    EntryBody::User { .. } => "user",
                    EntryBody::Assistant { .. } => "assistant",
                    EntryBody::ToolResult { .. } => "tool",
                    EntryBody::Summary { .. } => "summary",
                })
                .collect()
        }

        fn tool_results(&self) -> Vec<(String, bool)> {
            self.transcript
                .entries()
                .iter()
                .filter_map(|e| match &e.body {
                    EntryBody::ToolResult {
                        content, is_error, ..
                    } => Some((content.clone(), *is_error)),
                    _ => None,
                })
                .collect()
        }
    }

    #[tokio::test]
    async fn plain_reply_streams_and_is_recorded() {
        let mut rig = rig();
        let provider = MockProvider::scripted([MockReply::text("你好呀")]);
        let observer = Recorder::default();
        let outcome = rig
            .turn(
                &provider,
                vec![vec![says("你好", 1)]],
                &observer,
                &CancellationToken::new(),
            )
            .await;

        assert_eq!(outcome.end, TurnEnd::Completed);
        assert_eq!(outcome.steps, 1);
        assert_eq!(outcome.usage.output_tokens, 3);
        assert_eq!(
            observer.log(),
            ["text:你", "text:好", "text:呀", "usage:mock-model:3"]
        );
        assert_eq!(rig.kinds(), ["user", "assistant"]);
        assert_eq!(rig.transcript.last_message_seq(), 1);

        let request = &provider.requests()[0];
        assert_eq!(request.model, "mock-model");
        assert_eq!(request.messages[0].role, Role::System);
        assert_eq!(request.messages[0].content, "你是测试助手");
        assert_eq!(request.messages[1].content, "你好");
        assert_eq!(request.tools.len(), 8);
        assert_eq!(request.session_id.as_deref(), Some("ses_test"));
    }

    #[tokio::test]
    async fn reads_a_file_then_answers_with_its_content_in_context() {
        let mut rig = rig();
        let provider = MockProvider::scripted([
            MockReply::tool_calls("我先读一下。", &[("c1", "read", r#"{"path":"说明.md"}"#)]),
            MockReply::text("里面有两行。"),
        ]);
        let observer = Recorder::default();
        let outcome = rig
            .turn(
                &provider,
                vec![vec![says("总结 说明.md", 1)]],
                &observer,
                &CancellationToken::new(),
            )
            .await;

        assert_eq!(outcome.end, TurnEnd::Completed);
        assert_eq!(outcome.steps, 2);
        assert_eq!(rig.kinds(), ["user", "assistant", "tool", "assistant"]);
        assert_eq!(observer.text(), "我先读一下。\n\n里面有两行。");
        let log = observer.log();
        assert!(
            log.contains(&"start:c1:read:读取 说明.md".to_string()),
            "{log:?}"
        );
        assert!(log.contains(&"finish:c1:true".to_string()));

        // 第二次调用时，模型能看到工具结果。
        let second = &provider.requests()[1];
        let last = second.messages.last().unwrap();
        assert_eq!(last.role, Role::Tool);
        assert_eq!(last.tool_call_id.as_deref(), Some("c1"));
        assert!(last.content.contains("第一行") && last.content.contains("第二行"));
        assert_eq!(second.messages[2].tool_calls[0].name, "read");
    }

    #[tokio::test]
    async fn tool_problems_go_back_to_the_model_instead_of_failing_the_turn() {
        let mut rig = rig();
        let provider = MockProvider::scripted([
            MockReply::tool_calls(
                "",
                &[
                    ("c1", "delete_everything", "{}"),
                    ("c2", "read", r#"{"path": "说明"#),
                    ("c3", "read", r#"{"path":"../外面.txt"}"#),
                    ("c4", "probe", r#"{"fail":true}"#),
                    ("c5", "probe", r#"{"long":true}"#),
                    ("c6", "probe", ""),
                ],
            ),
            MockReply::text("都处理了"),
        ]);
        let observer = Recorder::default();
        let outcome = rig
            .turn(
                &provider,
                vec![vec![says("试试", 1)]],
                &observer,
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(outcome.end, TurnEnd::Completed);

        let results = rig.tool_results();
        assert_eq!(results.len(), 6);
        assert!(results[0].0.contains("没有叫 delete_everything 的工具") && results[0].1);
        assert!(results[0].0.contains("read"));
        assert!(results[1].0.contains("不是合法的 JSON") && results[1].1);
        assert!(results[2].0.contains("工作目录之外") && results[2].1);
        assert_eq!(results[3], ("探针失败".to_string(), true));
        assert!(!results[4].1);
        assert!(results[4].0.contains("结果共 200 字") && results[4].0.chars().count() < 100);
        assert_eq!(results[5], ("结果 ".to_string(), false));
        assert!(observer.log().contains(&"finish:c1:false".to_string()));
    }

    #[tokio::test]
    async fn parallel_tools_overlap_and_serial_ones_do_not() {
        let mut rig = rig();
        let provider = MockProvider::scripted([
            MockReply::tool_calls(
                "",
                &[
                    ("p1", "probe", r#"{"tag":"a"}"#),
                    ("p2", "probe", r#"{"tag":"b"}"#),
                    ("p3", "probe", r#"{"tag":"c"}"#),
                ],
            ),
            MockReply::tool_calls(
                "",
                &[
                    ("s1", "serial", r#"{"tag":"d"}"#),
                    ("s2", "serial", r#"{"tag":"e"}"#),
                ],
            ),
            MockReply::text("好"),
        ]);
        let observer = Recorder::default();
        let cancel = CancellationToken::new();

        let outcome = rig
            .turn(&provider, vec![vec![says("并发", 1)]], &observer, &cancel)
            .await;
        assert_eq!(outcome.end, TurnEnd::Completed);
        assert_eq!(rig.peak.load(Ordering::SeqCst), 3);
        // 结果按调用顺序记录。
        let results: Vec<_> = rig.tool_results().into_iter().map(|r| r.0).collect();
        assert_eq!(results, ["结果 a", "结果 b", "结果 c", "结果 d", "结果 e"]);
        // 串行的两个：前一个结束之后后一个才开始。
        let log = observer.log();
        let at = |needle: &str| log.iter().position(|l| l.starts_with(needle)).unwrap();
        assert!(at("finish:s1") < at("start:s2"), "{log:?}");
    }

    #[tokio::test]
    async fn cancel_while_streaming_keeps_what_was_said() {
        let mut rig = rig();
        let provider = MockProvider::scripted([
            MockReply::Hang(vec![
                StreamEvent::TextDelta("写到".into()),
                StreamEvent::TextDelta("一半".into()),
                StreamEvent::ToolCallStart {
                    id: "c1".into(),
                    name: "read".into(),
                },
            ]),
            MockReply::text("接着说"),
        ]);
        let cancel = CancellationToken::new();
        let observer = Recorder {
            cancel_after_deltas: Some((2, cancel.clone())),
            ..Default::default()
        };
        let outcome = rig
            .turn(
                &provider,
                vec![vec![says("讲个故事", 1)]],
                &observer,
                &cancel,
            )
            .await;
        assert_eq!(outcome.end, TurnEnd::Cancelled);
        assert_eq!(rig.kinds(), ["user", "assistant"]);
        match &rig.transcript.entries()[1].body {
            EntryBody::Assistant {
                content,
                tool_calls,
                ..
            } => {
                assert_eq!(content, "写到一半");
                assert!(tool_calls.is_empty(), "没收完整的工具调用不能留在记录里");
            }
            other => panic!("{other:?}"),
        }

        // 取消之后的记录还能接着用。
        let outcome = rig
            .turn(
                &provider,
                vec![vec![says("继续", 2)]],
                &Recorder::default(),
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(outcome.end, TurnEnd::Completed);
        let roles: Vec<_> = provider.requests()[1]
            .messages
            .iter()
            .map(|m| m.role)
            .collect();
        assert_eq!(
            roles,
            [Role::System, Role::User, Role::Assistant, Role::User]
        );
    }

    #[tokio::test]
    async fn cancel_while_a_tool_runs_closes_every_call() {
        let mut rig = rig();
        let provider = MockProvider::scripted([MockReply::tool_calls(
            "",
            &[
                ("c1", "serial", r#"{"tag":"快"}"#),
                ("c2", "serial", r#"{"wait":true}"#),
                ("c3", "serial", r#"{"tag":"轮不到"}"#),
            ],
        )]);
        let cancel = CancellationToken::new();
        let observer = Recorder::default();
        let stopper = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            stopper.cancel();
        });
        let outcome = rig
            .turn(&provider, vec![vec![says("跑三个", 1)]], &observer, &cancel)
            .await;

        assert_eq!(outcome.end, TurnEnd::Cancelled);
        let results = rig.tool_results();
        assert_eq!(results[0], ("结果 快".to_string(), false));
        assert_eq!(results[1], (CANCELLED_RESULT.to_string(), true));
        assert_eq!(results[2], (CANCELLED_RESULT.to_string(), true));
        // 记录里没有悬空的调用：重新打开不需要再补。
        assert_eq!(rig.transcript.close_open_tool_calls("x").unwrap(), 0);
        let log = observer.log();
        assert!(log.contains(&"finish:c2:stopped".to_string()));
        assert!(!log.iter().any(|l| l.starts_with("start:c3")), "{log:?}");
    }

    /// 开始了就要做完的工具（比如写文件）：中途点停止，等它做完，照实报做成了，不说「已停止」。
    #[tokio::test]
    async fn cancel_while_an_atomic_tool_runs_lets_it_finish() {
        let mut rig = rig();
        let provider = MockProvider::scripted([MockReply::tool_calls(
            "",
            &[("c1", "atomic", r#"{"wait":true,"tag":"写完"}"#)],
        )]);
        let cancel = CancellationToken::new();
        let observer = Recorder::default();
        let stopper = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            stopper.cancel();
        });
        // 写的时候人又问了一句：停下了，这句留给下一轮，不读掉。
        let outcome = rig
            .turn(
                &provider,
                vec![vec![says("写", 1)], vec![says("又问", 2)]],
                &observer,
                &cancel,
            )
            .await;

        assert_eq!(outcome.end, TurnEnd::Cancelled);
        assert_eq!(rig.tool_results()[0], ("结果 写完".to_string(), false));
        assert_eq!(
            rig.kinds().iter().filter(|kind| **kind == "user").count(),
            1
        );
        assert!(
            observer.log().contains(&"finish:c1:true".to_string()),
            "{:?}",
            observer.log()
        );
    }

    /// 还没开始就停了：叫它起来的那几句照样记下，后面的留在收件箱里。
    #[tokio::test]
    async fn cancel_before_the_turn_starts_still_keeps_what_woke_it() {
        let mut rig = rig();
        let provider = MockProvider::scripted([MockReply::text("好")]);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let outcome = rig
            .turn(
                &provider,
                vec![vec![says("写", 1)], vec![says("又问", 2)]],
                &Recorder::default(),
                &cancel,
            )
            .await;

        assert_eq!(outcome.end, TurnEnd::Cancelled);
        assert_eq!(rig.kinds(), ["user"]);
    }

    /// 要问人的调用：批准了才执行；不批准或者工具自己不让做，都不执行，并把原因告诉模型。
    #[tokio::test]
    async fn approval_gates_execution() {
        let calls = [
            ("c1", "serial", r#"{"tag":"问","permission":"ask"}"#),
            ("c2", "serial", r#"{"tag":"禁","permission":"deny"}"#),
            ("c3", "serial", r#"{"tag":"直接"}"#),
        ];
        for (approve, first) in [
            (true, ("结果 问".to_string(), false)),
            (false, (DENIED_RESULT.to_string(), true)),
        ] {
            let mut rig = rig();
            let provider =
                MockProvider::scripted([MockReply::tool_calls("", &calls), MockReply::text("好")]);
            let observer = Recorder {
                approve: Some(approve),
                ..Default::default()
            };
            let outcome = rig
                .turn(
                    &provider,
                    vec![vec![says("跑", 1)]],
                    &observer,
                    &CancellationToken::new(),
                )
                .await;

            assert_eq!(outcome.end, TurnEnd::Completed);
            assert_eq!(
                rig.tool_results(),
                [
                    first,
                    ("探针不让做".to_string(), true),
                    ("结果 直接".to_string(), false),
                ]
            );
            let log = observer.log();
            let asks: Vec<_> = log.iter().filter(|l| l.starts_with("ask:")).collect();
            assert_eq!(asks, ["ask:c1:serial:探测 问:探针要问一下"]);
            assert!(log.contains(&format!("finish:c1:{approve}")), "{log:?}");
            // 没批准的那次，工具一次都没跑：只有 c3 跑过。
            assert_eq!(rig.peak.load(Ordering::SeqCst), 1);
        }
    }

    /// 批准之后再看一眼：要做的事和卡片上写的不一样了，就不执行。
    #[tokio::test]
    async fn approval_does_not_carry_over_to_a_changed_target() {
        let mut rig = rig();
        let provider = MockProvider::scripted([
            MockReply::tool_calls("", &[("c1", "serial", r#"{"permission":"shifting"}"#)]),
            MockReply::text("好"),
        ]);
        let observer = Recorder {
            approve: Some(true),
            ..Default::default()
        };
        let outcome = rig
            .turn(
                &provider,
                vec![vec![says("跑", 1)]],
                &observer,
                &CancellationToken::new(),
            )
            .await;

        assert_eq!(outcome.end, TurnEnd::Completed);
        assert_eq!(rig.tool_results(), [(CHANGED_RESULT.to_string(), true)]);
        assert_eq!(rig.peak.load(Ordering::SeqCst), 0, "工具不应该跑");
        assert!(observer.log().contains(&"finish:c1:false".to_string()));
    }

    /// 结果太长：交给模型的是截短的，完整内容另存一份。
    #[tokio::test]
    async fn long_results_are_clipped_and_the_full_text_is_spilled() {
        let mut rig = rig();
        let spill = rig._dir.path().join("spill");
        rig.config.spill_dir = Some(spill.clone());
        let provider = MockProvider::scripted([
            MockReply::tool_calls(
                "",
                &[
                    ("c/../1", "serial", r#"{"long":true}"#),
                    ("c2", "serial", r#"{"tag":"短"}"#),
                ],
            ),
            MockReply::text("好"),
        ]);
        let outcome = rig
            .turn(
                &provider,
                vec![vec![says("跑", 1)]],
                &Recorder::default(),
                &CancellationToken::new(),
            )
            .await;

        assert_eq!(outcome.end, TurnEnd::Completed);
        let results = rig.tool_results();
        assert!(
            results[0].0.contains("中间省略了 150 字"),
            "{}",
            results[0].0
        );
        assert!(
            results[0].0.contains("完整结果已另存为 spill/"),
            "{}",
            results[0].0
        );
        assert_eq!(results[1].0, "结果 短");
        // 只有被截断的那个结果存了文件；文件名里没有路径分隔符，内容是完整的。
        let files: Vec<_> = std::fs::read_dir(&spill)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1, "{files:?}");
        assert!(files[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("-c1.txt"));
        assert_eq!(
            std::fs::read_to_string(&files[0]).unwrap(),
            "长".repeat(200)
        );
    }

    /// 往记录里放 `count` 组问答，每条 1000 个汉字（约 1000 token）。
    fn fill(rig: &mut Rig, count: usize) {
        for index in 0..count {
            rig.transcript
                .append(EntryBody::User {
                    content: format!("问{index}{}", "问".repeat(1_000)),
                    message_id: None,
                    message_seq: Some(index as i64 + 1),
                })
                .unwrap();
            rig.transcript
                .append(EntryBody::Assistant {
                    content: format!("答{index}{}", "答".repeat(1_000)),
                    reasoning: None,
                    tool_calls: vec![],
                })
                .unwrap();
        }
    }

    fn compact_steps(observer: &Recorder) -> Vec<String> {
        observer
            .log()
            .into_iter()
            .filter(|line| line.contains("compact-"))
            .map(|line| match line.split(':').collect::<Vec<_>>()[..] {
                ["start", _, tool, title] => format!("start:{tool}:{title}"),
                ["finish", _, ok] => format!("finish:{ok}"),
                _ => line.clone(),
            })
            .collect()
    }

    /// 对话超过压缩线：先让模型写摘要，再带着摘要和最近几条接着聊。
    #[tokio::test]
    async fn long_conversations_are_compacted_and_carry_on() {
        let mut rig = rig();
        // 窗口 64,000：压缩线 31,000，硬上限 44,000。记录里约 40,000。
        rig.config.context_window = Some(64_000);
        fill(&mut rig, 20);
        let provider = MockProvider::scripted([
            MockReply::text("  用户问了二十个问题。  "),
            MockReply::text("接着说"),
            MockReply::text("第二轮"),
        ]);
        let observer = Recorder::default();
        let outcome = rig
            .turn(
                &provider,
                vec![vec![says("最新的问题", 21)]],
                &observer,
                &CancellationToken::new(),
            )
            .await;

        assert_eq!(outcome.end, TurnEnd::Completed);
        // 写摘要不算一步，但用量算进去；摘要的文字不报给人看。
        assert_eq!(outcome.steps, 1);
        assert_eq!(outcome.usage.input_tokens, 20);
        assert_eq!(observer.text(), "接着说");
        assert_eq!(
            compact_steps(&observer),
            ["start:compact:对话太长，压缩前面的内容", "finish:true"]
        );

        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        // 摘要请求：专用提示词、不带工具、材料从最早的开始、不含留下的最后几条。
        let summarise = &requests[0];
        assert!(summarise.tools.is_empty());
        assert_eq!(summarise.messages.len(), 2);
        assert_eq!(summarise.messages[0].content, COMPACT_PROMPT);
        let material = &summarise.messages[1].content;
        assert!(
            material.starts_with("【对话记录】\n用户：问0"),
            "{}",
            &material[..80]
        );
        assert!(!material.contains("最新的问题") && !material.contains("答19"));
        // 之后的请求：系统提示词、摘要、最近几条原文，最后是刚发的那句。
        let next = &requests[1].messages;
        assert_eq!(next[0].role, Role::System);
        assert_eq!(
            next[1].content,
            format!("{}用户问了二十个问题。", crate::transcript::SUMMARY_PREFIX)
        );
        assert_eq!(next.last().unwrap().content, "最新的问题");
        assert!(next[next.len() - 2].content.starts_with("答19"));
        let limits = Limits::new(64_000, None);
        let size = compact::estimate_messages(next);
        assert!(size < limits.threshold / 2, "压缩后还有 {size}");
        assert!(next.len() > 3, "最近的几条应该原样留下");
        // 留下的部分不会从一条没有调用的工具结果开始。
        assert_ne!(next[2].role, Role::Tool);

        // 原始记录一条不少，只是多了一个边界；重新打开后发给模型的内容不变。
        assert_eq!(rig.kinds().len(), 43);
        assert_eq!(rig.kinds()[41..], ["summary", "assistant"]);
        assert_eq!(rig.transcript.last_message_seq(), 21);
        let reopened = Transcript::open(rig.transcript.path()).unwrap();
        assert_eq!(
            reopened.to_chat_messages(),
            rig.transcript.to_chat_messages()
        );

        // 压缩之后没到线：下一轮不再压缩。
        let outcome = rig
            .turn(
                &provider,
                vec![vec![says("再问一句", 22)]],
                &observer,
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(outcome.end, TurnEnd::Completed);
        assert_eq!(provider.requests().len(), 3);
        assert_eq!(rig.kinds().iter().filter(|k| **k == "summary").count(), 1);
    }

    /// 压缩失败但还放得下：照常继续；连续失败三次之后不再试。
    #[tokio::test]
    async fn failed_compaction_does_not_stop_the_turn_and_gives_up_after_three_tries() {
        let mut rig = rig();
        rig.config.context_window = Some(64_000);
        // 约 34,000：过了压缩线，没到硬上限。
        fill(&mut rig, 17);
        let bad = || MockReply::Fail(LlmError::Auth("坏密钥".into()));
        let call = |id| MockReply::tool_calls("", &[(id, "serial", r#"{"tag":"x"}"#)]);
        let provider = MockProvider::scripted([
            bad(),
            call("c1"),
            // 没写完的摘要不能用。
            MockReply::Events(vec![
                StreamEvent::TextDelta("写到一半".into()),
                StreamEvent::Done {
                    stop: StopReason::MaxTokens,
                },
            ]),
            call("c2"),
            MockReply::text(""),
            call("c3"),
            // 第四步不再试压缩，直接是正常回复。
            MockReply::text("做完了"),
        ]);
        let observer = Recorder::default();
        let outcome = rig
            .turn(
                &provider,
                vec![vec![says("开始", 18)]],
                &observer,
                &CancellationToken::new(),
            )
            .await;

        assert_eq!(outcome.end, TurnEnd::Completed);
        assert_eq!(outcome.steps, 4);
        assert_eq!(provider.requests().len(), 7);
        assert_eq!(
            compact_steps(&observer)
                .iter()
                .filter(|l| *l == "finish:false")
                .count(),
            3
        );
        assert!(!rig.kinds().contains(&"summary"));
        assert_eq!(observer.text(), "做完了");
    }

    /// 压缩不了，又真的放不下：不调模型，直接说明原因。
    #[tokio::test]
    async fn context_limit_ends_the_turn_when_compaction_cannot_help() {
        let mut rig = rig();
        rig.config.context_window = Some(64_000);
        // 约 50,000：过了硬上限。
        fill(&mut rig, 25);
        let provider =
            MockProvider::scripted([MockReply::Fail(LlmError::BadRequest("太长".into()))]);
        let observer = Recorder::default();
        let outcome = rig
            .turn(
                &provider,
                vec![vec![says("开始", 26)]],
                &observer,
                &CancellationToken::new(),
            )
            .await;

        assert_eq!(outcome.end, TurnEnd::ContextLimit);
        assert_eq!(outcome.steps, 0);
        assert_eq!(provider.requests().len(), 1);
        // 人说的话已经记下来了，下次不会丢。
        assert_eq!(rig.transcript.last_message_seq(), 26);
        assert_eq!(observer.text(), "");
    }

    /// 写摘要时点了停止：马上停，不留半截摘要。
    #[tokio::test]
    async fn stop_during_compaction_cancels_the_turn() {
        let mut rig = rig();
        rig.config.context_window = Some(64_000);
        fill(&mut rig, 20);
        let provider = MockProvider::scripted([MockReply::Hang(vec![StreamEvent::TextDelta(
            "摘要写到".into(),
        )])]);
        let observer = Recorder::default();
        let cancel = CancellationToken::new();
        let stop = async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            cancel.cancel();
        };
        let (outcome, ()) = tokio::join!(
            rig.turn(&provider, vec![vec![says("开始", 21)]], &observer, &cancel),
            stop
        );

        assert_eq!(outcome.end, TurnEnd::Cancelled);
        assert_eq!(outcome.steps, 0);
        assert!(!rig.kinds().contains(&"summary"));
        assert_eq!(
            compact_steps(&observer),
            ["start:compact:对话太长，压缩前面的内容", "finish:stopped"]
        );
    }

    /// 没有配置窗口大小：不压缩。
    #[tokio::test]
    async fn no_window_means_no_compaction() {
        let mut rig = rig();
        fill(&mut rig, 30);
        let provider = MockProvider::scripted([MockReply::text("好")]);
        let outcome = rig
            .turn(
                &provider,
                vec![vec![says("开始", 31)]],
                &Recorder::default(),
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(outcome.end, TurnEnd::Completed);
        assert_eq!(provider.requests().len(), 1);
        assert_eq!(provider.requests()[0].messages.len(), 62);
    }

    /// 等人批准时点了停止：不再等，调用按取消收尾。
    #[tokio::test]
    async fn cancel_while_waiting_for_approval() {
        let mut rig = rig();
        let provider = MockProvider::scripted([MockReply::tool_calls(
            "",
            &[("c1", "serial", r#"{"permission":"ask"}"#)],
        )]);
        let cancel = CancellationToken::new();
        let observer = Recorder::default();
        let stopper = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            stopper.cancel();
        });
        let outcome = rig
            .turn(&provider, vec![vec![says("跑", 1)]], &observer, &cancel)
            .await;

        assert_eq!(outcome.end, TurnEnd::Cancelled);
        assert_eq!(rig.tool_results(), [(CANCELLED_RESULT.to_string(), true)]);
        assert_eq!(rig.peak.load(Ordering::SeqCst), 0);
        assert!(observer.log().contains(&"finish:c1:stopped".to_string()));
    }

    #[tokio::test]
    async fn stops_after_max_steps() {
        let mut rig = rig();
        rig.config.max_steps = 3;
        let again = || MockReply::tool_calls("", &[("c", "probe", "{}")]);
        // 每次调用的 id 要不同，这里偷懒用同一个不影响计数。
        let provider = MockProvider::scripted([again(), again(), again(), again()]);
        let outcome = rig
            .turn(
                &provider,
                vec![vec![says("一直跑", 1)]],
                &Recorder::default(),
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(outcome.end, TurnEnd::MaxSteps);
        assert_eq!(outcome.steps, 3);
        assert_eq!(provider.remaining(), 1);
        // 停下来时最后一条是工具结果，记录是完整的。
        assert_eq!(rig.kinds().last(), Some(&"tool"));
    }

    #[tokio::test]
    async fn retries_before_output_but_fails_after_partial_output() {
        let mut rig = rig();
        let provider = MockProvider::scripted([
            MockReply::Fail(LlmError::RateLimited("慢点".into())),
            MockReply::Fail(LlmError::Server("炸了".into())),
            MockReply::text("第三次成功"),
            MockReply::FailAfter(
                vec![StreamEvent::TextDelta("说到一".into())],
                LlmError::Network("断了".into()),
            ),
            MockReply::Fail(LlmError::Auth("密钥不对".into())),
            MockReply::Events(vec![StreamEvent::TextDelta("没有结尾".into())]),
        ]);
        let cancel = CancellationToken::new();
        let observer = Recorder::default();

        let ok = rig
            .turn(&provider, vec![vec![says("一", 1)]], &observer, &cancel)
            .await;
        assert_eq!(ok.end, TurnEnd::Completed);
        assert_eq!(ok.steps, 1);
        assert_eq!(provider.requests().len(), 3);

        let dropped = rig
            .turn(&provider, vec![vec![says("二", 2)]], &observer, &cancel)
            .await;
        assert!(
            matches!(&dropped.end, TurnEnd::Failed(m) if m.contains("断了")),
            "{dropped:?}"
        );
        assert_eq!(provider.requests().len(), 4, "已经有输出之后不能重试");
        assert_eq!(rig.kinds(), ["user", "assistant", "user", "assistant"]);

        let denied = rig
            .turn(&provider, vec![vec![says("三", 3)]], &observer, &cancel)
            .await;
        assert!(matches!(&denied.end, TurnEnd::Failed(m) if m.contains("密钥不对")));
        assert_eq!(provider.requests().len(), 5, "凭证错误不重试");
        // 失败的那一轮，人的输入仍然留在记录里。
        assert_eq!(rig.transcript.last_message_seq(), 3);

        let unfinished = rig
            .turn(&provider, vec![vec![says("四", 4)]], &observer, &cancel)
            .await;
        assert!(matches!(&unfinished.end, TurnEnd::Failed(m) if m.contains("没有正常结束")));
    }

    #[tokio::test]
    async fn reply_cut_off_at_the_output_limit_is_not_reported_as_complete() {
        let mut rig = rig();
        let cut = |text: &str| {
            MockReply::Events(vec![
                StreamEvent::TextDelta(text.into()),
                StreamEvent::Done {
                    stop: StopReason::MaxTokens,
                },
            ])
        };
        let provider = MockProvider::scripted([cut("写到这里就没"), MockReply::text("接着写完了")]);
        let cancel = CancellationToken::new();
        let outcome = rig
            .turn(
                &provider,
                vec![vec![says("写一篇长文", 1)]],
                &Recorder::default(),
                &cancel,
            )
            .await;
        assert_eq!(outcome.end, TurnEnd::Truncated);
        // 写出来的那部分留在记录里，下一轮模型能接着写。
        assert_eq!(rig.kinds(), ["user", "assistant"]);
        let outcome = rig
            .turn(
                &provider,
                vec![vec![says("继续", 2)]],
                &Recorder::default(),
                &cancel,
            )
            .await;
        assert_eq!(outcome.end, TurnEnd::Completed);
        assert_eq!(provider.requests()[1].messages[2].content, "写到这里就没");
    }

    #[tokio::test]
    async fn interjections_are_picked_up_between_model_calls() {
        let mut rig = rig();
        let provider = MockProvider::scripted([
            MockReply::tool_calls("", &[("c1", "probe", r#"{"tag":"x"}"#)]),
            MockReply::text("收到补充"),
        ]);
        let outcome = rig
            .turn(
                &provider,
                vec![
                    vec![says("开始", 1), says("还有一句", 2)],
                    vec![says("等等，改成这样", 3)],
                ],
                &Recorder::default(),
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(outcome.end, TurnEnd::Completed);
        assert_eq!(
            rig.kinds(),
            ["user", "user", "assistant", "tool", "user", "assistant"]
        );
        let second = &provider.requests()[1];
        assert_eq!(second.messages.last().unwrap().content, "等等，改成这样");
        assert_eq!(rig.transcript.last_message_seq(), 3);
    }

    #[tokio::test]
    async fn reopened_transcript_continues_the_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut rig = rig();
        rig.transcript = Transcript::open(&path).unwrap();
        let provider = MockProvider::scripted([
            MockReply::Events(vec![
                StreamEvent::ReasoningDelta("用户在打招呼".into()),
                StreamEvent::TextDelta("你好".into()),
                StreamEvent::Done {
                    stop: StopReason::EndTurn,
                },
            ]),
            MockReply::text("记得，你说了你好"),
        ]);
        let cancel = CancellationToken::new();
        rig.turn(
            &provider,
            vec![vec![says("你好", 1)]],
            &Recorder::default(),
            &cancel,
        )
        .await;

        // 模拟重启：丢掉内存里的记录，从文件重新打开。
        rig.transcript = Transcript::open(&path).unwrap();
        let outcome = rig
            .turn(
                &provider,
                vec![vec![says("还记得吗", 2)]],
                &Recorder::default(),
                &cancel,
            )
            .await;
        assert_eq!(outcome.end, TurnEnd::Completed);
        let request = &provider.requests()[1];
        let contents: Vec<_> = request
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(contents, ["你是测试助手", "你好", "你好", "还记得吗"]);
        // 思考内容要原样带回去（DeepSeek 在带工具的请求里要求这样）。
        assert_eq!(
            request.messages[2].reasoning.as_deref(),
            Some("用户在打招呼")
        );
    }

    #[test]
    fn clip_keeps_head_and_tail() {
        assert_eq!(clip("短", 10), "短");
        let text: String = ('a'..='z').collect();
        let clipped = clip(&text, 10);
        assert!(clipped.starts_with("abcdefgh\n"));
        assert!(clipped.ends_with("\nyz"));
        assert!(clipped.contains("共 26 字") && clipped.contains("省略了 16 字"));
    }

    #[tokio::test]
    async fn auto_continues_on_max_tokens_when_enabled() {
        let mut rig = rig();
        rig.config.max_truncated_recoveries = 2;
        let provider = MockProvider::scripted([
            MockReply::Events(vec![
                StreamEvent::TextDelta("前半段".into()),
                StreamEvent::Done {
                    stop: StopReason::MaxTokens,
                },
            ]),
            MockReply::text("后半段"),
        ]);
        let observer = Recorder::default();
        let outcome = rig
            .turn(
                &provider,
                vec![vec![says("写长一点", 1)]],
                &observer,
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(outcome.end, TurnEnd::Completed);
        assert_eq!(outcome.steps, 2);
        assert_eq!(provider.requests().len(), 2);
    }

    #[tokio::test]
    async fn loop_guard_nudges_after_three_identical_tool_batches() {
        let mut rig = rig();
        let provider = MockProvider::scripted([
            MockReply::tool_calls("", &[("c1", "probe", r#"{"tag":"same"}"#)]),
            MockReply::tool_calls("", &[("c2", "probe", r#"{"tag":"same"}"#)]),
            MockReply::tool_calls("", &[("c3", "probe", r#"{"tag":"same"}"#)]),
            MockReply::text("我换个方法"),
        ]);
        let outcome = rig
            .turn(
                &provider,
                vec![vec![says("查一下", 1)]],
                &Recorder::default(),
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(outcome.end, TurnEnd::Completed);
        let last_req = provider.requests().last().unwrap().clone();
        let last_tool = last_req
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Tool)
            .unwrap();
        assert!(
            last_tool.content.contains("连续 3 次"),
            "{}",
            last_tool.content
        );
    }
}
