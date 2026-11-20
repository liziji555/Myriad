//! 消息：人发的一次写完，Bot 发的先建一条「正在写」的草稿，边写边广播，结束时定稿。

use crate::team::find_mentions;
use crate::{now_ms, to_json, Engine, EngineError, Result, MAX_MESSAGE_CHARS};
use sa_protocol::{
    BotId, EventKind, Message, MessageDelta, MessageId, MessagePage, MessageRef, MessageState,
    RoomActivity, RoomId, Sender, SessionId, StepState, ToolStep,
};
use sa_store::{bots, events, messages, rooms, sessions, usage};
use tokio::sync::broadcast;

pub const MAX_PAGE: u32 = 200;

/// 一次模型调用的用量。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRecord {
    pub room_id: RoomId,
    /// 哪个 Bot 花的。决定谁来接话的那次调用不属于任何 Bot。
    pub bot_id: Option<BotId>,
    pub session_id: Option<SessionId>,
    pub model: String,
    pub in_tokens: i64,
    pub out_tokens: i64,
    pub cached_tokens: i64,
}

impl Engine {
    /// 人在房间里发一条消息。
    pub fn post_user_message(&self, room_id: &RoomId, content: &str) -> Result<Message> {
        if content.trim().is_empty() {
            return Err(EngineError::Invalid("消息不能为空".into()));
        }
        if content.chars().count() > MAX_MESSAGE_CHARS {
            return Err(EngineError::Invalid(format!(
                "消息不能超过 {MAX_MESSAGE_CHARS} 个字"
            )));
        }
        // 人写的 `@名字` 在这里认出来：只认房间里启用着的成员。
        self.insert_message(
            room_id,
            Sender::User,
            content.into(),
            MessageState::Complete,
            None,
            0,
        )
    }

    /// Bot 开始写一条回复：先放一条空的「正在写」消息，之后用
    /// [`Engine::append_delta`]、[`Engine::set_steps`]、[`Engine::finish_message`] 推进。
    /// `hop` 是这条回复在 Bot 之间转到了第几手。
    pub fn begin_bot_message(&self, room_id: &RoomId, bot_id: &BotId, hop: u32) -> Result<Message> {
        let message = self.insert_message(
            room_id,
            Sender::Bot {
                bot_id: bot_id.clone(),
            },
            String::new(),
            MessageState::Streaming,
            Some(vec![]),
            hop,
        )?;
        self.drafts().insert(message.id.clone(), message.clone());
        Ok(message)
    }

    /// Bot 对另一个 Bot 说的一句话：一次写完，房间里的人都看得到。
    pub fn post_bot_message(
        &self,
        room_id: &RoomId,
        bot_id: &BotId,
        content: &str,
        mentions: Vec<BotId>,
        hop: u32,
    ) -> Result<Message> {
        self.insert_message(
            room_id,
            Sender::Bot {
                bot_id: bot_id.clone(),
            },
            content.into(),
            MessageState::Complete,
            Some(mentions),
            hop,
        )
    }

    /// `mentions` 为 `None` 时从内容里的 `@名字` 认。
    fn insert_message(
        &self,
        room_id: &RoomId,
        sender: Sender,
        content: String,
        state: MessageState,
        mentions: Option<Vec<BotId>>,
        hop: u32,
    ) -> Result<Message> {
        let now = now_ms();
        let (message, seq) = self
            .store
            .write(|tx| {
                let Some(room) = rooms::get(tx, room_id)? else {
                    return Ok(None);
                };
                let mentions = match mentions {
                    Some(mentions) => mentions,
                    None => {
                        let members: Vec<_> = bots::list(tx)?
                            .into_iter()
                            .filter(|bot| bot.enabled && room.member_bot_ids.contains(&bot.id))
                            .collect();
                        find_mentions(&content, &members)
                    }
                };
                let mut message = Message {
                    id: MessageId::new(),
                    seq: 0,
                    room_id: room_id.clone(),
                    sender,
                    content,
                    mentions,
                    hop,
                    steps: vec![],
                    state,
                    error: None,
                    rev: 0,
                    created_at: now,
                    updated_at: now,
                };
                message.seq = messages::insert(tx, &message)?;
                let seq = events::append(
                    tx,
                    Some(room_id),
                    EventKind::MessageCreated,
                    &to_json(&message),
                    now,
                )?;
                Ok(Some((message, seq)))
            })?
            .ok_or_else(|| EngineError::NotFound("没有这个房间".into()))?;
        self.publish(seq);
        Ok(message)
    }

