//! 对话轮次级文件检查点（Checkpoint & Rewind）：
//! 在内置 Bot 每次直接修改工作目录文件（`write` / `edit` / `multi_edit`）前，
//! 以内容寻址（SHA-256 Blob）保存改动前的原始字节，改动后记下目标哈希。
//! 用户可随时按轮次回滚到任意检查点，若文件在之后被外部改过则默认拒绝覆盖并列出冲突，避免误伤人工改动。

use crate::write::{file_lock, write_atomically};
use crate::Fence;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const MAX_CHECKPOINTS_PER_ROOM: usize = 50;
const MAX_SNAPSHOT_BYTES: u64 = 20 * 1024 * 1024;

/// 一个被某轮对话改动过的文件的前后快照。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSnapshot {
    /// 相对工作目录的路径（正斜杠）。
    pub path: String,
    /// 改动前的 SHA-256；`None` 表示该文件是这一轮新建的（回滚时删除）。
    pub before_sha256: Option<String>,
    /// 改动后的 SHA-256（用于回滚前检测是否发生过外部二次修改）。
    pub after_sha256: String,
}

/// 一轮对话产生的检查点记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointRecord {
    pub id: String,
    pub room_id: String,
    pub bot_name: String,
    pub created_at: i64,
    pub files: Vec<FileSnapshot>,
}

/// 回滚执行结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindOutcome {
    pub checkpoint_id: String,
    pub restored: Vec<String>,
    pub removed: Vec<String>,
}

/// 检查点存储（位于 `<data_dir>/checkpoints/`）。
#[derive(Debug)]
pub struct CheckpointStore {
    root: PathBuf,
    lock: Mutex<()>,
}

