//! harness 工位的验收：人把 harness 在独立目录里的改动带回工作目录，或者丢掉。

use crate::room::db;
use crate::{share_desk, workdir, Orchestrator, OrchestratorError, Shared};
use sa_engine::{Desk, Workdir};
use sa_protocol::{
    BotId, CheckpointInfo, DeskDiff, HarnessDesk, HarnessProbe, RewindReport, RoomId, SessionId,
    TaskId, WorkspaceEntry, WorkspaceFile,
};
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

    /// 探测本机已安装的外部 Agent CLI（claude / codex / opencode / gemini / kimi / cursor-agent）。
    pub async fn probe_harnesses(&self) -> Vec<HarnessProbe> {
        crate::harness::probe_harnesses(self.shared.settings.fence.root()).await
    }

    /// 列出房间中内置 Bot 产生的文件检查点（最新在前）。
    pub async fn list_checkpoints(
        &self,
        room: &RoomId,
    ) -> Result<Vec<CheckpointInfo>, OrchestratorError> {
        let (engine, r) = (self.shared.engine.clone(), room.clone());
        db(move || engine.get_room(&r)).await?;
        let Some(store) = self.shared.checkpoints.lock().unwrap().clone() else {
            return Ok(vec![]);
        };
        let room_key = room.to_string();
        let fence = self.shared.settings.fence.clone();
        let records = tokio::task::spawn_blocking(move || store.list(&room_key, &fence))
            .await
            .map_err(|e| OrchestratorError::Internal(e.to_string()))?;
        Ok(records
            .into_iter()
            .map(|r| CheckpointInfo {
                id: r.id,
                bot_name: r.bot_name,
                created_at: r.created_at,
                files: r.files.into_iter().map(|f| f.path).collect(),
            })
            .collect())
    }

    /// 将指定检查点所改动的全部文件恢复到该轮对话之前的快照状态。
    pub async fn rewind_checkpoint(
        &self,
        room: &RoomId,
        checkpoint_id: &str,
        force: bool,
    ) -> Result<RewindReport, OrchestratorError> {
        if self.shared.shutdown.is_cancelled() {
            return Err(OrchestratorError::ShuttingDown);
        }
        let (engine, r) = (self.shared.engine.clone(), room.clone());
        let room_meta = db(move || engine.get_room(&r)).await?;
        if room_meta.share.is_some() {
            return Err(OrchestratorError::Conflict(
                "共享目录房间暂不支持本地检查点回滚".into(),
            ));
        }
        let Some(store) = self.shared.checkpoints.lock().unwrap().clone() else {
            return Err(OrchestratorError::Conflict("检查点功能未启用".into()));
        };
        // 与任务/工位验收共用同一把写入锁，避免并发合并与回滚互相踩踏。
        let _one_at_a_time = self.shared.works.accepting.lock().await;
        let room_key = room.to_string();
        let cp_id = checkpoint_id.to_string();
        let fence = self.shared.settings.fence.clone();
        let outcome =
            tokio::task::spawn_blocking(move || store.rewind(&room_key, &cp_id, &fence, force))
                .await
                .map_err(|e| OrchestratorError::Internal(e.to_string()))?
                .map_err(OrchestratorError::Conflict)?;
        let summary = format!(
            "回滚检查点 {}（恢复 {} 个、删除 {} 个新建文件）",
            outcome.checkpoint_id,
            outcome.restored.len(),
            outcome.removed.len()
        );
        self.shared
            .engine
            .audit(sa_engine::Audit::user("checkpoint.rewound", summary).room(room));
        Ok(RewindReport {
            checkpoint_id: outcome.checkpoint_id,
            restored: outcome.restored,
            removed: outcome.removed,
        })
    }

    /// 房间里开着独立目录的 harness 工位。
    pub async fn room_desks(&self, room: &RoomId) -> Result<Vec<HarnessDesk>, OrchestratorError> {
        let (engine, room) = (self.shared.engine.clone(), room.clone());
        Ok(db(move || engine.room_desks(&room)).await?)
    }

    /// 查看 harness 独立目录里单个文件的 Unified Diff。
    pub async fn desk_diff(
        &self,
        room: &RoomId,
        bot: &BotId,
        path: &str,
    ) -> Result<DeskDiff, OrchestratorError> {
        let (engine, r, b) = (self.shared.engine.clone(), room.clone(), bot.clone());
        let desk = db(move || engine.desk(&r, &b)).await?;
        if desk.share.is_some() {
            return Err(OrchestratorError::Conflict(
                "共享目录那台电脑上的工位暂不支持在线预览 Diff".into(),
            ));
        }
        let Some(dir) = desk.workdir else {
            return Err(OrchestratorError::Conflict("它没有开着的独立目录".into()));
        };
        workdir::diff_file(&dir, path)
            .await
            .map_err(OrchestratorError::Conflict)
    }

    /// 只清空 harness 的会话号（重新开始上下文），保留独立目录和里面的全部改动。
    pub async fn reset_desk_session(
        &self,
        room: &RoomId,
        bot: &BotId,
    ) -> Result<(), OrchestratorError> {
        let lock = self.shared.desk_lock(room, bot);
        let Ok(_held) = lock.try_lock() else {
            return Err(OrchestratorError::Conflict(
                "它正在干活，等这一轮做完再重置会话".into(),
            ));
        };
        let (engine, r, b) = (self.shared.engine.clone(), room.clone(), bot.clone());
        db(move || {
            let mut desk = engine.desk(&r, &b)?;
            desk.session = None;
            engine.put_desk(&r, &b, &desk)
        })
        .await?;
        let scope = crate::harness::acp_scope(room, bot);
        self.shared.acp_pool.evict_scope(&scope);
        self.shared.codex_pool.evict_scope(&scope);
        Ok(())
    }

    /// 列出房间工作目录（或某个 harness 独立工位）下的文件与子目录。
    pub async fn list_workspace_files(
        &self,
        room: &RoomId,
        rel_dir: &str,
        desk_bot: Option<&BotId>,
    ) -> Result<Vec<WorkspaceEntry>, OrchestratorError> {
        let root = self.resolve_browse_root(room, desk_bot).await?;
        let rel = rel_dir.trim().to_string();
        tokio::task::spawn_blocking(move || list_entries_under(&root, &rel))
            .await
            .map_err(|e| OrchestratorError::Internal(e.to_string()))?
            .map_err(OrchestratorError::Conflict)
    }

    /// 读取房间工作目录（或某个 harness 独立工位）里的单个文本文件（上限 512 KB）。
    pub async fn read_workspace_file(
        &self,
        room: &RoomId,
        rel_path: &str,
        desk_bot: Option<&BotId>,
    ) -> Result<WorkspaceFile, OrchestratorError> {
        let root = self.resolve_browse_root(room, desk_bot).await?;
        let rel = rel_path.trim().to_string();
        tokio::task::spawn_blocking(move || read_file_under(&root, &rel))
            .await
            .map_err(|e| OrchestratorError::Internal(e.to_string()))?
            .map_err(OrchestratorError::Conflict)
    }

    async fn resolve_browse_root(
        &self,
        room: &RoomId,
        desk_bot: Option<&BotId>,
    ) -> Result<PathBuf, OrchestratorError> {
        let (engine, r) = (self.shared.engine.clone(), room.clone());
        let bot = desk_bot.cloned();
        let found = db(move || {
            let room = engine.get_room(&r)?;
            if let Some(bot_id) = bot {
                let desk = engine.desk(&r, &bot_id)?;
                if desk.share.is_some() {
                    return Ok(Err("共享目录那台电脑上的工位暂不支持直接浏览文件".into()));
                }
                return Ok(desk
                    .workdir
                    .map(|w| PathBuf::from(w.path))
                    .ok_or_else(|| "它没有开着的独立目录".to_string()));
            }
            if room.share.is_some() {
                return Ok(Err(
                    "这个房间用的是共享目录，请让 Bot 用只读工具查看文件".into()
                ));
            }
            Ok(Ok(PathBuf::new()))
        })
        .await?;
        match found {
            Ok(path) if path.as_os_str().is_empty() => {
                Ok(self.shared.settings.fence.root().to_path_buf())
            }
            Ok(path) => Ok(path),
            Err(reason) => Err(OrchestratorError::Conflict(reason)),
        }
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
        // 收掉之后下次重新开目录；先结束常驻 ACP / Codex 进程释放目录占用，会话也不再接着用。
        let scope = crate::harness::acp_scope(room, bot);
        self.shared.acp_pool.evict_scope(&scope);
        self.shared.codex_pool.evict_scope(&scope);
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

fn list_entries_under(root: &Path, rel: &str) -> Result<Vec<WorkspaceEntry>, String> {
    let fence = sa_tools::Fence::new(root, &[]).map_err(|e| format!("工作目录打不开：{e}"))?;
    let target = if rel.is_empty() || rel == "." {
        fence.root().to_path_buf()
    } else {
        fence.resolve(rel).map_err(|e| e.0)?
    };
    let read = std::fs::read_dir(&target).map_err(|e| format!("列不出目录：{e}"))?;
    let mut entries = Vec::new();
    for item in read.flatten() {
        let name = item.file_name().to_string_lossy().into_owned();
        if matches!(name.as_str(), ".git" | "node_modules" | "target") {
            continue;
        }
        let Ok(meta) = item.metadata() else {
            continue;
        };
        let path = if rel.is_empty() || rel == "." {
            name.clone()
        } else {
            format!("{}/{}", rel.trim_end_matches('/'), name)
        };
        entries.push(WorkspaceEntry {
            name,
            path,
            is_dir: meta.is_dir(),
            size: if meta.is_file() { meta.len() } else { 0 },
        });
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    entries.truncate(500);
    Ok(entries)
}

fn read_file_under(root: &Path, rel: &str) -> Result<WorkspaceFile, String> {
    if rel.is_empty() {
        return Err("没有指定文件路径".into());
    }
    let fence = sa_tools::Fence::new(root, &[]).map_err(|e| format!("工作目录打不开：{e}"))?;
    let target = fence.resolve(rel).map_err(|e| e.0)?;
    let meta = std::fs::metadata(&target).map_err(|e| format!("读不了文件：{e}"))?;
    if !meta.is_file() {
        return Err("这不是一个普通文件".into());
    }
    const MAX_READ_BYTES: usize = 512 * 1024;
    use std::io::Read;
    let mut file = std::fs::File::open(&target).map_err(|e| format!("打不开文件：{e}"))?;
    let mut buf = Vec::with_capacity((meta.len() as usize).min(MAX_READ_BYTES + 1));
    file.by_ref()
        .take((MAX_READ_BYTES + 1) as u64)
        .read_to_end(&mut buf)
        .map_err(|e| format!("读文件出错：{e}"))?;
    let truncated = buf.len() > MAX_READ_BYTES;
    if truncated {
        buf.truncate(MAX_READ_BYTES);
    }
    let binary = buf.iter().take(8192).any(|&b| b == 0);
    let content = if binary {
        String::new()
    } else {
        String::from_utf8_lossy(&buf).into_owned()
    };
    Ok(WorkspaceFile {
        path: rel.to_string(),
        binary,
        truncated,
        content,
    })
}
