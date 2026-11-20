//! 一个房间的后台任务：有新消息就跑一轮，跑完再看有没有新的。

use crate::{render_prompt, Shared};
use async_trait::async_trait;
use sa_engine::{EngineError, UsageRecord};
use sa_kernel::{run_turn, Inbox, Observer, Transcript, TurnConfig, TurnEnd, UserInput};
use sa_llm::{RetryPolicy, Usage};
use sa_protocol::{Bot, MessageId, MessageState, RoomId, SessionId, StepState, ToolStep};
use sa_tools::ToolCtx;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// 在阻塞线程上做数据库操作。
pub(crate) async fn db<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, EngineError> + Send + 'static,
) -> Result<T, EngineError> {
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|e| Err(EngineError::Invalid(format!("后台任务出错：{e}"))))
}

pub(crate) async fn run(
    shared: Arc<Shared>,
    room_id: RoomId,
    wake: Arc<Notify>,
    current: Arc<Mutex<Option<CancellationToken>>>,
) {
    let mut session: Option<(SessionId, Transcript)> = None;
    loop {
        tokio::select! {
            _ = wake.notified() => {}
            _ = shared.shutdown.cancelled() => return,
        }
        // 一轮结束后如果期间又来了新消息，接着再跑；出错就停下等下一次唤醒，避免原地打转。
        loop {
            match turn(&shared, &room_id, &mut session, &current).await {
                Ok(true) if !shared.shutdown.is_cancelled() => continue,
                Ok(_) => break,
                Err(error) => {
                    tracing::error!(room = %room_id, "这一轮没跑成：{error}");
                    break;
                }
            }
        }
    }
}

/// 跑一轮。返回 `Ok(true)` 表示正常跑完了，可以再看一眼有没有新消息。
async fn turn(
    shared: &Arc<Shared>,
    room_id: &RoomId,
    session: &mut Option<(SessionId, Transcript)>,
    current: &Mutex<Option<CancellationToken>>,
) -> Result<bool, String> {
    let engine = shared.engine.clone();
    let room = room_id.clone();
    let (bot, session_id) = db(move || {
        let room = engine.get_room(&room)?;
        let bot_id = room
            .host_bot_id
            .ok_or_else(|| EngineError::Invalid("房间里没有 Bot".into()))?;
        let bot = engine.get_bot(&bot_id)?;
        let session_id = engine.ensure_front_session(&room.id, &bot_id)?;
        Ok((bot, session_id))
    })
    .await
    .map_err(|e| e.to_string())?;

    // 会话记录在这个房间任务的生命周期里只打开一次。
    if session.as_ref().is_none_or(|(id, _)| id != &session_id) {
        let path = shared
            .settings
            .sessions_dir
            .join(format!("{session_id}.jsonl"));
        match Transcript::open(&path) {
            Ok(transcript) => *session = Some((session_id.clone(), transcript)),
            Err(error) => {
                fail_fast(shared, room_id, &bot, error.to_string()).await;
                return Ok(false);
            }
        }
    }
    let (_, transcript) = session.as_mut().expect("上面刚放进去");

    let mut inbox = RoomInbox {
        shared: shared.clone(),
        room_id: room_id.clone(),
        last_seq: transcript.last_message_seq(),
    };
    let first = inbox.drain().await;
    if first.is_empty() {
        return Ok(false);
    }
    let mut inbox = Primed {
        first: Some(first),
        rest: inbox,
    };

    let engine = shared.engine.clone();
    let (room, bot_id) = (room_id.clone(), bot.id.clone());
    let draft = db(move || engine.begin_bot_message(&room, &bot_id))
        .await
        .map_err(|e| e.to_string())?;

    let cancel = shared.shutdown.child_token();
    *current.lock().unwrap() = Some(cancel.clone());

    let outcome = match shared.models.resolve(bot.tier) {
        Err(reason) => {
            // 模型用不了：这一轮的输入照样记进会话，配置好之后接着聊时模型能看到。
            let recorded = record_inputs(transcript, inbox.drain().await);
            Err(recorded.err().unwrap_or(reason))
        }
        Ok(model) => {
            let config = TurnConfig {
                model: model.model.clone(),
                system_prompt: render_prompt(&shared.settings, &bot.name),
                max_steps: shared.settings.max_steps,
                max_tokens: None,
                thinking: model.thinking,
                retry: RetryPolicy::default(),
            };
            let mut ctx = ToolCtx::new(shared.settings.fence.clone());
            ctx.allow_private_network = shared.settings.allow_private_network;
            let observer = RoomObserver {
                shared: shared.clone(),
                room_id: room_id.clone(),
                bot: bot.clone(),
                session_id: session_id.clone(),
                message_id: draft.id.clone(),
                steps: Mutex::new(vec![]),
            };
            let outcome = run_turn(
                transcript,
                model.provider.as_ref(),
                &shared.tools,
                &ctx,
                &config,
                &mut inbox,
                &observer,
                &cancel,
            )
            .await;
            observer.settle_steps().await;
            Ok(outcome.end)
        }
    };
    *current.lock().unwrap() = None;

    let (state, error) = match outcome {
        Ok(TurnEnd::Completed) => (MessageState::Complete, None),
        // 服务关闭造成的取消算「中断」，人点的停止算「已停止」。
        Ok(TurnEnd::Cancelled) if shared.shutdown.is_cancelled() => {
            (MessageState::Interrupted, None)
        }
        Ok(TurnEnd::Cancelled) => (MessageState::Cancelled, None),
        Ok(TurnEnd::MaxSteps) => (
            MessageState::Failed,
            Some(format!(
                "连续调用了 {} 次模型还没做完，先停在这里。可以让它继续，或者把问题拆小一点。",
                shared.settings.max_steps
            )),
        ),
        Ok(TurnEnd::Failed(reason)) | Err(reason) => (MessageState::Failed, Some(reason)),
    };
    let finished_ok = state == MessageState::Complete;
    let engine = shared.engine.clone();
    db(move || engine.finish_message(&draft.id, state, error))
        .await
        .map_err(|e| e.to_string())?;
    Ok(finished_ok)
}

