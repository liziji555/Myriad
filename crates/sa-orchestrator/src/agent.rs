//! 子代理：工作会话把一件「去查清楚」的事交出去，换一个干净的上下文来做，只拿回结论。
//!
//! 子代理只有只读工具，没有 `agent`（不能再往下派），也不能交任务结果。

use crate::Shared;
use async_trait::async_trait;
use sa_kernel::{
    run_turn, EntryBody, Inbox, Observer, StepEnd, Transcript, TurnConfig, TurnEnd, UserInput,
};
use sa_llm::{RetryPolicy, Usage};
use sa_protocol::{BotId, ModelTier, RoomId, SessionId, TaskId};
use sa_tools::{Tool, ToolCtx, ToolError, ToolRegistry, ToolResult};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

/// 子代理最多调这么多次模型。
const MAX_STEPS: usize = 12;
/// 交给子代理的话最多这么长。
const MAX_PROMPT_CHARS: usize = 4_000;
const PROMPT: &str = include_str!("../prompts/agent.md");

pub(crate) struct AgentTool {
    pub shared: Arc<Shared>,
    pub room_id: RoomId,
    pub bot_id: BotId,
    /// 用量记在哪个会话名下（派它的那个工作会话或对话会话）。
    pub session_id: SessionId,
    /// 花费算在哪个任务头上；在房间对话里派子代理时为 `None`。
    pub task_id: Option<TaskId>,
    /// 子代理每有一点动静就记下时间：它在查资料时，派它的任务不算卡住。
    pub pulse: Option<Arc<AtomicI64>>,
}

static NEXT: AtomicU64 = AtomicU64::new(0);