impl CheckpointStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            lock: Mutex::new(()),
        }
    }

    fn blobs_dir(&self) -> PathBuf {
        self.root.join("blobs")
    }

    fn room_index_path(&self, room_id: &str) -> PathBuf {
        let safe: String = room_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .collect();
        self.root.join("rooms").join(format!("{safe}.json"))
    }

    /// 为一轮对话创建一个检查点记录器；只有真的改动了文件时才会在结束或写入时落盘。
    pub fn begin_turn(
        self: &Arc<Self>,
        room_id: impl Into<String>,
        checkpoint_id: impl Into<String>,
        bot_name: impl Into<String>,
        created_at: i64,
    ) -> Arc<TurnCheckpoint> {
        Arc::new(TurnCheckpoint {
            store: self.clone(),
            room_id: room_id.into(),
            id: checkpoint_id.into(),
            bot_name: bot_name.into(),
            created_at,
            files: Mutex::new(BTreeMap::new()),
        })
    }

    /// 列出某个房间的所有检查点（最新的在前）。
    pub fn list(&self, room_id: &str) -> Vec<CheckpointRecord> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut records = self.read_room_unlocked(room_id);
        records.reverse();
        records
    }

    /// 回滚指定检查点所做的全部文件改动。
    /// `force == false` 时若检测到文件在检查点之后被外部修改，拒绝覆盖并列出冲突文件。
    pub fn rewind(
        &self,
        room_id: &str,
        checkpoint_id: &str,
        fence: &Fence,
        force: bool,
    ) -> Result<RewindOutcome, String> {
        let _file_guard = file_lock();
        let _store_guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());

        let records = self.read_room_unlocked(room_id);
        let record = records
            .iter()
            .find(|r| r.id == checkpoint_id)
            .ok_or_else(|| format!("找不到检查点 {checkpoint_id}"))?
            .clone();

        // 第一阶段：校验所有目标路径均仍在 Fence 内、未漂移为符号链接且无未预期的外部修改。
        let mut plan: Vec<(String, PathBuf, Option<Vec<u8>>)> = Vec::new();
        let mut conflicts = Vec::new();
        for snap in &record.files {
            let (target, inside) = fence
                .resolve_for_write(&snap.path)
                .map_err(|e| format!("回滚路径 {} 受限：{}", snap.path, e.0))?;
            if !inside {
                return Err(format!("拒绝回滚工作目录之外的路径：{}", snap.path));
            }
            if let Ok(meta) = std::fs::symlink_metadata(&target) {
                if meta.file_type().is_symlink() {
                    return Err(format!("拒绝回滚已被替换为符号链接的路径：{}", snap.path));
                }
            }
            if !force {
                let current_hash = match std::fs::read(&target) {
                    Ok(bytes) => Some(sha256_hex(&bytes)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => return Err(format!("读不了 {}：{e}", snap.path)),
                };
                if current_hash.as_deref() != Some(snap.after_sha256.as_str()) {
                    conflicts.push(snap.path.clone());
                }
            }
            let blob = match &snap.before_sha256 {
                Some(hash) => {
                    let blob_path = self.blobs_dir().join(hash);
                    let bytes = std::fs::read(&blob_path)
                        .map_err(|e| format!("找不到 {} 的历史快照 Blob：{e}", snap.path))?;
                    Some(bytes)
                }
                None => None,
            };
            plan.push((snap.path.clone(), target, blob));
        }

        if !conflicts.is_empty() {
            return Err(format!(
                "以下文件在检查点之后被外部修改过，已取消回滚：{}。确认覆盖请传入 force=true。",
                conflicts.join("、")
            ));
        }

        // 第二阶段：逐个恢复旧内容或删除该轮新建的文件。
        let mut restored = Vec::new();
        let mut removed = Vec::new();
        for (rel, target, maybe_bytes) in plan {
            match maybe_bytes {
                Some(bytes) => {
                    write_atomically(&target, &bytes)
                        .map_err(|e| format!("恢复 {rel} 失败：{e}"))?;
                    restored.push(rel);
                }
                None => {
                    if target.exists() {
                        std::fs::remove_file(&target)
                            .map_err(|e| format!("删除新建文件 {rel} 失败：{e}"))?;
                    }
                    removed.push(rel);
                }
            }
        }

        Ok(RewindOutcome {
            checkpoint_id: record.id,
            restored,
            removed,
        })
    }

    fn save_blob(&self, bytes: &[u8]) -> Result<String, String> {
        let hash = sha256_hex(bytes);
        let path = self.blobs_dir().join(&hash);
        if path.exists() {
            return Ok(hash);
        }
        std::fs::create_dir_all(self.blobs_dir())
            .map_err(|e| format!("创建检查点目录失败：{e}"))?;
        write_atomically(&path, bytes).map_err(|e| format!("保存快照 Blob 失败：{e}"))?;
        Ok(hash)
    }

    fn upsert_record(&self, record: CheckpointRecord) {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let room_id = record.room_id.clone();
        let mut records = self.read_room_unlocked(&room_id);
        if let Some(existing) = records.iter_mut().find(|r| r.id == record.id) {
            *existing = record;
        } else {
            records.push(record);
            if records.len() > MAX_CHECKPOINTS_PER_ROOM {
                let drain = records.len() - MAX_CHECKPOINTS_PER_ROOM;
                records.drain(..drain);
            }
        }
        let path = self.room_index_path(&room_id);
        if let Ok(bytes) = serde_json::to_vec_pretty(&records) {
            let _ = write_atomically(&path, &bytes);
        }
    }

    fn read_room_unlocked(&self, room_id: &str) -> Vec<CheckpointRecord> {
        let path = self.room_index_path(room_id);
        std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Vec<CheckpointRecord>>(&bytes).ok())
            .unwrap_or_default()
    }
}

/// 绑定到单轮对话上下文（`ToolCtx`）的文件检查点录制器。
#[derive(Debug)]
pub struct TurnCheckpoint {
    store: Arc<CheckpointStore>,
    room_id: String,
    id: String,
    bot_name: String,
    created_at: i64,
    files: Mutex<BTreeMap<String, FileSnapshot>>,
}

