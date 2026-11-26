//! 对话轮次级文件检查点（Checkpoint & Rewind）：
//! 在内置 Bot 每次直接修改工作目录文件（`write` / `edit` / `multi_edit`）前，
//! 以内容寻址（SHA-256 Blob，Unix 下强制 `0o600` 权限）保存改动前的原始字节，改动后记下目标哈希与规范化工作目录身份。
//! 用户可随时按轮次回滚到任意检查点：
//! - 严格校验工作目录身份（切换项目后即使 `force=true` 也拒绝误回滚）；
//! - 逐段检查相对路径组件，拒绝任何符号链接与目录替换；
//! - 检测轮次内插队改动和轮次后外部改动（非 `force` 拒绝覆盖）；
//! - 两阶段校验 + 失败补偿回退，避免部分写入残留；
//! - 索引过期淘汰时自动回收无引用的孤立 Blob。

use crate::write::file_lock;
use crate::Fence;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

const MAX_CHECKPOINTS_PER_ROOM: usize = 50;
const MAX_SNAPSHOT_BYTES: u64 = 20 * 1024 * 1024;

/// 写入前目标文件的原始状态。
pub(crate) enum BeforeState<'a> {
    /// 文件原本不存在（本轮新建）。
    Missing,
    /// 成功读取到原本的完整字节。
    Bytes(&'a [u8]),
    /// 文件原本存在但无法读取（如权限仅写、超大文件等），不可按「新建文件」误删。
    Unreadable,
}

/// 一个被某轮对话改动过的文件的前后快照。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSnapshot {
    /// 相对工作目录的路径（正斜杠）。
    pub path: String,
    /// 改动前的 SHA-256；`None` 且 `!unreadable` 表示该文件是这一轮新建的（回滚时删除）。
    pub before_sha256: Option<String>,
    /// 改动后的 SHA-256（用于回滚前检测是否发生过外部二次修改）。
    pub after_sha256: String,
    /// 改动后的字节长度（用于 O(1) 快速冲突短路，避免扫描超大外部改动文件）。
    #[serde(default)]
    pub after_size: Option<u64>,
    /// 文件原本存在但未能捕获原内容：绝不允许按新建文件删除。
    #[serde(default)]
    pub unreadable: bool,
    /// 同一轮两次写入之间发生了外部修改（如用户或其他 Bot 插队编辑）：非 force 回滚应报冲突。
    #[serde(default)]
    pub intervening_conflict: bool,
}

