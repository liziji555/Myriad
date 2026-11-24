//! harness 工位的验收：人把 harness 在独立目录里的改动带回工作目录，或者丢掉。

use crate::room::db;
use crate::{share_desk, workdir, Orchestrator, OrchestratorError, Shared};
use sa_engine::{Desk, Workdir};
use sa_protocol::{BotId, HarnessDesk, RoomId, TaskId};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

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
        if let Some(name) = desk.share.clone() {
            return self.close_shared_desk(room, bot, &name, dir, accept).await;
        }
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
            workdir::bring_back(&dir)
                .await
                .map_err(OrchestratorError::Conflict)?;
        }
        // 收掉之后下次重新开目录；harness 的会话跟着旧目录，也不再接着用。
        let (engine, r, b) = (shared.engine.clone(), room.clone(), bot.clone());
        db(move || engine.put_desk(&r, &b, &Desk::default())).await?;
        workdir::remove(&TaskId::from(format!("desk-{bot}")), &dir).await;
        self.room_desks(room).await
    }

    /// 工位开在共享目录那台电脑上：交给那边带回或丢掉。那台电脑没连着时带不回；丢掉的话只是这边不再记着它。
    async fn close_shared_desk(
        &self,
        room: &RoomId,
        bot: &BotId,
        name: &str,
        dir: Workdir,
        accept: bool,
    ) -> Result<Vec<HarnessDesk>, OrchestratorError> {
        let shares = &self.shared.shares;
        if shares.online(name) && shares.shell(name).is_some() {
            let close = share_desk::Close {
                workdir: dir,
                accept,
            };
            let input = serde_json::to_value(close).expect("只有字符串");
            let cancel = CancellationToken::new();
            let asked = shares.ask(name, share_desk::CLOSE, input, &cancel, None);
            match tokio::time::timeout(SHARED_CLOSE_WAIT, asked).await {
                Ok(Ok(_)) => {}
                Ok(Err(reason)) => return Err(OrchestratorError::Conflict(reason)),
                Err(_) => {
                    return Err(OrchestratorError::Conflict(format!(
                        "共享目录「{name}」那台电脑 {} 秒都没做完，过一会儿再看一下",
                        SHARED_CLOSE_WAIT.as_secs()
                    )))
                }
            }
        } else if accept {
            return Err(OrchestratorError::Conflict(format!(
                "它的独立目录在共享目录「{name}」那台电脑上，那台电脑没连着（或者共享时没加 --commands），连上之后再带回"
            )));
        }
        let (engine, r, b) = (self.shared.engine.clone(), room.clone(), bot.clone());
        db(move || engine.put_desk(&r, &b, &Desk::default())).await?;
        self.room_desks(room).await
    }
}

/// 在共享目录那台电脑上带回、丢掉改动最多等多久。
const SHARED_CLOSE_WAIT: std::time::Duration = std::time::Duration::from_secs(120);