impl TurnCheckpoint {
    /// 在 `file_lock()` 下调用：先快照改动前的原始字节，写入完成后更新 `after_sha256` 并落盘索引。
    pub(crate) fn record_write(
        &self,
        fence: &Fence,
        target: &Path,
        before_bytes: Option<&[u8]>,
        after_bytes: &[u8],
    ) {
        // 只为工作目录内的文件建检查点（工作目录外的写入走显式审批）。
        let Ok(rel_path) = target.strip_prefix(fence.root()) else {
            return;
        };
        let rel = rel_path.to_string_lossy().replace('\\', "/");
        if rel.is_empty() {
            return;
        }
        let after_hash = sha256_hex(after_bytes);
        let mut map = self.files.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = map.get_mut(&rel) {
            // 同一轮里多次改同一个文件：保留本轮最开始的 before_sha256，只刷新最新 after_sha256。
            existing.after_sha256 = after_hash;
        } else {
            let before_hash = match before_bytes {
                Some(bytes) if (bytes.len() as u64) <= MAX_SNAPSHOT_BYTES => {
                    match self.store.save_blob(bytes) {
                        Ok(h) => Some(h),
                        Err(_) => return,
                    }
                }
                Some(_) => return,
                None => None,
            };
            map.insert(
                rel.clone(),
                FileSnapshot {
                    path: rel,
                    before_sha256: before_hash,
                    after_sha256: after_hash,
                },
            );
        }
        let snapshot_list: Vec<FileSnapshot> = map.values().cloned().collect();
        drop(map);
        self.store.upsert_record(CheckpointRecord {
            id: self.id.clone(),
            room_id: self.room_id.clone(),
            bot_name: self.bot_name.clone(),
            created_at: self.created_at,
            files: snapshot_list,
        });
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EditTool, MultiEditTool, Tool, ToolCtx, WriteTool};
    use serde_json::json;
    use std::fs;

    #[tokio::test]
    async fn snapshots_and_rewinds_edited_and_created_files_with_conflict_guard() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        let data = dir.path().join("data");
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("app.rs"), "fn main() { let x = 1; }\n").unwrap();

        let fence = Fence::new(&work, &[]).unwrap();
        let store = Arc::new(CheckpointStore::new(data.join("checkpoints")));
        let cp = store.begin_turn("room_1", "msg_1", "助手", 1000);
        let mut ctx = ToolCtx::new(fence.clone());
        ctx.checkpoint = Some(cp);

        // 同一轮里改两次 app.rs 并新建 new.txt。
        EditTool
            .call(
                json!({"path": "app.rs", "old_string": "let x = 1;", "new_string": "let x = 2;"}),
                &ctx,
            )
            .await
            .unwrap();
        MultiEditTool
            .call(
                json!({
                    "path": "app.rs",
                    "edits": [{"old_string": "let x = 2;", "new_string": "let x = 99;"}]
                }),
                &ctx,
            )
            .await
            .unwrap();
        WriteTool
            .call(json!({"path": "new.txt", "content": "hello\n"}), &ctx)
            .await
            .unwrap();

        let list = store.list("room_1");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].files.len(), 2);

        // 模拟外部修改冲突检测。
        fs::write(work.join("app.rs"), "external change\n").unwrap();
        let conflict = store.rewind("room_1", "msg_1", &fence, false).unwrap_err();
        assert!(conflict.contains("app.rs"), "{conflict}");

        // force=true 强制回滚：app.rs 回到最初始的 x = 1，new.txt 被删除。
        let report = store.rewind("room_1", "msg_1", &fence, true).unwrap();
        assert_eq!(report.restored, vec!["app.rs"]);
        assert_eq!(report.removed, vec!["new.txt"]);
        assert_eq!(
            fs::read_to_string(work.join("app.rs")).unwrap(),
            "fn main() { let x = 1; }\n"
        );
        assert!(!work.join("new.txt").exists());
    }
}