    /// 给正在写的消息追加一段文字。只改内存并广播，不落库。
    pub fn append_delta(&self, id: &MessageId, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut drafts = self.drafts();
        let Some(draft) = drafts.get_mut(id) else {
            return;
        };
        draft.content.push_str(text);
        draft.rev += 1;
        // 在持有草稿锁时广播，保证片段按版本号的顺序发出。没有订阅者时 send 会报错，可以忽略。
        let _ = self.live.send(MessageDelta {
            message_id: id.clone(),
            room_id: draft.room_id.clone(),
            rev: draft.rev,
            text: text.to_string(),
        });
    }

    /// 更新正在写的消息的工具步骤，并把到目前为止的内容一起落库。
    pub fn set_steps(&self, id: &MessageId, steps: Vec<ToolStep>) -> Result<Message> {
        self.persist_draft(id, false, |draft| draft.steps = steps)
    }

    /// 定稿：写入最终状态，之后这条消息不再变化。
    pub fn finish_message(
        &self,
        id: &MessageId,
        state: MessageState,
        error: Option<String>,
    ) -> Result<Message> {
        debug_assert!(state != MessageState::Streaming);
        self.persist_draft(id, true, |draft| {
            draft.state = state;
            draft.error = error;
        })
    }

    fn persist_draft(
        &self,
        id: &MessageId,
        done: bool,
        change: impl FnOnce(&mut Message),
    ) -> Result<Message> {
        // 整个过程持有草稿锁：落库的内容和版本号与广播出去的片段保持一致的先后顺序。
        let mut drafts = self.drafts();
        let draft = drafts
            .get_mut(id)
            .ok_or_else(|| EngineError::NotFound("这条消息已经写完了".into()))?;
        let mut next = draft.clone();
        change(&mut next);
        next.rev += 1;
        next.updated_at = now_ms();
        let seq = self.store.write(|tx| {
            messages::update(tx, &next)?;
            events::append(
                tx,
                Some(&next.room_id),
                EventKind::MessageUpdated,
                &to_json(&next),
                next.updated_at,
            )
        })?;
        if done {
            drafts.remove(id);
        } else {
            *draft = next.clone();
        }
        drop(drafts);
        self.publish(seq);
        Ok(next)
    }

    /// 启动时调用：上次进程退出时还「正在写」的消息标成「中断」。返回处理了几条。
    pub fn recover_interrupted(&self) -> Result<usize> {
        let now = now_ms();
        let (count, seq) = self.store.write(|tx| {
            let mut last = None;
            let stuck = messages::list_in_state(tx, MessageState::Streaming)?;
            for mut message in stuck.iter().cloned() {
                message.state = MessageState::Interrupted;
                // 没跑完的步骤和没人答复的审批都作废。
                for step in &mut message.steps {
                    if matches!(step.state, StepState::Running | StepState::Waiting) {
                        step.state = StepState::Cancelled;
                    }
                }
                message.rev += 1;
                message.updated_at = now;
                messages::update(tx, &message)?;
                last = Some(events::append(
                    tx,
                    Some(&message.room_id),
                    EventKind::MessageUpdated,
                    &to_json(&message),
                    now,
                )?);
            }
            Ok((stuck.len(), last))
        })?;
        if let Some(seq) = seq {
            self.publish(seq);
        }
        Ok(count)
    }

    /// 房间里 `before` 之前的最近 `limit` 条消息（`before` 为空表示从最新的开始）。
    /// 正在写的消息带上内存里的最新内容。
    pub fn list_messages(
        &self,
        room_id: &RoomId,
        before: Option<i64>,
        limit: u32,
    ) -> Result<MessagePage> {
        let limit = limit.clamp(1, MAX_PAGE);
        // 先拿草稿锁再读库：读到的行不会比草稿新。
        let drafts = self.drafts();
        let mut page = self
            .store
            .read(|c| {
                if rooms::get(c, room_id)?.is_none() {
                    return Ok(None);
                }
                Ok(Some(messages::list_before(c, room_id, before, limit + 1)?))
            })?
            .ok_or_else(|| EngineError::NotFound("没有这个房间".into()))?;
        let has_more = page.len() > limit as usize;
        if has_more {
            page.remove(0);
        }
        for message in &mut page {
            if let Some(draft) = drafts.get(&message.id) {
                *message = draft.clone();
            }
        }
        Ok(MessagePage {
            messages: page,
            has_more,
        })
    }