#[async_trait]
impl Tool for AgentTool {
    fn name(&self) -> &'static str {
        "agent"
    }

    fn description(&self) -> String {
        "派一个子代理去查一件事，只拿回它的结论。适合要翻很多文件才能回答的问题\
         （「这个函数在哪些地方被调用」「登录流程经过哪些文件」），这样翻找的过程不占你的上下文。\
         子代理只能读（read、list_dir、glob、grep、web_fetch），不能改文件、不能执行命令，\
         看不到你之前的对话。prompt 里把要查什么、要它带回什么写完整。几件互不相干的事可以同时派几个。"
            .into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string", "description": "要查什么、要带回什么。写完整，它看不到别的上下文。" }
            },
            "required": ["prompt"]
        })
    }

    fn title(&self, input: &Value) -> String {
        let prompt = input["prompt"].as_str().unwrap_or_default();
        let mut chars = prompt.chars();
        let mut brief: String = chars.by_ref().take(40).collect();
        if chars.next().is_some() {
            brief.push('…');
        }
        format!("子代理：{}", brief.replace('\n', " "))
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolResult {
        let prompt = input["prompt"].as_str().unwrap_or_default().trim();
        if prompt.is_empty() {
            return Err(ToolError::new(
                "prompt 是空的。写清楚要子代理查什么、带回什么。",
            ));
        }
        if prompt.chars().count() > MAX_PROMPT_CHARS {
            return Err(ToolError(format!(
                "prompt 太长了（最多 {MAX_PROMPT_CHARS} 个字）。只写要查什么，资料让它自己去读。"
            )));
        }
        let shared = &self.shared;
        let model = shared
            .models
            .resolve(ModelTier::Mid)
            .or_else(|_| shared.models.resolve(ModelTier::Strong))
            .map_err(ToolError)?;

        // 记录只在这一次调用里用，用完就删。
        let dir = shared.settings.sessions_dir.join("sub");
        std::fs::create_dir_all(&dir).map_err(|e| ToolError(format!("子代理的记录建不了：{e}")))?;
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("{}-{serial}.jsonl", self.session_id));
        let _ = std::fs::remove_file(&path);
        let _cleanup = RemoveOnDrop(path.clone());
        let mut transcript =
            Transcript::open(&path).map_err(|e| ToolError(format!("子代理的记录打不开：{e}")))?;

        let config = TurnConfig {
            model: model.model.clone(),
            system_prompt: PROMPT
                .replace("{workspace}", &ctx.fence.root_display())
                .trim_end()
                .to_string()
                + "\n",
            max_steps: MAX_STEPS,
            max_tokens: None,
            thinking: model.thinking,
            session_id: Some(self.session_id.to_string()),
            context_window: Some(model.context_window.unwrap_or_else(|| {
                model.provider.capabilities(&model.model).context_window as usize
            })),
            spill_dir: None,
            retry: RetryPolicy::default(),
            max_truncated_recoveries: 0,
            hooks: None,
        };
        let mut sub_ctx = ToolCtx::new(ctx.fence.clone());
        sub_ctx.cancel = ctx.cancel.clone();
        sub_ctx.allow_private_network = ctx.allow_private_network;
        let mut inbox = Once(Some(prompt.to_string()));
        let observer = Quiet(self);
        let provider = crate::budget::guard(shared, model.provider.clone(), self.task_id.clone());
        let outcome = run_turn(
            &mut transcript,
            provider.as_ref(),
            &ToolRegistry::read_only(),
            &sub_ctx,
            &config,
            &mut inbox,
            &observer,
            &ctx.cancel,
        )
        .await;

        let said = transcript
            .entries()
            .iter()
            .rev()
            .find_map(|entry| match &entry.body {
                EntryBody::Assistant { content, .. } if !content.trim().is_empty() => {
                    Some(content.trim().to_string())
                }
                _ => None,
            });
        match (outcome.end, said) {
            (TurnEnd::Completed, Some(said)) => Ok(said),
            (TurnEnd::Completed, None) => Err(ToolError::new("子代理什么都没说就结束了。")),
            (TurnEnd::Cancelled, _) => Err(ToolError::new("子代理被停下了。")),
            (TurnEnd::MaxSteps, Some(said)) => Ok(format!(
                "（子代理调了 {MAX_STEPS} 次模型还没查完，下面是它最后说的话，可能不完整。）\n\n{said}"
            )),
            (TurnEnd::Failed(reason), _) => Err(ToolError(format!("子代理出错了：{reason}"))),
            (end, _) => Err(ToolError(format!(
                "子代理没有查完（{}）。把问题拆小一点再派。",
                match end {
                    TurnEnd::Truncated => "回复太长被截断",
                    TurnEnd::ContextLimit => "读的东西太多，放不进上下文",
                    _ => "调用次数到了上限",
                }
            ))),
        }
    }
}

struct RemoveOnDrop(std::path::PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl Quiet<'_> {
    fn beat(&self) {
        if let Some(pulse) = &self.0.pulse {
            pulse.store(sa_engine::now_ms(), Ordering::Relaxed);
        }
    }
}

/// 只有一条输入。
struct Once(Option<String>);

#[async_trait]
impl Inbox for Once {
    async fn drain(&mut self) -> Vec<UserInput> {
        self.0
            .take()
            .map(|content| UserInput {
                content,
                message_id: None,
                message_seq: None,
            })
            .into_iter()
            .collect()
    }
}

/// 子代理的过程不往房间里写，只记用量。它要不到任何批准。
struct Quiet<'a>(&'a AgentTool);

#[async_trait]
impl Observer for Quiet<'_> {
    async fn text_delta(&self, _text: &str) {
        self.beat();
    }
    async fn tool_started(&self, _call_id: &str, _tool: &str, _title: &str, _arguments: &str) {
        self.beat();
    }
    async fn tool_finished(&self, _call_id: &str, _end: StepEnd, _output: &str) {
        self.beat();
    }

    async fn usage(&self, model: &str, usage: Usage) {
        self.beat();
        let tool = self.0;
        crate::budget::record(
            &tool.shared,
            &tool.room_id,
            Some(&tool.bot_id),
            Some(&tool.session_id),
            model,
            usage,
        )
        .await;
    }
}
