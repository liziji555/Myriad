//! 一个房间的后台任务：有新消息就决定谁来接，让它跑一轮；它找了别的 Bot 就接着让那个 Bot 跑。

use crate::ask::AskBotTool;
use crate::episode::Job;
use crate::{render_prompt, router, RoomLink, Shared};
use async_trait::async_trait;
use sa_engine::{EngineError, UsageRecord};
use sa_kernel::{
    run_turn, ApprovalRequest, Inbox, Observer, Transcript, TurnConfig, TurnEnd, UserInput,
};
use sa_llm::{RetryPolicy, Usage};
use sa_protocol::{
    ApprovalDecision, ApprovalId, Bot, BotId, Message, MessageId, MessageState, Room, RoomActivity,
    RoomId, Sender, SessionId, StepApproval, StepState, ToolStep,
};
use sa_tools::{Fence, ToolCtx};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// Bot 表示「这次我不说话」的写法。
const PASS: &str = "(pass)";
/// 挑人接话时给路由看最近这么多条消息。
const ROUTER_CONTEXT: u32 = 12;

type Sessions = HashMap<BotId, (SessionId, Transcript)>;

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
    link: Arc<RoomLink>,
) {
    // 每个 Bot 在这个房间里的会话记录，在这个房间任务的生命周期里只打开一次。
    let mut sessions = Sessions::new();
    loop {
        tokio::select! {
            _ = wake.notified() => {}
            _ = shared.shutdown.cancelled() => return,
        }
        // 出错就停下等下一次唤醒，避免原地打转。
        if let Err(error) = work(&shared, &room_id, &link, &mut sessions).await {
            tracing::error!(room = %room_id, "这一轮没跑成：{error}");
        }
        announce(&shared, &room_id, &link, None).await;
    }
}

/// 把手头的事做完：新来的人类消息决定由谁接，然后排着队的 Bot 一个一个说，直到没有人要说话。
async fn work(
    shared: &Arc<Shared>,
    room_id: &RoomId,
    link: &Arc<RoomLink>,
    sessions: &mut Sessions,
) -> Result<(), String> {
    while !shared.shutdown.is_cancelled() {
        let engine = shared.engine.clone();
        let id = room_id.clone();
        let (room, members) = db(move || {
            let room = engine.get_room(&id)?;
            // 按进房间的先后排。
            let all = engine.list_bots()?;
            let members: Vec<Bot> = room
                .member_bot_ids
                .iter()
                .filter_map(|id| all.iter().find(|bot| &bot.id == id && bot.enabled).cloned())
                .collect();
            Ok((room, members))
        })
        .await
        .map_err(|e| e.to_string())?;

        if link.handled.load(Ordering::SeqCst) < 0 {
            // 刚启动：哪些人类消息已经有 Bot 看过，从会话记录里算。
            let mut seen = 0;
            for bot in &members {
                match session_of(shared, room_id, &bot.id, sessions).await {
                    Ok((_, transcript)) => seen = seen.max(transcript.last_message_seq()),
                    Err(error) => tracing::warn!(room = %room_id, "会话记录打不开：{error}"),
                }
            }
            link.handled.store(seen, Ordering::SeqCst);
        }

        let engine = shared.engine.clone();
        let (id, after) = (room_id.clone(), link.handled.load(Ordering::SeqCst));
        let fresh = db(move || engine.user_messages_after(&id, after))
            .await
            .map_err(|e| e.to_string())?;
        if let Some(last) = fresh.last() {
            link.handled.fetch_max(last.seq, Ordering::SeqCst);
            let first = dispatch(shared, &room, &members, &fresh, link).await;
            // 人说了新的话：之前排着队的不算了，重新开始一轮。
            link.episode()
                .restart(first.map(|bot_id| Job { bot_id, hop: 0 }));
        }

        let Some(job) = link.episode().next() else {
            return Ok(());
        };
        // 排队的时候它还在，现在可能已经被请出房间或者停用了。
        let Some(bot) = members.iter().find(|bot| bot.id == job.bot_id).cloned() else {
            continue;
        };
        let others: Vec<Bot> = members
            .into_iter()
            .filter(|other| other.id != bot.id)
            .collect();
        turn(shared, room_id, &bot, job.hop, &others, link, sessions).await?;
    }
    Ok(())
}