/// 一轮对话产生的检查点记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointRecord {
    pub id: String,
    pub room_id: String,
    /// 规范化后的工作目录根路径，防止切换工作区后把旧检查点误应用到新项目。
    #[serde(default)]
    pub workspace: String,
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

    fn rooms_dir(&self) -> PathBuf {
        self.root.join("rooms")
    }

    fn room_index_path(&self, room_id: &str) -> PathBuf {
        let safe: String = room_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .collect();
        self.rooms_dir().join(format!("{safe}.json"))
    }

    /// 为一轮对话创建一个检查点记录器；只有真的改动了文件时才会在写入时落盘。
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

    /// 列出某个房间在当前工作目录下产生的所有检查点（最新的在前）。
    pub fn list(&self, room_id: &str, fence: &Fence) -> Vec<CheckpointRecord> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let active_ws = fence.root().to_string_lossy();
        let mut records: Vec<CheckpointRecord> = self
            .read_room_unlocked(room_id)
            .into_iter()
            .filter(|r| r.workspace == active_ws.as_ref())
            .collect();
        records.reverse();
        records
    }

    /// 回滚指定检查点所做的全部文件改动。
    /// - 切换工作区后即使 `force == true` 也严格拒绝；
    /// - 逐组件校验符号链接与目录替换，防符号链接劫持与部分回滚残留；
    /// - `force == false` 时若检测到轮次内或轮次后的外部修改，拒绝覆盖并列出冲突文件。
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

        let active_ws = fence.root().to_string_lossy();
        if record.workspace.is_empty() || record.workspace != active_ws.as_ref() {
            return Err(format!(
                "这个检查点来自工作目录 {}，当前工作目录是 {}，不能跨项目回滚。",
                if record.workspace.is_empty() {
                    "（未知目录）"
                } else {
                    record.workspace.as_str()
                },
                fence.root_display()
            ));
        }

        // 第一阶段：严格预检每个路径的符号链接、文件类型、外部冲突与 Blob 可用性。
        // 全程不将当前文件或恢复快照缓存到内存（使用流式分块哈希 + 同目录原子重命名备份）。
        struct PlannedOp {
            rel: String,
            target: PathBuf,
            restore_blob: Option<PathBuf>,
            existed: bool,
        }

        let mut plan: Vec<PlannedOp> = Vec::new();
        let mut conflicts = Vec::new();
        for snap in &record.files {
            if snap.unreadable {
                return Err(format!(
                    "文件 {} 在修改前无法读取原始快照，为避免数据丢失无法自动回滚。",
                    snap.path
                ));
            }
            // 先沿原始相对路径逐段检查，拒绝路径中间或末端出现任何符号链接（防止 Fence 跟随符号链接误伤目标）。
            verify_no_symlinks_under(fence.root(), &snap.path)?;

            let (target, inside) = fence
                .resolve_for_write(&snap.path)
                .map_err(|e| format!("回滚路径 {} 受限：{}", snap.path, e.0))?;
            if !inside {
                return Err(format!("拒绝回滚工作目录之外的路径：{}", snap.path));
            }

            let current_len = match std::fs::symlink_metadata(&target) {
                Ok(meta) => {
                    if meta.file_type().is_symlink() {
                        return Err(format!("拒绝回滚已被替换为符号链接的路径：{}", snap.path));
                    }
                    if !meta.is_file() {
                        return Err(format!(
                            "路径 {} 当前不是普通文件（可能已被替换为目录），无法安全回滚。",
                            snap.path
                        ));
                    }
                    Some(meta.len())
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(format!("检查 {} 状态失败：{e}", snap.path)),
            };

            if !force {
                let is_conflict = if snap.intervening_conflict {
                    true
                } else {
                    match current_len {
                        None => true,
                        Some(len) if snap.after_size.is_some_and(|expected| expected != len) => {
                            true
                        }
                        Some(_) => {
                            let current_hash = sha256_file_stream(&target)
                                .map_err(|e| format!("校验 {} 失败：{e}", snap.path))?;
                            current_hash != snap.after_sha256
                        }
                    }
                };
                if is_conflict {
                    conflicts.push(snap.path.clone());
                }
            }

            let restore_blob = match &snap.before_sha256 {
                Some(hash) => {
                    let blob_path = self.blobs_dir().join(hash);
                    let meta = std::fs::metadata(&blob_path)
                        .map_err(|e| format!("找不到 {} 的历史快照 Blob：{e}", snap.path))?;
                    if !meta.is_file() {
                        return Err(format!("{} 的历史快照 Blob 损坏（非普通文件）", snap.path));
                    }
                    if meta.len() > MAX_SNAPSHOT_BYTES {
                        return Err(format!("{} 的历史快照 Blob 超出安全大小上限", snap.path));
                    }
                    Some(blob_path)
                }
                None => None,
            };

            plan.push(PlannedOp {
                rel: snap.path.clone(),
                target,
                restore_blob,
                existed: current_len.is_some(),
            });
        }

        if !conflicts.is_empty() {
            return Err(format!(
                "以下文件在检查点期间或之后被外部修改过，已取消回滚：{}。确认覆盖请传入 force=true。",
                conflicts.join("、")
            ));
        }

        // 第二阶段：使用同目录原子重命名暂存原文件（O(1) 空间、零内存缓冲），
        // 任一步骤失败时按逆序将备份文件原子迁回，全部成功后再清理备份文件。
        let mut applied: Vec<AppliedOp> = Vec::new();
        let mut restored = Vec::new();
        let mut removed = Vec::new();
        for op in &plan {
            let apply_res: Result<Option<PathBuf>, String> = match &op.restore_blob {
                Some(blob_path) => (|| {
                    let parent = op
                        .target
                        .parent()
                        .ok_or_else(|| format!("恢复 {} 失败：缺少父目录", op.rel))?;
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("创建目录失败（{}）：{e}", op.rel))?;
                    let staged =
                        parent.join(format!(".sa-rewind-new-{}", uuid::Uuid::new_v4().simple()));
                    if let Err(e) = copy_file_synced(blob_path, &staged) {
                        let _ = std::fs::remove_file(&staged);
                        return Err(format!("准备 {} 恢复内容失败：{e}", op.rel));
                    }
                    let backup = if op.existed {
                        let bak = parent
                            .join(format!(".sa-rewind-bak-{}", uuid::Uuid::new_v4().simple()));
                        if let Err(e) = std::fs::rename(&op.target, &bak) {
                            let _ = std::fs::remove_file(&staged);
                            return Err(format!("暂存原文件 {} 失败：{e}", op.rel));
                        }
                        Some(bak)
                    } else {
                        None
                    };
                    match std::fs::rename(&staged, &op.target) {
                        Ok(()) => {
                            restored.push(op.rel.clone());
                            Ok(backup)
                        }
                        Err(e) => {
                            let _ = std::fs::remove_file(&staged);
                            if let Some(bak) = backup {
                                let _ = std::fs::rename(&bak, &op.target);
                            }
                            Err(format!("恢复 {} 失败：{e}", op.rel))
                        }
                    }
                })(),
                None => (|| {
                    if op.existed {
                        let parent = op
                            .target
                            .parent()
                            .ok_or_else(|| format!("删除 {} 失败：缺少父目录", op.rel))?;
                        let bak = parent
                            .join(format!(".sa-rewind-bak-{}", uuid::Uuid::new_v4().simple()));
                        std::fs::rename(&op.target, &bak)
                            .map(|()| {
                                removed.push(op.rel.clone());
                                Some(bak)
                            })
                            .map_err(|e| format!("删除新建文件 {} 失败：{e}", op.rel))
                    } else {
                        removed.push(op.rel.clone());
                        Ok(None)
                    }
                })(),
            };

            match apply_res {
                Ok(backup) => {
                    applied.push(AppliedOp {
                        rel: op.rel.clone(),
                        target: op.target.clone(),
                        backup,
                    });
                }
                Err(err) => {
                    return Err(compensate_applied(applied, err));
                }
            }
        }

        // 全部成功，清理暂存的原文件备份。
        for done in applied {
            if let Some(bak) = done.backup {
                let _ = std::fs::remove_file(bak);
            }
        }

        Ok(RewindOutcome {
            checkpoint_id: record.id,
            restored,
            removed,
        })
    }

    fn save_blob_unlocked(&self, bytes: &[u8]) -> Result<String, String> {
        let hash = sha256_hex(bytes);
        let path = self.blobs_dir().join(&hash);
        if path.exists() {
            return Ok(hash);
        }
        std::fs::create_dir_all(self.blobs_dir())
            .map_err(|e| format!("创建检查点目录失败：{e}"))?;
        write_private_atomically(&path, bytes).map_err(|e| format!("保存快照 Blob 失败：{e}"))?;
        Ok(hash)
    }

    fn upsert_record_unlocked(&self, record: CheckpointRecord) {
        let room_id = record.room_id.clone();
        let mut records = self.read_room_unlocked(&room_id);
        let mut pruned = false;
        if let Some(existing) = records.iter_mut().find(|r| r.id == record.id) {
            *existing = record;
        } else {
            records.push(record);
            if records.len() > MAX_CHECKPOINTS_PER_ROOM {
                let drain = records.len() - MAX_CHECKPOINTS_PER_ROOM;
                records.drain(..drain);
                pruned = true;
            }
        }
        let path = self.room_index_path(&room_id);
        let Ok(bytes) = serde_json::to_vec_pretty(&records) else {
            return;
        };
        if write_private_atomically(&path, &bytes).is_err() {
            return;
        }
        if pruned {
            self.gc_blobs_unlocked();
        }
    }

    /// 回收不再被任何房间检查点引用的孤立 Blob。
    /// 采用严格 fail-closed 策略：扫描所有房间索引过程中只要遇到任意目录/读取/解析错误，
    /// 立刻中止清理，绝不误删可能仍被引用的快照。
    fn gc_blobs_unlocked(&self) {
        let Ok(entries) = std::fs::read_dir(self.rooms_dir()) else {
            return;
        };
        let mut referenced: HashSet<String> = HashSet::new();
        for entry in entries {
            let Ok(entry) = entry else {
                return;
            };
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with(".sa-tmp-") {
                    continue;
                }
            }
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                return;
            };
            let Ok(records) = serde_json::from_slice::<Vec<CheckpointRecord>>(&bytes) else {
                return;
            };
            for rec in records {
                for file in rec.files {
                    if let Some(hash) = file.before_sha256 {
                        referenced.insert(hash);
                    }
                }
            }
        }
        if let Ok(entries) = std::fs::read_dir(self.blobs_dir()) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if !name.starts_with(".sa-tmp-") && !referenced.contains(name) {
                        let _ = std::fs::remove_file(path);
                    }
                }
            }
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