    /// 房间里 `after_seq` 之后人发的消息。
    pub fn user_messages_after(&self, room_id: &RoomId, after_seq: i64) -> Result<Vec<Message>> {
        Ok(self
            .store
            .read(|c| messages::list_user_after(c, room_id, after_seq))?)
    }

    /// 房间里 `after_seq` 之后的全部消息，人和 Bot 的都算。正在写的那些内容是落库时的样子。
    pub fn messages_after(&self, room_id: &RoomId, after_seq: i64) -> Result<Vec<Message>> {
        Ok(self
            .store
            .read(|c| messages::list_after(c, room_id, after_seq))?)
    }

    /// 撤掉一条正在写的消息（Bot 决定不说话）。界面上这条消息会消失。
    pub fn discard_message(&self, id: &MessageId) -> Result<()> {
        let mut drafts = self.drafts();
        let draft = drafts
            .remove(id)
            .ok_or_else(|| EngineError::NotFound("这条消息已经写完了".into()))?;
        let gone = MessageRef {
            id: id.clone(),
            room_id: draft.room_id.clone(),
        };
        let seq = self.store.write(|tx| {
            messages::delete(tx, id)?;
            events::append(
                tx,
                Some(&gone.room_id),
                EventKind::MessageDeleted,
                &to_json(&gone),
                now_ms(),
            )
        })?;
        drop(drafts);
        self.publish(seq);
        Ok(())
    }

    /// 广播房间里现在谁在说话、谁在排队。
    pub fn record_activity(&self, activity: &RoomActivity) -> Result<()> {
        let seq = self.store.write(|tx| {
            events::append(
                tx,
                Some(&activity.room_id),
                EventKind::RoomActivity,
                &to_json(activity),
                now_ms(),
            )
        })?;
        self.publish(seq);
        Ok(())
    }

    /// 某个 Bot 在某个房间里的入口会话，没有就建一个。
    pub fn ensure_front_session(&self, room_id: &RoomId, bot_id: &BotId) -> Result<SessionId> {
        Ok(self
            .store
            .write(|tx| sessions::ensure_front(tx, room_id, bot_id, now_ms()))?)
    }

    pub fn record_usage(&self, record: UsageRecord) -> Result<()> {
        let row = usage::UsageRow {
            room_id: Some(record.room_id),
            bot_id: record.bot_id,
            session_id: record.session_id,
            model: record.model,
            in_tokens: record.in_tokens,
            out_tokens: record.out_tokens,
            cached_tokens: record.cached_tokens,
            created_at: now_ms(),
        };
        Ok(self.store.write(|tx| usage::insert(tx, &row))?)
    }

    /// 订阅流式片段。片段不落库：落后太多（`Lagged`）或刚连上时，用
    /// [`Engine::list_messages`] 取完整内容。
    pub fn subscribe_live(&self) -> broadcast::Receiver<MessageDelta> {
        self.live.subscribe()
    }

