//! 定时任务：到点把存好的那段话交给负责的 Bot，在房间里做一轮。
//!
//! 定时器只有一个主人：先抢到租约的进程来做这一次，做完还掉；进程中途没了，租约过期后别人才能接手。
//! Bot 没什么可说时可以不出声，房间里什么都不留。

use crate::room::db;
use crate::{budget, Orchestrator, OrchestratorError};
use sa_engine::{now_ms, EngineError, NewNotice, MAX_ROUTINE_FAILURES};
use sa_protocol::{
    MessageKind, MessageState, NoticeKind, NoticeLevel, RoomId, Routine, RoutineId, RoutineResult,
    Sender,
};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// 定时任务多久看一遍、等多久。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routines {
    /// 隔多久看一遍有没有到点的。
    pub every: Duration,
    /// 租约多久过期。要比 `settle` 长。
    pub lease: Duration,
    /// 到点之后过了这么久才轮到（服务当时没开）：这一次不补做。
    pub late: Duration,
    /// 最多等 Bot 做这么久。
    pub settle: Duration,
}

impl Default for Routines {
    fn default() -> Self {
        Self {
            every: Duration::from_secs(30),
            lease: Duration::from_secs(15 * 60),
            late: Duration::from_secs(60 * 60),
            settle: Duration::from_secs(10 * 60),
        }
    }
}

const POLL: Duration = Duration::from_millis(50);
/// 停下一个超时的定时任务之后，最多等这么久让它收尾。
const STOP_GRACE: Duration = Duration::from_secs(10);

