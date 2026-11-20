//! 定时任务：到点让一个 Bot 在房间里做一件事。

use crate::cron::Schedule;
use crate::messages::add_message;
use crate::{now_ms, to_json, Audit, Engine, EngineError, Result, MAX_MESSAGE_CHARS};
use sa_protocol::{
    BotId, EventKind, Message, MessageId, MessageKind, MessageState, Millis, RoomId, Routine,
    RoutineId, RoutineRef, RoutineResult, Sender,
};
use sa_store::{events, rooms, routines};

pub const MAX_ROUTINE_TITLE_CHARS: usize = 40;
/// 一个房间里最多几个定时任务。
pub const MAX_ROOM_ROUTINES: usize = 20;
/// 连续这么多次没做成就自动停用，等人来看。
pub const MAX_ROUTINE_FAILURES: u32 = 3;

/// 对一个定时任务的修改：只改给出的字段。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutinePatch {
    pub bot_id: Option<BotId>,
    pub title: Option<String>,
    pub prompt: Option<String>,
    pub schedule: Option<String>,
    pub enabled: Option<bool>,
}

impl Engine {
    /// `bot_id` 不给就是房间的入口 Bot。
    pub fn create_routine(
        &self,
        room_id: &RoomId,
        bot_id: Option<&BotId>,
        title: &str,
        prompt: &str,
        schedule: &str,
    ) -> Result<Routine> {
        let now = now_ms();
        let (routine, seq) = self.store.write(|tx| {
            let Some(room) = rooms::get(tx, room_id)? else {
                return Ok(Err(EngineError::NotFound("没有这个房间".into())));
            };
            let Some(bot_id) = bot_id.or(room.host_bot_id.as_ref()) else {
                return Ok(Err(EngineError::Invalid(
                    "这个房间里还没有 Bot，定时任务没人做".into(),
                )));
            };
            if !room.member_bot_ids.contains(bot_id) {
                return Ok(Err(EngineError::Invalid("这个 Bot 不在房间里".into())));
            }
            let existing = routines::list(tx)?;
            if existing.iter().filter(|r| &r.room_id == room_id).count() >= MAX_ROOM_ROUTINES {
                return Ok(Err(EngineError::Invalid(format!(
                    "一个房间里最多 {MAX_ROOM_ROUTINES} 个定时任务"
                ))));
            }
            let checked = (
                check_title(title),
                check_prompt(prompt),
                check_schedule(schedule),
            );
            let (title, prompt, (schedule, parsed)) = match checked {
                (Ok(title), Ok(prompt), Ok(schedule)) => (title, prompt, schedule),
                (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => return Ok(Err(e)),
            };
            let routine = Routine {
                id: RoutineId::new(),
                room_id: room_id.clone(),
                bot_id: bot_id.clone(),
                title,
                prompt,
                schedule,
                enabled: true,
                next_at: parsed.next_after(now),
                last_at: None,
                last_result: None,
                last_note: String::new(),
                failures: 0,
                created_at: now,
                updated_at: now,
            };
            routines::insert(tx, &routine)?;
            let seq = events::append(
                tx,
                Some(room_id),
                EventKind::RoutineCreated,
                &to_json(&routine),
                now,
            )?;
            Ok(Ok((routine, seq)))
        })??;
        self.publish(seq);
        let summary = format!(
            "新建了定时任务「{}」（{}）",
            routine.title, routine.schedule
        );
        self.audit(
            Audit::user("routine.created", summary)
                .room(&routine.room_id)
                .detail(&routine.prompt),
        );
        Ok(routine)
    }

    pub fn update_routine(&self, id: &RoutineId, patch: RoutinePatch) -> Result<Routine> {
        let now = now_ms();
        let (routine, seq) = self.store.write(|tx| {
            let Some(mut routine) = routines::get(tx, id)? else {
                return Ok(Err(EngineError::NotFound("没有这个定时任务".into())));
            };
            if let Some(bot_id) = patch.bot_id {
                let members = rooms::get(tx, &routine.room_id)?
                    .map(|room| room.member_bot_ids)
                    .unwrap_or_default();
                if !members.contains(&bot_id) {
                    return Ok(Err(EngineError::Invalid("这个 Bot 不在房间里".into())));
                }
                routine.bot_id = bot_id;
            }
            if let Some(title) = &patch.title {
                match check_title(title) {
                    Ok(title) => routine.title = title,
                    Err(e) => return Ok(Err(e)),
                }
            }
            if let Some(prompt) = &patch.prompt {
                match check_prompt(prompt) {
                    Ok(prompt) => routine.prompt = prompt,
                    Err(e) => return Ok(Err(e)),
                }
            }
            let rescheduled = patch.schedule.is_some();
            if let Some(schedule) = &patch.schedule {
                match check_schedule(schedule) {
                    Ok((schedule, _)) => routine.schedule = schedule,
                    Err(e) => return Ok(Err(e)),
                }
            }
            let resumed = patch.enabled == Some(true) && !routine.enabled;
            routine.enabled = patch.enabled.unwrap_or(routine.enabled);
            if resumed {
                // 人重新启用：之前连着没做成的不算了。
                routine.failures = 0;
            }
            if !routine.enabled {
                routine.next_at = None;
            } else if rescheduled || resumed || routine.next_at.is_none() {
                routine.next_at = next_after(&routine.schedule, now);
            }
            routine.updated_at = now;
            routines::update(tx, &routine)?;
            let seq = updated(tx, &routine, now)?;
            Ok(Ok((routine, seq)))
        })??;
        self.publish(seq);
        let state = if routine.enabled { "启用" } else { "停用" };
        let summary = format!(
            "改了定时任务「{}」（{}，{state}）",
            routine.title, routine.schedule
        );
        self.audit(
            Audit::user("routine.updated", summary)
                .room(&routine.room_id)
                .detail(&routine.prompt),
        );
        Ok(routine)
    }

    pub fn delete_routine(&self, id: &RoutineId) -> Result<()> {
        let now = now_ms();
        let seq = self.store.write(|tx| {
            let Some(routine) = routines::get(tx, id)? else {
                return Ok(Err(EngineError::NotFound("没有这个定时任务".into())));
            };
            routines::delete(tx, id)?;
            let gone = RoutineRef {
                id: id.clone(),
                room_id: routine.room_id.clone(),
            };
            let seq = events::append(
                tx,
                Some(&routine.room_id),
                EventKind::RoutineDeleted,
                &to_json(&gone),
                now,
            )?;
            Ok(Ok((seq, routine)))
        })??;
        let (seq, routine) = seq;
        self.publish(seq);
        let summary = format!("删掉了定时任务「{}」", routine.title);
        self.audit(Audit::user("routine.deleted", summary).room(&routine.room_id));
        Ok(())
    }

    pub fn get_routine(&self, id: &RoutineId) -> Result<Routine> {
        self.store
            .read(|c| routines::get(c, id))?
            .ok_or_else(|| EngineError::NotFound("没有这个定时任务".into()))
    }

    pub fn list_routines(&self) -> Result<Vec<Routine>> {
        Ok(self.store.read(routines::list)?)
    }

    /// 已经到点、没有被别人占着的定时任务。
    pub fn due_routines(&self, now: Millis) -> Result<Vec<Routine>> {
        Ok(self.store.read(|c| routines::due(c, now))?)
    }

    /// 把下一次到点的时间改成 `next_at`（之后仍然按原来的时间表走）。
    pub fn reschedule_routine(&self, id: &RoutineId, next_at: Millis) -> Result<Routine> {
        let now = now_ms();
        let (routine, seq) = self.store.write(|tx| {
            let Some(mut routine) = routines::get(tx, id)? else {
                return Ok(Err(EngineError::NotFound("没有这个定时任务".into())));
            };
            if !routine.enabled {
                return Ok(Err(EngineError::Invalid("这个定时任务停用着".into())));
            }
            routine.next_at = Some(next_at);
            routine.updated_at = now;
            routines::update(tx, &routine)?;
            let seq = updated(tx, &routine, now)?;
            Ok(Ok((routine, seq)))
        })??;
        self.publish(seq);
        Ok(routine)
    }

    /// 抢这个定时任务的租约，抢到了才能去做。租约到 `until` 过期。
    pub fn claim_routine(&self, id: &RoutineId, owner: &str, until: Millis) -> Result<bool> {
        let now = now_ms();
        Ok(self
            .store
            .write(|tx| routines::claim(tx, id, owner, now, until))?)
    }

    /// 到点了：把存好的那段话放进房间，交给负责的 Bot。只有占着租约的人能放。
    /// 这条消息不是人刚说的，界面上不当成人的发言显示。
    pub fn post_routine_message(&self, id: &RoutineId, owner: &str) -> Result<Message> {
        let now = now_ms();
        let (message, seq) = self.store.write(|tx| {
            let Some(routine) = routines::get(tx, id)? else {
                return Ok(Err(EngineError::NotFound("没有这个定时任务".into())));
            };
            if routines::lease(tx, id, now)?.as_deref() != Some(owner) {
                return Ok(Err(EngineError::Invalid(
                    "这个定时任务正由别的进程在做".into(),
                )));
            }
            let message = Message {
                id: MessageId::new(),
                seq: 0,
                room_id: routine.room_id.clone(),
                sender: Sender::User,
                content: format!("{}\n\n{}", routine.title, routine.prompt),
                mentions: vec![routine.bot_id.clone()],
                hop: 0,
                kind: MessageKind::Routine,
                task_id: None,
                run_id: None,
                origin: None,
                steps: vec![],
                state: MessageState::Complete,
                error: None,
                rev: 0,
                created_at: now,
                updated_at: now,
            };
            Ok(Ok(add_message(tx, message)?))
        })??;
        self.publish(seq);
        Ok(message)
    }

    /// 这一次做完了（或者没做）：记下结果、排好下一次、还掉租约。
    /// 连续 [`MAX_ROUTINE_FAILURES`] 次没做成的自动停用。租约已经被别人接手时不改，返回 `None`。
    pub fn finish_routine(
        &self,
        id: &RoutineId,
        owner: &str,
        result: RoutineResult,
        note: &str,
    ) -> Result<Option<Routine>> {
        let now = now_ms();
        let done = self.store.write(|tx| {
            if !routines::release(tx, id, owner)? {
                return Ok(None);
            }
            let Some(mut routine) = routines::get(tx, id)? else {
                return Ok(None);
            };
            routine.last_at = Some(now);
            routine.last_result = Some(result);
            routine.last_note = note.to_string();
            routine.failures = match result {
                RoutineResult::Spoke | RoutineResult::Silent => 0,
                RoutineResult::Failed => routine.failures + 1,
                RoutineResult::Skipped => routine.failures,
            };
            let was_enabled = routine.enabled;
            if routine.failures >= MAX_ROUTINE_FAILURES {
                routine.enabled = false;
            }
            let gave_up = was_enabled && !routine.enabled;
            // 人在这期间停用了它：不再排下一次。
            routine.next_at = routine
                .enabled
                .then(|| next_after(&routine.schedule, now))
                .flatten();
            routine.updated_at = now;
            routines::update(tx, &routine)?;
            let seq = updated(tx, &routine, now)?;
            Ok(Some((routine, seq, gave_up)))
        })?;
        Ok(done.map(|(routine, seq, gave_up)| {
            self.publish(seq);
            if gave_up {
                let summary = format!(
                    "定时任务「{}」连续 {MAX_ROUTINE_FAILURES} 次没做成，自动停用了",
                    routine.title
                );
                self.audit(
                    Audit::system("routine.disabled", summary)
                        .room(&routine.room_id)
                        .detail(&routine.last_note),
                );
            }
            routine
        }))
    }
}

fn updated(tx: &sa_store::Connection, routine: &Routine, now: Millis) -> sa_store::Result<i64> {
    events::append(
        tx,
        Some(&routine.room_id),
        EventKind::RoutineUpdated,
        &to_json(routine),
        now,
    )
}

fn next_after(schedule: &str, now: Millis) -> Option<Millis> {
    Schedule::parse(schedule).ok()?.next_after(now)
}

fn check_title(title: &str) -> Result<String> {
    let title = title.trim();
    if title.is_empty() {
        return Err(EngineError::Invalid("定时任务要有个名字".into()));
    }
    if title.chars().count() > MAX_ROUTINE_TITLE_CHARS {
        return Err(EngineError::Invalid(format!(
            "名字不能超过 {MAX_ROUTINE_TITLE_CHARS} 个字"
        )));
    }
    Ok(title.to_string())
}

fn check_prompt(prompt: &str) -> Result<String> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Err(EngineError::Invalid("要写清楚到点让 Bot 做什么".into()));
    }
    if prompt.chars().count() > MAX_MESSAGE_CHARS {
        return Err(EngineError::Invalid(format!(
            "内容不能超过 {MAX_MESSAGE_CHARS} 个字"
        )));
    }
    Ok(prompt.to_string())
}