fn record_inputs(transcript: &mut Transcript, inputs: Vec<UserInput>) -> Result<(), String> {
    for input in inputs {
        transcript
            .append(sa_kernel::EntryBody::User {
                content: input.content,
                message_id: input.message_id,
                message_seq: input.message_seq,
            })
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 还没开始就失败（会话记录打不开）：直接放一条失败的回复，让人在界面上看到原因。
async fn fail_fast(shared: &Arc<Shared>, room_id: &RoomId, bot: &Bot, reason: String) {
    let engine = shared.engine.clone();
    let (room, bot_id) = (room_id.clone(), bot.id.clone());
    let result = db(move || {
        let draft = engine.begin_bot_message(&room, &bot_id)?;
        engine.finish_message(&draft.id, MessageState::Failed, Some(reason))
    })
    .await;
    if let Err(error) = result {
        tracing::error!(room = %room_id, "写不进失败提示：{error}");
    }
}

/// 房间里还没进过会话的人类消息。
struct RoomInbox {
    shared: Arc<Shared>,
    room_id: RoomId,
    last_seq: i64,
}

#[async_trait]
impl Inbox for RoomInbox {
    async fn drain(&mut self) -> Vec<UserInput> {
        let engine = self.shared.engine.clone();
        let (room, after) = (self.room_id.clone(), self.last_seq);
        let messages = match db(move || engine.user_messages_after(&room, after)).await {
            Ok(messages) => messages,
            Err(error) => {
                tracing::error!(room = %self.room_id, "取不到新消息：{error}");
                return vec![];
            }
        };
        if let Some(last) = messages.last() {
            self.last_seq = last.seq;
        }
        messages
            .into_iter()
            .map(|message| UserInput {
                content: message.content,
                message_id: Some(message.id.to_string()),
                message_seq: Some(message.seq),
            })
            .collect()
    }
}

/// 第一批输入在开始这一轮之前已经取出来了（用来判断要不要开始），这里先把它交出去。
struct Primed {
    first: Option<Vec<UserInput>>,
    rest: RoomInbox,
}

#[async_trait]
impl Inbox for Primed {
    async fn drain(&mut self) -> Vec<UserInput> {
        match self.first.take() {
            Some(first) => first,
            None => self.rest.drain().await,
        }
    }
}

/// 把这一轮的过程写回房间里那条正在写的消息。
struct RoomObserver {
    shared: Arc<Shared>,
    room_id: RoomId,
    bot: Bot,
    session_id: SessionId,
    message_id: MessageId,
    steps: Mutex<Vec<ToolStep>>,
}

impl RoomObserver {
    async fn save_steps(&self) {
        let steps = self.steps.lock().unwrap().clone();
        let engine = self.shared.engine.clone();
        let id = self.message_id.clone();
        if let Err(error) = db(move || engine.set_steps(&id, steps).map(drop)).await {
            tracing::error!(room = %self.room_id, "工具步骤没写进去：{error}");
        }
    }

    /// 一轮结束时还显示「进行中」的步骤（被取消的）改成「已取消」。
    async fn settle_steps(&self) {
        let changed = {
            let mut steps = self.steps.lock().unwrap();
            let mut changed = false;
            for step in steps.iter_mut().filter(|s| s.state == StepState::Running) {
                step.state = StepState::Cancelled;
                changed = true;
            }
            changed
        };
        if changed {
            self.save_steps().await;
        }
    }
}

#[async_trait]
impl Observer for RoomObserver {
    async fn text_delta(&self, text: &str) {
        self.shared.engine.append_delta(&self.message_id, text);
    }

    async fn tool_started(&self, call_id: &str, tool: &str, title: &str) {
        self.steps.lock().unwrap().push(ToolStep {
            id: call_id.to_string(),
            tool: tool.to_string(),
            title: title.to_string(),
            state: StepState::Running,
        });
        self.save_steps().await;
    }

    async fn tool_finished(&self, call_id: &str, ok: bool) {
        {
            let mut steps = self.steps.lock().unwrap();
            // 从后往前找：同一个 id 出现两次时（模型偶尔会这样）改最近的那个。
            if let Some(step) = steps
                .iter_mut()
                .rev()
                .find(|s| s.id == call_id && s.state == StepState::Running)
            {
                step.state = if ok { StepState::Ok } else { StepState::Error };
            }
        }
        self.save_steps().await;
    }

    async fn usage(&self, model: &str, usage: Usage) {
        let record = UsageRecord {
            room_id: self.room_id.clone(),
            bot_id: self.bot.id.clone(),
            session_id: self.session_id.clone(),
            model: model.to_string(),
            in_tokens: usage.input_tokens as i64,
            out_tokens: usage.output_tokens as i64,
            cached_tokens: usage.cached_tokens as i64,
        };
        let engine = self.shared.engine.clone();
        if let Err(error) = db(move || engine.record_usage(record)).await {
            tracing::error!(room = %self.room_id, "用量没记上：{error}");
        }
    }
}