impl Orchestrator {
    /// 启动定时器：每隔一阵看一遍，直到服务关闭。
    /// 到点的放到后台去做，不等它做完：一个做得慢的定时任务不耽误别的房间按时开始。
    pub fn start_routines(self: &Arc<Self>) {
        let this = self.clone();
        tokio::spawn(async move {
            let every = this.shared.settings.routines.every;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(every) => {}
                    _ = this.shared.shutdown.cancelled() => return,
                }
                for routine in this.claim_due().await {
                    let this = this.clone();
                    tokio::spawn(async move { this.fire(routine, false).await });
                }
            }
        });
    }

    /// 看一遍：到点的都做掉，等它们做完。返回这一遍处理过的定时任务（做了的和跳过的）。
    /// 同一个房间里到点的不止一个时，这一遍只做一个，剩下的下一遍再做。
    pub async fn routines_tick(&self) -> Vec<Routine> {
        let claimed = self.claim_due().await;
        let fired = claimed.into_iter().map(|routine| self.fire(routine, false));
        let done = futures_util::future::join_all(fired).await;
        done.into_iter().flatten().collect()
    }

    /// 抢下到点的定时任务。一个房间一次只抢一个：租约是从抢到那一刻算的，
    /// 排在后面的要是先抢下来，等前面的做完时租约早就过期了。
    async fn claim_due(&self) -> Vec<Routine> {
        let engine = self.shared.engine.clone();
        let due = match db(move || engine.due_routines(now_ms())).await {
            Ok(due) => due,
            Err(error) => {
                tracing::error!("查不到到点的定时任务：{error}");
                return vec![];
            }
        };
        let mut claimed = vec![];
        for routine in due {
            // 房间里正在对话，或者正在做另一个定时任务：不插话，下一遍再看。
            if self.room_busy(&routine.room_id) {
                continue;
            }
            match self.claim(&routine.id).await {
                Ok(true) => {
                    let mut rooms = self.shared.routine_rooms.lock().unwrap();
                    rooms.insert(routine.room_id.clone());
                    claimed.push(routine);
                }
                Ok(false) => {}
                Err(error) => tracing::error!(routine = %routine.id, "租约没抢成：{error}"),
            }
        }
        claimed
    }

    /// 现在就做一次，不等到点。马上返回，做的过程在后台；结果记在定时任务上。
    pub async fn run_routine_now(
        self: &Arc<Self>,
        id: &RoutineId,
    ) -> Result<Routine, OrchestratorError> {
        if self.shared.shutdown.is_cancelled() {
            return Err(OrchestratorError::ShuttingDown);
        }
        let (engine, routine_id) = (self.shared.engine.clone(), id.clone());
        let routine = db(move || engine.get_routine(&routine_id)).await?;
        if self.room_busy(&routine.room_id) {
            return Err(OrchestratorError::Conflict(
                "房间里正在对话，等它说完再试".into(),
            ));
        }
        if !self.claim(id).await? {
            return Err(OrchestratorError::Conflict(
                "这个定时任务正在做，等它做完再试".into(),
            ));
        }
        {
            let mut rooms = self.shared.routine_rooms.lock().unwrap();
            rooms.insert(routine.room_id.clone());
        }
        let this = self.clone();
        let firing = routine.clone();
        tokio::spawn(async move { this.fire(firing, true).await });
        Ok(routine)
    }

    async fn claim(&self, id: &RoutineId) -> Result<bool, EngineError> {
        let shared = &self.shared;
        let until = now_ms() + shared.settings.routines.lease.as_millis() as i64;
        let (engine, id, owner) = (shared.engine.clone(), id.clone(), shared.owner.clone());
        db(move || engine.claim_routine(&id, &owner, until)).await
    }

    fn room_busy(&self, room_id: &RoomId) -> bool {
        self.shared.routine_rooms.lock().unwrap().contains(room_id) || self.room_active(room_id)
    }

    /// 房间的后台任务是不是正在干活。
    fn room_active(&self, room_id: &RoomId) -> bool {
        let rooms = self.rooms.lock().unwrap();
        rooms
            .get(room_id)
            .is_some_and(|handle| handle.link.active.load(Ordering::SeqCst))
    }

    /// 做一次已经抢到租约的定时任务。`forced`：人点的「现在就做」，不管是不是错过太久。
    async fn fire(&self, routine: Routine, forced: bool) -> Option<Routine> {
        let room_id = routine.room_id.clone();
        let done = self.fire_claimed(routine, forced).await;
        self.shared.routine_rooms.lock().unwrap().remove(&room_id);
        done
    }

    async fn fire_claimed(&self, routine: Routine, forced: bool) -> Option<Routine> {
        let (result, note) = match self.attempt(&routine, forced).await {
            Ok(Some(outcome)) => outcome,
            // 服务正在关闭：不记结果，租约过期之后下次启动再说。
            Ok(None) => return None,
            Err(error) => (RoutineResult::Failed, error.to_string()),
        };
        let shared = &self.shared;
        let (engine, id, owner) = (
            shared.engine.clone(),
            routine.id.clone(),
            shared.owner.clone(),
        );
        let done = match db(move || engine.finish_routine(&id, &owner, result, &note)).await {
            Ok(done) => done?,
            Err(error) => {
                tracing::error!(routine = %routine.id, "定时任务的结果没记上：{error}");
                return None;
            }
        };
        if !done.enabled && done.failures >= MAX_ROUTINE_FAILURES {
            let notice = NewNotice {
                kind: NoticeKind::Failures,
                level: NoticeLevel::Warning,
                room_id: Some(done.room_id.clone()),
                task_id: None,
                text: format!(
                    "定时任务「{}」连续 {} 次没做成，已经停用。最后一次的原因：{}。处理好之后在「定时」里重新启用。",
                    done.title, done.failures, done.last_note
                ),
            };
            let key = format!("routine:{}:{}", done.id, done.last_at.unwrap_or_default());
            let engine = shared.engine.clone();
            if let Err(error) = db(move || engine.notify(&key, notice)).await {
                tracing::error!(routine = %done.id, "提醒没记上：{error}");
            }
        }
        Some(done)
    }

    /// 返回结果和原因。`None`：服务正在关闭，没做完。
    async fn attempt(
        &self,
        routine: &Routine,
        forced: bool,
    ) -> Result<Option<(RoutineResult, String)>, EngineError> {
        let shared = &self.shared;
        let settings = &shared.settings.routines;
        let skipped = |note: String| Ok(Some((RoutineResult::Skipped, note)));

        let late = now_ms() - routine.next_at.unwrap_or(i64::MAX);
        if !forced && late > settings.late.as_millis() as i64 {
            return skipped(format!(
                "到点的时候服务没在运行，晚了 {} 分钟，这一次不补做",
                late / 60_000
            ));
        }
        let (engine, room_id, bot_id) = (
            shared.engine.clone(),
            routine.room_id.clone(),
            routine.bot_id.clone(),
        );
        let present = db(move || {
            let room = engine.get_room(&room_id)?;
            let bot = engine.list_bots()?.into_iter().find(|b| b.id == bot_id);
            Ok(bot.is_some_and(|bot| bot.enabled && room.member_bot_ids.contains(&bot.id)))
        })
        .await?;
        if !present {
            return skipped("负责的 Bot 不在房间里或者被停用了".into());
        }
        if budget::daily_exhausted(shared).await {
            return skipped("今天的花费到上限了".into());
        }

        let (engine, id, owner) = (
            shared.engine.clone(),
            routine.id.clone(),
            shared.owner.clone(),
        );
        let posted = db(move || engine.post_routine_message(&id, &owner)).await?;
        self.wake(&routine.room_id);

        // 等房间把这条消息接过去，再等它做完。
        let deadline = tokio::time::Instant::now() + settings.settle;
        let finished = loop {
            let taken = {
                let rooms = self.rooms.lock().unwrap();
                rooms.get(&routine.room_id).is_some_and(|handle| {
                    handle.link.handled.load(Ordering::SeqCst) >= posted.seq
                        && !handle.link.active.load(Ordering::SeqCst)
                })
            };
            if taken {
                break true;
            }
            if tokio::time::Instant::now() >= deadline {
                break false;
            }
            tokio::select! {
                _ = tokio::time::sleep(POLL) => {}
                _ = shared.shutdown.cancelled() => return Ok(None),
            }
        };

        if !finished {
            // 到时间了还没做完：停下它。不然这一次已经记成「没做成」、租约也还了，Bot 却还在接着动手。
            self.stop(&routine.room_id);
            let deadline = tokio::time::Instant::now() + STOP_GRACE;
            while self.room_active(&routine.room_id) && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(POLL).await;
            }
        }

        let (engine, room_id) = (shared.engine.clone(), routine.room_id.clone());
        let after = db(move || engine.messages_after(&room_id, posted.seq)).await?;
        let reply = after.iter().find(|message| {
            message.kind == MessageKind::Chat
                && matches!(&message.sender, Sender::Bot { bot_id } if bot_id == &routine.bot_id)
        });
        Ok(Some(match reply {
            Some(reply) if reply.state == MessageState::Complete => {
                (RoutineResult::Spoke, String::new())
            }
            Some(reply) if reply.state == MessageState::Streaming || !finished => (
                RoutineResult::Failed,
                format!(
                    "等了 {} 分钟还没做完，已经停下",
                    settings.settle.as_secs() / 60
                ),
            ),
            Some(reply) => (
                RoutineResult::Failed,
                match reply.state {
                    MessageState::Cancelled => "被停下了".to_string(),
                    _ => reply.error.clone().unwrap_or_else(|| "没做完".into()),
                },
            ),
            None if !finished => (
                RoutineResult::Failed,
                format!(
                    "等了 {} 分钟还没做完，已经停下",
                    settings.settle.as_secs() / 60
                ),
            ),
            None => (RoutineResult::Silent, String::new()),
        }))
    }
}