/// 沿原始相对路径逐段检查，拒绝路径中间目录或目标文件本身是符号链接。
fn verify_no_symlinks_under(root: &Path, rel: &str) -> Result<(), String> {
    let mut cur = root.to_path_buf();
    for comp in Path::new(rel).components() {
        let Component::Normal(seg) = comp else {
            return Err(format!("检查点路径不合法：{rel}"));
        };
        cur.push(seg);
        match std::fs::symlink_metadata(&cur) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!("拒绝沿着符号链接回滚路径：{rel}"));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(format!("检查路径 {rel} 失败：{e}")),
        }
    }
    Ok(())
}

/// 始终以仅当前用户可读（Unix `0o600`）原子写入快照 Blob 与索引文件。
fn write_private_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("路径没有上级目录"))?;
    std::fs::create_dir_all(dir)?;
    let temp = dir.join(format!(".sa-tmp-{}", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
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
        before: BeforeState<'_>,
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
        let workspace = fence.root().to_string_lossy().into_owned();
        let after_hash = sha256_hex(after_bytes);
        let _store_guard = self.store.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut map = self.files.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = map.get_mut(&rel) {
            // 同一轮里多次改同一个文件：对比本次写前内容和上次写后内容，发现外部插队修改则标冲突。
            match before {
                BeforeState::Bytes(bytes) => {
                    if sha256_hex(bytes) != existing.after_sha256 {
                        existing.intervening_conflict = true;
                    }
                }
                BeforeState::Missing | BeforeState::Unreadable => {
                    existing.intervening_conflict = true;
                }
            }
            existing.after_sha256 = after_hash;
            existing.after_size = Some(after_bytes.len() as u64);
        } else {
            let (before_hash, unreadable) = match before {
                BeforeState::Missing => (None, false),
                BeforeState::Unreadable => (None, true),
                BeforeState::Bytes(bytes) if (bytes.len() as u64) <= MAX_SNAPSHOT_BYTES => {
                    match self.store.save_blob_unlocked(bytes) {
                        Ok(h) => (Some(h), false),
                        Err(_) => (None, true),
                    }
                }
                BeforeState::Bytes(_) => (None, true),
            };
            map.insert(
                rel.clone(),
                FileSnapshot {
                    path: rel,
                    before_sha256: before_hash,
                    after_sha256: after_hash,
                    after_size: Some(after_bytes.len() as u64),
                    unreadable,
                    intervening_conflict: false,
                },
            );
        }
        let snapshot_list: Vec<FileSnapshot> = map.values().cloned().collect();
        drop(map);
        self.store.upsert_record_unlocked(CheckpointRecord {
            id: self.id.clone(),
            room_id: self.room_id.clone(),
            workspace,
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

/// 分块流式计算文件 SHA-256，内存恒定 64 KiB，避免大文件回滚预检 OOM。
fn sha256_file_stream(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// 分块流式复制快照 Blob 到同目录临时文件并刷盘，不把整个文件留在内存中。
fn copy_file_synced(src: &Path, dst: &Path) -> std::io::Result<()> {
    let mut reader = std::fs::File::open(src)?;
    let mut writer = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)?;
    std::io::copy(&mut reader, &mut writer)?;
    writer.sync_all()
}

struct AppliedOp {
    rel: String,
    target: PathBuf,
    backup: Option<PathBuf>,
}

fn compensate_applied(applied: Vec<AppliedOp>, err: String) -> String {
    let mut comp_errors = Vec::new();
    for prev in applied.into_iter().rev() {
        let undo = match prev.backup {
            Some(bak) => std::fs::rename(&bak, &prev.target),
            None => {
                if prev.target.exists() {
                    std::fs::remove_file(&prev.target)
                } else {
                    Ok(())
                }
            }
        };
        if let Err(ue) = undo {
            comp_errors.push(format!("{}({ue})", prev.rel));
        }
    }
    if comp_errors.is_empty() {
        format!("{err}（已自动撤销本批前面恢复的改动）")
    } else {
        format!(
            "{err}；尝试补偿撤销时以下文件失败：{}",
            comp_errors.join("、")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EditTool, MultiEditTool, Tool, ToolCtx, WriteTool};
    use serde_json::json;
    use std::fs;

    #[tokio::test]
    async fn snapshots_and_rewinds_edited_and_created_files_with_conflict_and_workspace_guards() {
        let dir = tempfile::tempdir().unwrap();
        let work_a = dir.path().join("work_a");
        let work_b = dir.path().join("work_b");
        let data = dir.path().join("data");
        fs::create_dir_all(&work_a).unwrap();
        fs::create_dir_all(&work_b).unwrap();
        fs::write(work_a.join("app.rs"), "fn main() { let x = 1; }\n").unwrap();
        fs::write(work_b.join("app.rs"), "fn main() { let x = 99; }\n").unwrap();

        let fence_a = Fence::new(&work_a, &[]).unwrap();
        let fence_b = Fence::new(&work_b, &[]).unwrap();
        let store = Arc::new(CheckpointStore::new(data.join("checkpoints")));
        let cp = store.begin_turn("room_1", "msg_1", "助手", 1000);
        let mut ctx = ToolCtx::new(fence_a.clone());
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

        let list_a = store.list("room_1", &fence_a);
        assert_eq!(list_a.len(), 1);
        assert_eq!(list_a[0].files.len(), 2);
        // 切换工作目录 B 后看不到 A 的检查点，强制回滚也会被拒绝。
        assert!(store.list("room_1", &fence_b).is_empty());
        let ws_err = store.rewind("room_1", "msg_1", &fence_b, true).unwrap_err();
        assert!(ws_err.contains("跨项目回滚"), "{ws_err}");

        // 模拟外部修改冲突检测。
        fs::write(work_a.join("app.rs"), "external change\n").unwrap();
        let conflict = store
            .rewind("room_1", "msg_1", &fence_a, false)
            .unwrap_err();
        assert!(conflict.contains("app.rs"), "{conflict}");

        // force=true 强制回滚：app.rs 回到最初始的 x = 1，new.txt 被删除。
        let report = store.rewind("room_1", "msg_1", &fence_a, true).unwrap();
        assert_eq!(report.restored, vec!["app.rs"]);
        assert_eq!(report.removed, vec!["new.txt"]);
        assert_eq!(
            fs::read_to_string(work_a.join("app.rs")).unwrap(),
            "fn main() { let x = 1; }\n"
        );
        assert!(!work_a.join("new.txt").exists());
    }

    #[tokio::test]
    async fn detects_intervening_edit_between_two_writes_in_same_turn() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("doc.txt"), "title\nbody\nfooter\n").unwrap();

        let fence = Fence::new(&work, &[]).unwrap();
        let store = Arc::new(CheckpointStore::new(dir.path().join("cp")));
        let mut ctx = ToolCtx::new(fence.clone());
        ctx.checkpoint = Some(store.begin_turn("room_1", "msg_2", "助手", 2000));

        EditTool
            .call(
                json!({"path": "doc.txt", "old_string": "title", "new_string": "TITLE"}),
                &ctx,
            )
            .await
            .unwrap();
        // 模拟用户在两步之间修改了 body。
        fs::write(work.join("doc.txt"), "TITLE\nUSER_BODY\nfooter\n").unwrap();
        EditTool
            .call(
                json!({"path": "doc.txt", "old_string": "footer", "new_string": "FOOTER"}),
                &ctx,
            )
            .await
            .unwrap();

        let err = store.rewind("room_1", "msg_2", &fence, false).unwrap_err();
        assert!(err.contains("doc.txt"), "{err}");
    }

    #[tokio::test]
    async fn gc_aborts_when_another_room_index_is_unreadable_or_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("b.txt"), "room-b-original").unwrap();
        fs::write(work.join("a.txt"), "room-a-0").unwrap();

        let fence = Fence::new(&work, &[]).unwrap();
        let store = Arc::new(CheckpointStore::new(dir.path().join("cp")));

        // 房间 B 建一个检查点，记下它引用的 Blob 路径。
        let mut ctx_b = ToolCtx::new(fence.clone());
        ctx_b.checkpoint = Some(store.begin_turn("room_b", "b_1", "助手", 100));
        WriteTool
            .call(
                json!({"path": "b.txt", "content": "room-b-updated"}),
                &ctx_b,
            )
            .await
            .unwrap();
        let b_hash = store.list("room_b", &fence)[0].files[0]
            .before_sha256
            .clone()
            .unwrap();
        let b_blob = dir.path().join("cp").join("blobs").join(&b_hash);
        assert!(b_blob.exists());

        // 模拟房间 B 索引损坏（无法解析），再让房间 A 写入超过上限触发淘汰与 GC。
        let room_b_index = dir.path().join("cp").join("rooms").join("room_b.json");
        let saved_index = fs::read(&room_b_index).unwrap();
        fs::write(&room_b_index, b"{corrupt json").unwrap();

        for i in 0..=MAX_CHECKPOINTS_PER_ROOM {
            let mut ctx_a = ToolCtx::new(fence.clone());
            ctx_a.checkpoint =
                Some(store.begin_turn("room_a", format!("a_{i}"), "助手", 200 + i as i64));
            WriteTool
                .call(
                    json!({"path": "a.txt", "content": format!("room-a-next-{i}")}),
                    &ctx_a,
                )
                .await
                .unwrap();
        }

        // GC 必须 fail-closed 中止，房间 B 的 Blob 依然完整保留。
        assert!(b_blob.exists(), "房间 B 的快照 Blob 不应在索引损坏时被误删");

        // 恢复房间 B 索引后，依然能完整回滚。
        fs::write(&room_b_index, saved_index).unwrap();
        let report = store.rewind("room_b", "b_1", &fence, false).unwrap();
        assert_eq!(report.restored, vec!["b.txt"]);
        assert_eq!(
            fs::read_to_string(work.join("b.txt")).unwrap(),
            "room-b-original"
        );
    }

    #[tokio::test]
    async fn rewind_cleans_up_temp_backups_and_detects_size_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("fixture.bin"), "small").unwrap();

        let fence = Fence::new(&work, &[]).unwrap();
        let store = Arc::new(CheckpointStore::new(dir.path().join("cp")));
        let mut ctx = ToolCtx::new(fence.clone());
        ctx.checkpoint = Some(store.begin_turn("room_1", "msg_3", "助手", 3000));
        WriteTool
            .call(json!({"path": "fixture.bin", "content": "updated"}), &ctx)
            .await
            .unwrap();

        // 外部把文件写大，非 force 应直接通过 after_size 判定冲突；force=true 应顺利恢复且不留临时备份文件。
        fs::write(work.join("fixture.bin"), "x".repeat(128 * 1024)).unwrap();
        let err = store.rewind("room_1", "msg_3", &fence, false).unwrap_err();
        assert!(err.contains("fixture.bin"), "{err}");

        let report = store.rewind("room_1", "msg_3", &fence, true).unwrap();
        assert_eq!(report.restored, vec!["fixture.bin"]);
        assert_eq!(
            fs::read_to_string(work.join("fixture.bin")).unwrap(),
            "small"
        );
        let leftovers: Vec<_> = fs::read_dir(&work)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".sa-rewind-"))
            .collect();
        assert!(leftovers.is_empty(), "不应遗留临时备份文件: {leftovers:?}");
    }
}