    fn drafts(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<MessageId, Message>> {
        self.drafts.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sa_protocol::StepState;
    use sa_store::Store;

    fn engine() -> (Engine, RoomId, BotId) {
        let engine = Engine::new(Store::open_in_memory().unwrap()).unwrap();
        engine.ensure_defaults().unwrap();
        let room = engine.list_rooms().unwrap().remove(0);
        let bot = room.host_bot_id.clone().unwrap();
        (engine, room.id, bot)
    }

    #[test]
    fn user_message_is_validated_stored_and_announced() {
        let (engine, room, _) = engine();
        assert!(matches!(
            engine.post_user_message(&room, "  \n"),
            Err(EngineError::Invalid(_))
        ));
        assert!(matches!(
            engine.post_user_message(&room, &"字".repeat(MAX_MESSAGE_CHARS + 1)),
            Err(EngineError::Invalid(_))
        ));
        assert!(matches!(
            engine.post_user_message(&RoomId::new(), "你好"),
            Err(EngineError::NotFound(_))
        ));

        let before = engine.event_head();
        let message = engine.post_user_message(&room, "你好").unwrap();
        assert_eq!(message.state, MessageState::Complete);
        let events = engine.events_after(before, 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::MessageCreated);
        assert_eq!(events[0].payload, to_json(&message));
        assert_eq!(
            engine.list_messages(&room, None, 50).unwrap().messages,
            vec![message]
        );
    }

    #[test]
    fn bot_message_streams_then_settles() {
        let (engine, room, bot) = engine();
        let mut live = engine.subscribe_live();
        let draft = engine.begin_bot_message(&room, &bot, 0).unwrap();
        assert_eq!(draft.state, MessageState::Streaming);

        engine.append_delta(&draft.id, "你");
        engine.append_delta(&draft.id, "好");
        assert_eq!(live.try_recv().unwrap().rev, 1);
        let second = live.try_recv().unwrap();
        assert_eq!((second.rev, second.text.as_str()), (2, "好"));

        // 快照里能看到还没落库的内容。
        let page = engine.list_messages(&room, None, 50).unwrap();
        assert_eq!(page.messages[0].content, "你好");
        assert_eq!(page.messages[0].rev, 2);

        let step = ToolStep {
            id: "c1".into(),
            tool: "read".into(),
            title: "读取 a.txt".into(),
            state: StepState::Running,
            approval: None,
        };
        let updated = engine.set_steps(&draft.id, vec![step.clone()]).unwrap();
        assert_eq!(updated.rev, 3);
        assert_eq!(updated.content, "你好");

        let done = engine
            .finish_message(&draft.id, MessageState::Complete, None)
            .unwrap();
        assert_eq!(done.rev, 4);
        assert_eq!(done.steps, vec![step]);
        assert_eq!(
            engine.list_messages(&room, None, 50).unwrap().messages,
            vec![done.clone()]
        );

        // 定稿之后不能再改。
        engine.append_delta(&draft.id, "多余");
        assert!(live.try_recv().is_err());
        assert!(engine
            .finish_message(&draft.id, MessageState::Failed, None)
            .is_err());
        let events = engine.events_after(0, 100).unwrap();
        assert_eq!(events.last().unwrap().payload, to_json(&done));
    }

    #[test]
    fn paging_reports_has_more() {
        let (engine, room, _) = engine();
        let all: Vec<_> = (0..5)
            .map(|i| {
                engine
                    .post_user_message(&room, &format!("第 {i} 条"))
                    .unwrap()
            })
            .collect();
        let latest = engine.list_messages(&room, None, 2).unwrap();
        assert!(latest.has_more);
        assert_eq!(latest.messages, all[3..]);
        let older = engine
            .list_messages(&room, Some(latest.messages[0].seq), 10)
            .unwrap();
        assert!(!older.has_more);
        assert_eq!(older.messages, all[..3]);
        assert!(matches!(
            engine.list_messages(&RoomId::new(), None, 10),
            Err(EngineError::NotFound(_))
        ));
    }

    #[test]
    fn streaming_messages_become_interrupted_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let (room, id) = {
            let engine = Engine::new(Store::open(&path).unwrap()).unwrap();
            engine.ensure_defaults().unwrap();
            let room = engine.list_rooms().unwrap().remove(0);
            let draft = engine
                .begin_bot_message(&room.id, room.host_bot_id.as_ref().unwrap(), 0)
                .unwrap();
            engine.append_delta(&draft.id, "写到一半");
            let step = |id: &str, state| ToolStep {
                id: id.into(),
                tool: "shell".into(),
                title: "执行 ls".into(),
                state,
                approval: None,
            };
            let steps = vec![
                step("a", StepState::Ok),
                step("b", StepState::Running),
                step("c", StepState::Waiting),
            ];
            engine.set_steps(&draft.id, steps).unwrap();
            engine.append_delta(&draft.id, "，没来得及落库");
            (room.id, draft.id)
        };
        let engine = Engine::new(Store::open(&path).unwrap()).unwrap();
        assert_eq!(engine.recover_interrupted().unwrap(), 1);
        assert_eq!(engine.recover_interrupted().unwrap(), 0);
        let page = engine.list_messages(&room, None, 10).unwrap();
        assert_eq!(page.messages[0].id, id);
        assert_eq!(page.messages[0].state, MessageState::Interrupted);
        assert_eq!(page.messages[0].content, "写到一半");
        // 没跑完的步骤和没人答复的审批不再显示成进行中。
        let states: Vec<_> = page.messages[0].steps.iter().map(|s| s.state).collect();
        assert_eq!(
            states,
            [StepState::Ok, StepState::Cancelled, StepState::Cancelled]
        );
    }

    #[test]
    fn front_session_is_stable_per_room_and_bot() {
        let (engine, room, bot) = engine();
        let first = engine.ensure_front_session(&room, &bot).unwrap();
        assert_eq!(engine.ensure_front_session(&room, &bot).unwrap(), first);
        let other = engine.create_room("另一个").unwrap();
        assert_ne!(engine.ensure_front_session(&other.id, &bot).unwrap(), first);
    }
}