fn check_schedule(schedule: &str) -> Result<(String, Schedule)> {
    let schedule = schedule.split_whitespace().collect::<Vec<_>>().join(" ");
    let parsed = Schedule::parse(&schedule).map_err(EngineError::Invalid)?;
    if parsed.next_after(now_ms()).is_none() {
        return Err(EngineError::Invalid(
            "这个时间永远到不了（比如 2 月 30 日）".into(),
        ));
    }
    Ok((schedule, parsed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sa_protocol::ModelTier;
    use sa_store::Store;

    fn engine() -> (Engine, RoomId, BotId) {
        let engine = Engine::new(Store::open_in_memory().unwrap()).unwrap();
        engine.ensure_defaults().unwrap();
        let room = engine.list_rooms().unwrap().remove(0);
        let bot = room.host_bot_id.clone().unwrap();
        (engine, room.id, bot)
    }

    #[test]
    fn create_checks_its_input_and_schedules_the_first_time() {
        let (engine, room, bot) = engine();
        let routine = engine
            .create_routine(&room, None, " 早报 ", " 看看新消息 ", "0   9 * * *")
            .unwrap();
        assert_eq!(routine.bot_id, bot);
        assert_eq!(routine.title, "早报");
        assert_eq!(routine.prompt, "看看新消息");
        assert_eq!(routine.schedule, "0 9 * * *");
        assert!(routine.next_at.unwrap() > now_ms());
        assert_eq!(engine.list_routines().unwrap(), vec![routine]);

        let outsider = engine.create_bot("外人", ModelTier::Mid, "").unwrap();
        for (bot, title, prompt, schedule, needle) in [
            (None, "", "做事", "0 9 * * *", "名字"),
            (None, "早报", " ", "0 9 * * *", "做什么"),
            (None, "早报", "做事", "9点", "五段"),
            (None, "早报", "做事", "0 0 30 2 *", "永远到不了"),
            (
                Some(&outsider.id),
                "早报",
                "做事",
                "0 9 * * *",
                "不在房间里",
            ),
        ] {
            let error = engine
                .create_routine(&room, bot, title, prompt, schedule)
                .unwrap_err();
            assert!(error.to_string().contains(needle), "{error}");
        }
    }

    #[test]
    fn disabling_clears_the_next_time_and_enabling_starts_over() {
        let (engine, room, _) = engine();
        let routine = engine
            .create_routine(&room, None, "早报", "做事", "0 9 * * *")
            .unwrap();
        let off = RoutinePatch {
            enabled: Some(false),
            ..Default::default()
        };
        let stopped = engine.update_routine(&routine.id, off).unwrap();
        assert!(!stopped.enabled);
        assert_eq!(stopped.next_at, None);
        assert!(engine.due_routines(i64::MAX).unwrap().is_empty());

        let on = RoutinePatch {
            enabled: Some(true),
            schedule: Some("*/5 * * * *".into()),
            ..Default::default()
        };
        let started = engine.update_routine(&routine.id, on).unwrap();
        assert!(started.next_at.unwrap() <= now_ms() + 5 * 60_000);

        engine.delete_routine(&routine.id).unwrap();
        assert!(engine.get_routine(&routine.id).is_err());
        assert!(engine.delete_routine(&routine.id).is_err());
    }

    #[test]
    fn only_the_lease_owner_posts_and_finishes_and_three_failures_disable() {
        let (engine, room, bot) = engine();
        let routine = engine
            .create_routine(&room, None, "早报", "看看新消息", "0 9 * * *")
            .unwrap();
        let far = now_ms() + 60_000;
        assert!(engine.post_routine_message(&routine.id, "a").is_err());
        assert!(engine.claim_routine(&routine.id, "a", far).unwrap());
        assert!(!engine.claim_routine(&routine.id, "b", far).unwrap());
        assert!(engine.post_routine_message(&routine.id, "b").is_err());

        let message = engine.post_routine_message(&routine.id, "a").unwrap();
        assert_eq!(message.kind, MessageKind::Routine);
        assert_eq!(message.sender, Sender::User);
        assert_eq!(message.mentions, vec![bot]);
        assert_eq!(message.content, "早报\n\n看看新消息");

        // 别人不能替它交差。
        let stolen = engine.finish_routine(&routine.id, "b", RoutineResult::Spoke, "");
        assert_eq!(stolen.unwrap(), None);
        let done = engine
            .finish_routine(&routine.id, "a", RoutineResult::Silent, "")
            .unwrap()
            .unwrap();
        assert_eq!(done.last_result, Some(RoutineResult::Silent));
        assert!(done.last_at.is_some() && done.next_at.is_some());

        // 跳过不算没做成；连着三次没做成就停用。
        for (result, failures, enabled) in [
            (RoutineResult::Failed, 1, true),
            (RoutineResult::Skipped, 1, true),
            (RoutineResult::Failed, 2, true),
            (RoutineResult::Failed, 3, false),
        ] {
            assert!(engine.claim_routine(&routine.id, "a", far).unwrap());
            let done = engine
                .finish_routine(&routine.id, "a", result, "模型报错")
                .unwrap()
                .unwrap();
            assert_eq!((done.failures, done.enabled), (failures, enabled));
            assert_eq!(done.next_at.is_some(), enabled);
            assert_eq!(done.last_note, "模型报错");
        }
        // 人重新启用：从头算。
        let on = RoutinePatch {
            enabled: Some(true),
            ..Default::default()
        };
        let again = engine.update_routine(&routine.id, on).unwrap();
        assert_eq!((again.failures, again.next_at.is_some()), (0, true));
    }
}