/// 新来的人类消息由谁接。点了名的听点名的，没点名的由路由挑。
async fn dispatch(
    shared: &Arc<Shared>,
    room: &Room,
    members: &[Bot],
    fresh: &[Message],
    link: &Arc<RoomLink>,
) -> Option<BotId> {
    let is_member = |id: &BotId| members.iter().any(|bot| &bot.id == id);
    // 攒了几条时，以最后一条点了名的为准。
    let mentioned = fresh
        .iter()
        .rev()
        .find_map(|message| message.mentions.iter().find(|id| is_member(id)));
    if let Some(bot_id) = mentioned {
        return Some(bot_id.clone());
    }
    let latest = fresh.last()?;
    let fallback = room
        .host_bot_id
        .clone()
        .filter(|id| is_member(id))
        .or_else(|| members.first().map(|bot| bot.id.clone()));
    let Some(fallback) = fallback else {
        // 房间里没有能说话的 Bot：放一条失败的回复说明原因。
        if let Some(host) = &room.host_bot_id {
            let reason = "房间里的 Bot 都停用了，没有人能回应。请在成员面板里启用一个。";
            fail_fast(shared, &room.id, host, reason.into()).await;
        }
        return None;
    };

    let engine = shared.engine.clone();
    let id = room.id.clone();
    let recent = db(move || engine.list_messages(&id, None, ROUTER_CONTEXT))
        .await
        .map(|page| page.messages)
        .unwrap_or_default();
    let cancel = shared.shutdown.child_token();
    *link.current.lock().unwrap() = Some((None, cancel.clone()));
    let chosen = router::route(
        shared, &room.id, members, &fallback, &recent, latest, &cancel,
    )
    .await;
    *link.current.lock().unwrap() = None;
    chosen
}

/// 打开（或取出已经打开的）某个 Bot 在这个房间里的会话记录。
async fn session_of<'a>(
    shared: &Arc<Shared>,
    room_id: &RoomId,
    bot_id: &BotId,
    sessions: &'a mut Sessions,
) -> Result<&'a mut (SessionId, Transcript), String> {
    if !sessions.contains_key(bot_id) {
        let engine = shared.engine.clone();
        let (room, bot) = (room_id.clone(), bot_id.clone());
        let session_id = db(move || engine.ensure_front_session(&room, &bot))
            .await
            .map_err(|e| e.to_string())?;
        let path = shared
            .settings
            .sessions_dir
            .join(format!("{session_id}.jsonl"));
        let transcript = Transcript::open(&path).map_err(|e| e.to_string())?;
        sessions.insert(bot_id.clone(), (session_id, transcript));
    }
    Ok(sessions.get_mut(bot_id).expect("上面刚放进去"))
}

/// 告诉外面房间里现在谁在说话、谁在排队。
async fn announce(
    shared: &Arc<Shared>,
    room_id: &RoomId,
    link: &RoomLink,
    speaking: Option<&BotId>,
) {
    let activity = RoomActivity {
        room_id: room_id.clone(),
        speaking: speaking.cloned(),
        queued: link.episode().queued(),
        handled_seq: link.handled.load(Ordering::SeqCst).max(0),
    };
    let engine = shared.engine.clone();
    if let Err(error) = db(move || engine.record_activity(&activity)).await {
        tracing::error!(room = %room_id, "房间动态没发出去：{error}");
    }
}

