//! harness 工位的验收：人把 harness 在独立目录里的改动带回工作目录，或者丢掉。

use crate::room::db;
use crate::{workdir, Orchestrator, OrchestratorError, Shared};
use sa_engine::Desk;
use sa_protocol::{BotId, HarnessDesk, RoomId, TaskId};
use std::sync::Arc;

pub(crate) type DeskLock = Arc<tokio::sync::Mutex<()>>;

impl Shared {
    pub(crate) fn desk_lock(&self, room: &RoomId, bot: &BotId) -> DeskLock {
        let mut locks = self.desk_locks.lock().unwrap();
        locks
            .entry((room.clone(), bot.clone()))
            .or_default()
            .clone()
    }
}

impl Orchestrator {
    /// 房间里开着独立目录的 harness 工位。
    pub async fn room_desks(&self, room: &RoomId) -> Result<Vec<HarnessDesk>, OrchestratorError> {
        let (engine, room) = (self.shared.engine.clone(), room.clone());
        Ok(db(move || engine.room_desks(&room)).await?)
    }

    /// 把 harness 工位里的改动带回工作目录，然后收掉工位。带不回去时什么都不动。
    pub async fn accept_desk(
        &self,
        room: &RoomId,
        bot: &BotId,
    ) -> Result<Vec<HarnessDesk>, OrchestratorError> {
        self.close_desk(room, bot, true).await
    }

    /// 丢掉 harness 工位里的改动，收掉工位。
    pub async fn discard_desk(
        &self,
        room: &RoomId,
        bot: &BotId,
    ) -> Result<Vec<HarnessDesk>, OrchestratorError> {
        self.close_desk(room, bot, false).await
    }

    async fn close_desk(
        &self,
        room: &RoomId,
        bot: &BotId,
        accept: bool,
    ) -> Result<Vec<HarnessDesk>, OrchestratorError> {
        if self.shared.shutdown.is_cancelled() {
            return Err(OrchestratorError::ShuttingDown);
        }
        let shared = &self.shared;
        let lock = shared.desk_lock(room, bot);
        let Ok(_held) = lock.try_lock() else {
            return Err(OrchestratorError::Conflict(
                "它正在这个目录里干活，等它这一轮做完再来".into(),
            ));
        };
        let (engine, r, b) = (shared.engine.clone(), room.clone(), bot.clone());
        let desk = db(move || engine.desk(&r, &b)).await?;
        let Some(dir) = desk.workdir.clone() else {
            return Err(OrchestratorError::Conflict("它没有开着的独立目录".into()));
        };
        if accept {
            if !std::path::Path::new(&dir.path).is_dir() {
                return Err(OrchestratorError::Conflict(
                    "它的独立目录已经不在了，没有改动可以带回".into(),
                ));
            }
            let real = std::path::Path::new(&dir.source).canonicalize().ok();
            if real.as_deref() != Some(shared.settings.fence.root()) {
                return Err(OrchestratorError::Conflict(format!(
                    "这个目录是从 {} 开出来的，现在的工作目录是 {}，不能带回",
                    dir.source,
                    shared.settings.fence.root_display()
                )));
            }
            workdir::commit_and_diff(&dir, "带回前记下改动")
                .await
                .map_err(OrchestratorError::Conflict)?;
            workdir::apply_to_workspace(&dir)
                .await
                .map_err(OrchestratorError::Conflict)?;
        }
        // 收掉之后下次重新开目录；harness 的会话跟着旧目录，也不再接着用。
        let (engine, r, b) = (shared.engine.clone(), room.clone(), bot.clone());
        db(move || engine.put_desk(&r, &b, &Desk::default())).await?;
        workdir::remove(&TaskId::from(format!("desk-{bot}")), &dir).await;
        self.room_desks(room).await
    }
}
