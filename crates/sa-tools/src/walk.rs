//! 遍历工作目录：遵守 .gitignore，跳过隐藏文件、符号链接和围栏不允许的目录。

use crate::Fence;
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

pub(crate) struct Found {
    pub path: PathBuf,
    pub is_dir: bool,
}

/// 从 `start`（已通过围栏检查的真实路径）往下走，结果按路径排序。
pub(crate) fn walk(fence: &Fence, start: &Path) -> impl Iterator<Item = Found> {
    let guard = fence.clone();
    WalkBuilder::new(start)
        // 没有 .git 目录时也认 .gitignore。
        .require_git(false)
        .follow_links(false)
        .sort_by_file_path(|a, b| a.cmp(b))
        .filter_entry(move |entry| guard.allows(entry.path()))
        .build()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let kind = entry.file_type()?;
            // 不跟符号链接：它可能指到工作目录外面。
            if kind.is_symlink() || entry.depth() == 0 {
                return None;
            }
            Some(Found {
                is_dir: kind.is_dir(),
                path: entry.into_path(),
            })
        })
}
