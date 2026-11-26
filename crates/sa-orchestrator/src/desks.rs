//! harness 工位的验收：人把 harness 在独立目录里的改动带回工作目录，或者丢掉。

use crate::room::db;
use crate::{share_desk, workdir, Orchestrator, OrchestratorError, Shared};
use sa_engine::{Desk, Workdir};
use sa_protocol::{BotId, HarnessDesk, RoomId, SessionId, TaskId};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tokio_util::sync::CancellationToken;

pub(crate) type DeskLock = Arc<tokio::sync::Mutex<()>>;

/// 会话记录文件现在的样子：长度或修改时间变了就要重新读。
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
}

/// 「上下文用了多少」的缓存。会话记录只往后追加、可能很大：没变就不再从头读一遍。
#[derive(Default)]
pub(crate) struct ContextCache {
    /// 每个会话上一轮带的系统提示词和工具说明有多大。重启之后、说第一句之前不知道，按 0 算。
    fixed: Mutex<HashMap<SessionId, usize>>,
    sizes: Mutex<HashMap<PathBuf, (FileStamp, usize)>>,
}

impl ContextCache {
    pub(crate) fn note_fixed(&self, session: &SessionId, size: usize) {
        let mut fixed = self.fixed.lock().unwrap_or_else(|e| e.into_inner());
        fixed.insert(session.clone(), size);
    }

    fn fixed(&self, session: &SessionId) -> usize {
        let fixed = self.fixed.lock().unwrap_or_else(|e| e.into_inner());
        fixed.get(session).copied().unwrap_or(0)
    }

    /// 会话记录里现在发给模型的对话有多大。文件没变时用上次算好的。
    fn transcript_size(&self, path: &Path) -> Result<usize, String> {
        let stamp = match std::fs::metadata(path) {
            Ok(meta) => FileStamp {
                len: meta.len(),
                modified: meta.modified().ok(),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(format!("读不了 {}：{error}", path.display())),
        };
        {
            let sizes = self.sizes.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((seen, size)) = sizes.get(path) {
                if *seen == stamp {
                    return Ok(*size);
                }
            }
        }
        // 读的时候文件可能又长了：记下的是读之前的样子，下次一比对不上就会重读，不会把旧数当新的。
        let size = sa_kernel::Transcript::context_size(path).map_err(|error| error.to_string())?;
        let mut sizes = self.sizes.lock().unwrap_or_else(|e| e.into_inner());
        sizes.insert(path.to_path_buf(), (stamp, size));
        Ok(size)
    }
}

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
    /// 入口 Bot 在这个房间里的对话占了上下文窗口多少。
    ///
    /// 没有入口 Bot、它被停用了、它交给 harness 干、或者它的模型现在用不了时是 `None`。
    /// 只读：没说过话的房间不会因为看了一眼就多出一个会话。
    pub async fn room_context(
        &self,
        room: &RoomId,
    ) -> Result<Option<sa_protocol::ContextUsage>, OrchestratorError> {
        let (engine, room) = (self.shared.engine.clone(), room.clone());
        let found = db(move || {
            let Some(host) = engine.get_room(&room)?.host_bot_id else {
                return Ok(None);
            };
            let bot = engine.get_bot(&host)?;
            if !bot.enabled || bot.harness != sa_protocol::Harness::Builtin {
                return Ok(None);
            }
            let session = engine.front_session(&room, &host)?;
            Ok(Some((bot.tier, session)))
        })
        .await?;
        let Some((tier, session)) = found else {
            return Ok(None);
        };
        let Ok(model) = self.shared.models.resolve(tier) else {
            return Ok(None);
        };
        let window = model
            .context_window
            .unwrap_or_else(|| model.provider.capabilities(&model.model).context_window as usize);
        let used = match session {
            None => 0,
            Some(session) => {
                let path = self
                    .shared
                    .settings
                    .sessions_dir
                    .join(format!("{session}.jsonl"));
                let shared = self.shared.clone();
                let talk =
                    tokio::task::spawn_blocking(move || shared.context.transcript_size(&path))
                        .await
                        .map_err(|error| OrchestratorError::Internal(error.to_string()))?
                        .map_err(OrchestratorError::Internal)?;
                talk + self.shared.context.fixed(&session)
            }
        };
        let clamp = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        Ok(Some(sa_protocol::ContextUsage {
            used: clamp(used),
            compact_at: clamp(sa_kernel::compact_threshold(window)),
            window: clamp(window),
        }))
    }

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
            // 与 accept_task 共用同一把工作目录合并锁，避免并发 git merge / apply 互相破坏。
            let _one_at_a_time = shared.works.accepting.lock().await;
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