/// 让一个 Bot 说一轮。`hop` 是这次发言在 Bot 之间转到了第几手，`others` 是房间里别的 Bot。
async fn turn(
    shared: &Arc<Shared>,
    room_id: &RoomId,
    bot: &Bot,
    hop: u32,
    others: &[Bot],
    link: &Arc<RoomLink>,
    sessions: &mut Sessions,
) -> Result<(), String> {
    let (session_id, transcript) = match session_of(shared, room_id, &bot.id, sessions).await {
        Ok((session_id, transcript)) => (session_id.clone(), transcript),
        Err(error) => {
            fail_fast(shared, room_id, &bot.id, error).await;
            return Err("会话记录打不开".into());
        }
    };

    let mut names: HashMap<BotId, String> = others
        .iter()
        .map(|other| (other.id.clone(), other.name.clone()))
        .collect();
    names.insert(bot.id.clone(), bot.name.clone());
    let mut inbox = RoomInbox {
        shared: shared.clone(),
        link: link.clone(),
        room_id: room_id.clone(),
        bot_id: bot.id.clone(),
        names,
        solo: others.is_empty(),
        last_seq: transcript.last_message_seq(),
    };
    let first = inbox.drain().await;
    if first.is_empty() {
        return Ok(());
    }
    let mut inbox = Primed {
        first: Some(first),
        rest: inbox,
    };

    let engine = shared.engine.clone();
    let (room, bot_id) = (room_id.clone(), bot.id.clone());
    let draft = db(move || engine.begin_bot_message(&room, &bot_id, hop))
        .await
        .map_err(|e| e.to_string())?;

    let cancel = shared.shutdown.child_token();
    *link.current.lock().unwrap() = Some((Some(bot.id.clone()), cancel.clone()));
    announce(shared, room_id, link, Some(&bot.id)).await;

    let observer = RoomObserver {
        shared: shared.clone(),
        room_id: room_id.clone(),
        bot: bot.clone(),
        session_id: session_id.clone(),
        message_id: draft.id.clone(),
        steps: Mutex::new(vec![]),
        held: Mutex::new(Some(String::new())),
    };
    let outcome = match shared.models.resolve(bot.tier) {
        Err(reason) => {
            // 模型用不了：这一轮的输入照样记进会话，配置好之后接着聊时模型能看到。
            let recorded = record_inputs(transcript, inbox.drain().await);
            Err(recorded.err().unwrap_or(reason))
        }
        Ok(model) => {
            let config = TurnConfig {
                model: model.model.clone(),
                system_prompt: render_prompt(&shared.settings, bot, others),
                max_steps: shared.settings.max_steps,
                max_tokens: None,
                thinking: model.thinking,
                session_id: Some(session_id.to_string()),
                context_window: Some(model.context_window.unwrap_or_else(|| {
                    model.provider.capabilities(&model.model).context_window as usize
                })),
                spill_dir: shared
                    .settings
                    .sessions_dir
                    .parent()
                    .map(|d| d.join("spill")),
                retry: RetryPolicy::default(),
            };
            let mut ctx = ToolCtx::new(shared.settings.fence.clone());
            ctx.allow_private_network = shared.settings.allow_private_network;
            // 房间里有别的 Bot 时才给它「找别的 Bot」这个工具。
            let mut tools = shared.tools.clone();
            if !others.is_empty() {
                tools.register(AskBotTool {
                    shared: shared.clone(),
                    link: link.clone(),
                    room_id: room_id.clone(),
                    from: bot.clone(),
                    hop,
                });
            }
            let outcome = run_turn(
                transcript,
                model.provider.as_ref(),
                &tools,
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
    *link.current.lock().unwrap() = None;

    let (state, error) = match outcome {
        Ok(TurnEnd::Completed) => (MessageState::Complete, None),
        // 服务关闭造成的取消算「中断」，人点的停止算「已停止」。
        Ok(TurnEnd::Cancelled) if shared.shutdown.is_cancelled() => {
            (MessageState::Interrupted, None)
        }
        Ok(TurnEnd::Cancelled) => (MessageState::Cancelled, None),
        Ok(TurnEnd::Truncated) => (
            MessageState::Failed,
            Some(
                "回复太长，到了模型单次输出的上限，后面被截断了。发一句「继续」可以让它接着写。"
                    .into(),
            ),
        ),
        Ok(TurnEnd::MaxSteps) => (
            MessageState::Failed,
            Some(format!(
                "连续调用了 {} 次模型还没做完，先停在这里。可以让它继续，或者把问题拆小一点。",
                shared.settings.max_steps
            )),
        ),
        Ok(TurnEnd::ContextLimit) => (
            MessageState::Failed,
            Some(
                "对话太长，已经放不进模型的上下文窗口，自动压缩也没能把它压下来。请新建一个房间继续，或者过一会儿再试。"
                    .into(),
            ),
        ),
        Ok(TurnEnd::Failed(reason)) | Err(reason) => (MessageState::Failed, Some(reason)),
    };

    // 它决定不说话：没做过任何事就把这条空的回复撤掉，做过事（有工具步骤）就留着步骤、不留字。
    let silent = observer.release_held(state == MessageState::Complete);
    let engine = shared.engine.clone();
    let has_steps = !observer.steps.lock().unwrap().is_empty();
    let finished = db(move || {
        if silent && !has_steps {
            engine.discard_message(&draft.id).map(|()| None)
        } else {
            engine.finish_message(&draft.id, state, error).map(Some)
        }
    })
    .await
    .map_err(|e| e.to_string())?;
    if let Some(message) = finished {
        link.episode().remember(&message.content);
    }
    if state == MessageState::Complete {
        Ok(())
    } else {
        // 没正常说完（出错、被停下）：这一轮交流到此为止，排着队的也不再说。
        link.episode().clear_queue();
        Err(format!("{} 的回复没有正常结束", bot.name))
    }
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

/// 还没开始就失败（比如会话记录打不开）：直接放一条失败的回复，让人在界面上看到原因。
async fn fail_fast(shared: &Arc<Shared>, room_id: &RoomId, bot_id: &BotId, reason: String) {
    let engine = shared.engine.clone();
    let (room, bot_id) = (room_id.clone(), bot_id.clone());
    let result = db(move || {
        let draft = engine.begin_bot_message(&room, &bot_id, 0)?;
        engine.finish_message(&draft.id, MessageState::Failed, Some(reason))
    })
    .await;
    if let Err(error) = result {
        tracing::error!(room = %room_id, "写不进失败提示：{error}");
    }
}

/// 房间里这个 Bot 还没看过的消息：人说的，和别的 Bot 说的。
struct RoomInbox {
    shared: Arc<Shared>,
    link: Arc<RoomLink>,
    room_id: RoomId,
    bot_id: BotId,
    /// 房间里每个 Bot 的名字。
    names: HashMap<BotId, String>,
    /// 房间里只有它一个 Bot：人说的话原样给它，不加「是谁说的」。
    solo: bool,
    last_seq: i64,
}

#[async_trait]
impl Inbox for RoomInbox {
    async fn drain(&mut self) -> Vec<UserInput> {
        let engine = self.shared.engine.clone();
        let (room, after) = (self.room_id.clone(), self.last_seq);
        let messages = match db(move || engine.messages_after(&room, after)).await {
            Ok(messages) => messages,
            Err(error) => {
                tracing::error!(room = %self.room_id, "取不到新消息：{error}");
                return vec![];
            }
        };
        let mut inputs = vec![];
        for message in messages {
            let speaker = match &message.sender {
                Sender::User => None,
                // 自己说过的话已经在自己的会话记录里了。
                Sender::Bot { bot_id } if bot_id == &self.bot_id => {
                    if message.state == MessageState::Streaming {
                        continue;
                    }
                    self.last_seq = message.seq;
                    continue;
                }
                Sender::Bot { bot_id } => {
                    Some(self.names.get(bot_id).map_or("另一个 Bot", String::as_str))
                }
            };
            // 别人还没写完的不看，也不越过它：等它写完了再一起看。
            if message.state == MessageState::Streaming {
                break;
            }
            self.last_seq = message.seq;
            if message.content.trim().is_empty() {
                continue;
            }
            if speaker.is_none() {
                // 正在说话的 Bot 看到了这条人类消息：不用再另外挑人来接。
                self.link.handled.fetch_max(message.seq, Ordering::SeqCst);
            }
            let to_me = if message.mentions.contains(&self.bot_id) {
                "（对你说）"
            } else {
                ""
            };
            let content = match speaker {
                None if self.solo => message.content,
                None => format!("【用户{to_me}】{}", message.content),
                Some(name) => format!("【{name}{to_me}】{}", message.content),
            };
            inputs.push(UserInput {
                content,
                message_id: Some(message.id.to_string()),
                message_seq: Some(message.seq),
            });
        }
        inputs
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
    /// 回复开头还分不清是不是 `(pass)` 的那几个字，先扣着不显示，免得界面上闪一下。
    /// 一旦确定不是，就变成 `None`，之后的字直接显示。
    held: Mutex<Option<String>>,
}

impl RoomObserver {
    /// 一轮结束时处理扣着的字。返回这一轮是不是「不说话」：正常结束，而且说的只有 `(pass)` 或者什么都没说。
    /// 不是的话，把扣着的字补显示出来。
    fn release_held(&self, completed: bool) -> bool {
        let Some(held) = self.held.lock().unwrap().take() else {
            return false;
        };
        let said = held.trim();
        if completed && (said.is_empty() || said == PASS) {
            return true;
        }
        self.shared.engine.append_delta(&self.message_id, &held);
        false
    }

    async fn save_steps(&self) {
        let steps = self.steps.lock().unwrap().clone();
        let engine = self.shared.engine.clone();
        let id = self.message_id.clone();
        if let Err(error) = db(move || engine.set_steps(&id, steps).map(drop)).await {
            tracing::error!(room = %self.room_id, "工具步骤没写进去：{error}");
        }
    }

    /// 改最近的那个 id 是 `call_id` 的步骤（同一个 id 出现两次时，模型偶尔会这样，改后面那个）。
    fn update_step(&self, call_id: &str, change: impl FnOnce(&mut ToolStep)) {
        let mut steps = self.steps.lock().unwrap();
        if let Some(step) = steps.iter_mut().rev().find(|s| s.id == call_id) {
            change(step);
        }
    }

    /// 一轮结束时还显示「进行中」或「等批准」的步骤（被取消的）改成「已取消」。
    async fn settle_steps(&self) {
        let changed = {
            let mut steps = self.steps.lock().unwrap();
            let mut changed = false;
            for step in steps
                .iter_mut()
                .filter(|s| matches!(s.state, StepState::Running | StepState::Waiting))
            {
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
        let mut held = self.held.lock().unwrap();
        let Some(buffer) = held.as_mut() else {
            drop(held);
            self.shared.engine.append_delta(&self.message_id, text);
            return;
        };
        buffer.push_str(text);
        let said = buffer.trim();
        // 到目前为止还可能是 `(pass)`：接着扣。
        if PASS.starts_with(said) {
            return;
        }
        let all = held.take().expect("上面刚看过");
        drop(held);
        self.shared.engine.append_delta(&self.message_id, &all);
    }

    async fn tool_started(&self, call_id: &str, tool: &str, title: &str) {
        self.steps.lock().unwrap().push(ToolStep {
            id: call_id.to_string(),
            tool: tool.to_string(),
            title: title.to_string(),
            state: StepState::Running,
            approval: None,
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

    async fn request_approval(&self, request: ApprovalRequest<'_>) -> bool {
        let engine = &self.shared.engine;
        let key = rule_key(request.tool, request.input, &self.shared.settings.fence);
        if let Some(key) = key.clone() {
            let (engine, room, tool) = (
                engine.clone(),
                self.room_id.clone(),
                request.tool.to_string(),
            );
            match db(move || engine.is_always_allowed(&room, &tool, &key)).await {
                Ok(true) => return true,
                Ok(false) => {}
                Err(error) => tracing::error!(room = %self.room_id, "查不了已保存的许可：{error}"),
            }
        }

        let id = ApprovalId::new();
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.shared
            .approvals
            .lock()
            .unwrap()
            .insert(id.clone(), reply);
        // 不管怎么离开（答复、超时、这一轮被停下），这张卡都不再接受答复。
        let _pending = Pending {
            shared: &self.shared,
            id: id.clone(),
        };
        let timeout = self.shared.settings.approval_timeout;
        let approval = StepApproval {
            id,
            reason: request.reason.to_string(),
            expires_at: sa_engine::now_ms() + timeout.as_millis() as i64,
        };
        self.update_step(request.call_id, |step| {
            step.state = StepState::Waiting;
            step.approval = Some(approval);
        });
        self.save_steps().await;

        let decision = tokio::select! {
            answer = answer => answer.unwrap_or(ApprovalDecision::Deny),
            _ = tokio::time::sleep(timeout) => ApprovalDecision::Deny,
        };
        let allowed = decision != ApprovalDecision::Deny;
        if let (ApprovalDecision::Always, Some(key)) = (decision, key) {
            let (engine, room, tool) = (
                engine.clone(),
                self.room_id.clone(),
                request.tool.to_string(),
            );
            if let Err(error) = db(move || engine.allow_always(&room, &tool, &key).map(drop)).await
            {
                tracing::error!(room = %self.room_id, "「总是允许」没记上：{error}");
            }
        }
        self.update_step(request.call_id, |step| {
            step.state = if allowed {
                StepState::Running
            } else {
                StepState::Denied
            };
        });
        self.save_steps().await;
        allowed
    }

    async fn usage(&self, model: &str, usage: Usage) {
        let record = UsageRecord {
            room_id: self.room_id.clone(),
            bot_id: Some(self.bot.id.clone()),
            session_id: Some(self.session_id.clone()),
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

/// 等待中的审批；离开作用域时从登记表里去掉。
struct Pending<'a> {
    shared: &'a Shared,
    id: ApprovalId,
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        self.shared.approvals.lock().unwrap().remove(&self.id);
    }
}

/// 「总是允许」记的是什么：命令记原文，写文件记文件的完整路径。其他工具不能记。
fn rule_key(tool: &str, input: &Value, fence: &Fence) -> Option<String> {
    match tool {
        "shell" => {
            let command = input["command"].as_str()?.trim();
            (!command.is_empty()).then(|| command.to_string())
        }
        "write" | "edit" => {
            let (real, _) = fence.resolve_for_write(input["path"].as_str()?).ok()?;
            Some(real.display().to_string())
        }
        _ => None,
    }
}
