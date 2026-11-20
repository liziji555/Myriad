//! 任务的独立工作目录。
//!
//! 工作目录是 git 仓库时，给任务开一个 worktree 和一条分支；不是的话复制一份出来。
//! 两种情况下任务目录里都有 git，用它来算「改了哪些文件」和把改动带回去。

use sa_engine::Workdir;
use sa_protocol::TaskId;
use std::path::{Path, PathBuf};
use tokio::process::Command;

pub const KIND_WORKTREE: &str = "worktree";
pub const KIND_COPY: &str = "copy";
/// 复制工作目录时最多复制这么多个文件、这么多字节。
const MAX_COPY_FILES: usize = 50_000;
const MAX_COPY_BYTES: u64 = 200 * 1024 * 1024;

/// 跑一条 git 命令，返回标准输出。失败时返回 git 自己说的原因。
pub async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(plain(dir))
        .args([
            "-c",
            "user.name=SuperAgent",
            "-c",
            "user.email=superagent@localhost",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.quotepath=false",
        ])
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => "这台电脑上没有装 git，任务需要它".to_string(),
            _ => format!("git 跑不起来：{e}"),
        })?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let said = String::from_utf8_lossy(&output.stderr);
        let said = said.trim();
        Err(if said.is_empty() {
            format!("git {} 失败了", args.first().copied().unwrap_or(""))
        } else {
            said.to_string()
        })
    }
}

/// Windows 上规范化的路径带 `\\?\` 前缀，git 不认，去掉它。
fn plain(path: &Path) -> PathBuf {
    let text = path.display().to_string();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path.to_path_buf(),
    }
}

/// 给任务准备独立目录。`workspace` 是人的工作目录，`work_root` 是放任务目录的地方。
pub async fn provision(
    workspace: &Path,
    work_root: &Path,
    task_id: &TaskId,
) -> Result<Workdir, String> {
    git(workspace, &["--version"]).await?;
    let target = work_root.join(task_id.as_str());
    if target.exists() {
        return Err(format!("任务目录 {} 已经存在", target.display()));
    }
    std::fs::create_dir_all(work_root).map_err(|e| format!("建不了任务目录：{e}"))?;

    // 工作目录在一个有提交的 git 仓库里：开 worktree。
    if let Ok(head) = git(workspace, &["rev-parse", "--verify", "HEAD"]).await {
        let head = head.trim().to_string();
        let prefix = git(workspace, &["rev-parse", "--show-prefix"]).await?;
        let branch = format!("sa/{}", task_id.as_str().replace('_', "-"));
        let target_text = plain(&target).display().to_string();
        git(
            workspace,
            &["worktree", "add", "-b", &branch, &target_text, &head],
        )
        .await?;
        // 工作目录是仓库里的一个子目录时，任务也只在对应的子目录里干活。
        let path = target.join(prefix.trim());
        return Ok(Workdir {
            path: path.display().to_string(),
            kind: KIND_WORKTREE.into(),
            branch: Some(branch),
            base_ref: head,
        });
    }

    let (from, to) = (workspace.to_path_buf(), target.clone());
    tokio::task::spawn_blocking(move || copy_tree(&from, &to))
        .await
        .map_err(|e| format!("复制工作目录时出错：{e}"))??;
    git(&target, &["init", "-q"]).await?;
    git(&target, &["add", "-A"]).await?;
    git(
        &target,
        &[
            "commit",
            "-q",
            "--no-verify",
            "--allow-empty",
            "-m",
            "开工前的样子",
        ],
    )
    .await?;
    let head = git(&target, &["rev-parse", "HEAD"]).await?;
    Ok(Workdir {
        path: target.display().to_string(),
        kind: KIND_COPY.into(),
        branch: None,
        base_ref: head.trim().to_string(),
    })
}

/// 把 `from` 整个复制到 `to`，不带 `.git`，不跟符号链接。
fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    let (mut files, mut bytes) = (0usize, 0u64);
    let mut pending = vec![(from.to_path_buf(), to.to_path_buf())];
    while let Some((src, dst)) = pending.pop() {
        std::fs::create_dir_all(&dst).map_err(|e| format!("建不了 {}：{e}", dst.display()))?;
        let entries =
            std::fs::read_dir(&src).map_err(|e| format!("读不了 {}：{e}", src.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("读不了 {}：{e}", src.display()))?;
            let kind = entry
                .file_type()
                .map_err(|e| format!("读不了 {}：{e}", entry.path().display()))?;
            let target = dst.join(entry.file_name());
            if kind.is_dir() {
                if entry.file_name() != ".git" {
                    pending.push((entry.path(), target));
                }
            } else if kind.is_file() {
                files += 1;
                bytes += std::fs::copy(entry.path(), &target)
                    .map_err(|e| format!("复制不了 {}：{e}", entry.path().display()))?;
                if files > MAX_COPY_FILES || bytes > MAX_COPY_BYTES {
                    return Err(
                        "工作目录太大，复制不了一份给任务用。在工作目录里执行一次 git init 并提交，任务就能直接用 git 分支干活。"
                            .into(),
                    );
                }
            }
        }
    }
    Ok(())
}

/// 任务目录所在的 git 仓库根。
fn repo_root(workdir: &Workdir, work_root: &Path, task_id: &TaskId) -> PathBuf {
    if workdir.kind == KIND_WORKTREE {
        work_root.join(task_id.as_str())
    } else {
        PathBuf::from(&workdir.path)
    }
}

/// 把任务目录里现在的改动提交掉，然后列出从开工到现在改了哪些文件（路径、「新增 / 修改 / 删除」）。
pub async fn commit_and_diff(
    workdir: &Workdir,
    message: &str,
) -> Result<Vec<(String, String)>, String> {
    let dir = Path::new(&workdir.path);
    git(dir, &["add", "-A", "."]).await?;
    let staged = git(dir, &["status", "--porcelain"]).await?;
    if !staged.trim().is_empty() {
        git(dir, &["commit", "-q", "--no-verify", "-m", message]).await?;
    }
    let listed = git(
        dir,
        &[
            "diff",
            "--name-status",
            "--no-renames",
            &workdir.base_ref,
            "HEAD",
            "--",
            ".",
        ],
    )
    .await?;
    Ok(listed
        .lines()
        .filter_map(|line| {
            let (status, path) = line.split_once('\t')?;
            let label = match status.chars().next()? {
                'A' => "新增",
                'D' => "删除",
                _ => "修改",
            };
            Some((path.to_string(), label.to_string()))
        })
        .collect())
}

/// 验收通过：把任务的改动带回人的工作目录。
///
/// worktree：把任务分支合并进工作目录当前所在的分支。复制出来的：把改动当补丁打回去。
/// 带不回去（有冲突、工作目录里有没提交的改动挡着）时返回原因，什么都不改。
pub async fn apply_to_workspace(workspace: &Path, workdir: &Workdir) -> Result<(), String> {
    if workdir.kind == KIND_WORKTREE {
        let branch = workdir.branch.as_deref().unwrap_or_default();
        let message = format!("合并任务分支 {branch}");
        return match git(workspace, &["merge", "--no-edit", "-m", &message, branch]).await {
            Ok(_) => Ok(()),
            Err(reason) => {
                // 合并到一半停下的话收拾干净，不把冲突留在人的工作目录里。
                let _ = git(workspace, &["merge", "--abort"]).await;
                Err(format!("改动合并不进工作目录：{reason}"))
            }
        };
    }
    let dir = Path::new(&workdir.path);
    let patch = git(dir, &["diff", "--binary", &workdir.base_ref, "HEAD"]).await?;
    if patch.trim().is_empty() {
        return Ok(());
    }
    let file = dir.join(".git").join("sa-accept.patch");
    std::fs::write(&file, patch).map_err(|e| format!("写不了补丁：{e}"))?;
    let file_text = plain(&file).display().to_string();
    // 工作目录可能在别人的 git 仓库里的某一层：补丁里的路径从工作目录算起。
    let applied = Command::new("git")
        .current_dir(plain(workspace))
        .args(["apply", "--whitespace=nowarn", &file_text])
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| format!("git 跑不起来：{e}"))?;
    if applied.status.success() {
        Ok(())
    } else {
        Err(format!(
            "改动打不回工作目录（开工之后那里的文件也变了）：{}",
            String::from_utf8_lossy(&applied.stderr).trim()
        ))
    }
}

/// 任务结束后删掉它的独立目录。分支留着。删不掉只记日志，不影响任务状态。
pub async fn remove(workspace: &Path, work_root: &Path, task_id: &TaskId, workdir: &Workdir) {
    let root = repo_root(workdir, work_root, task_id);
    if workdir.kind == KIND_WORKTREE {
        let root_text = plain(&root).display().to_string();
        if let Err(error) = git(workspace, &["worktree", "remove", "--force", &root_text]).await {
            tracing::warn!(task = %task_id, "worktree 没删掉：{error}");
        }
    }
    if root.exists() {
        let gone = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(root)).await;
        if let Ok(Err(error)) = gone {
            tracing::warn!(task = %task_id, "任务目录没删掉：{error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn repo(dir: &Path) {
        git(dir, &["init", "-q", "-b", "main"]).await.unwrap();
        std::fs::write(dir.join("a.txt"), "一\n").unwrap();
        git(dir, &["add", "-A"]).await.unwrap();
        git(dir, &["commit", "-q", "-m", "init"]).await.unwrap();
    }

    #[tokio::test]
    async fn a_git_workspace_gets_a_worktree_and_accepting_merges_the_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let (workspace, work_root) = (tmp.path().join("ws"), tmp.path().join("work"));
        std::fs::create_dir_all(&workspace).unwrap();
        repo(&workspace).await;
        let task_id = TaskId::new();

        let workdir = provision(&workspace, &work_root, &task_id).await.unwrap();
        assert_eq!(workdir.kind, KIND_WORKTREE);
        let dir = PathBuf::from(&workdir.path);
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "一\n");

        std::fs::write(dir.join("a.txt"), "二\n").unwrap();
        std::fs::write(dir.join("b.txt"), "新的\n").unwrap();
        let changed = commit_and_diff(&workdir, "修 bug").await.unwrap();
        assert_eq!(
            changed,
            [
                ("a.txt".to_string(), "修改".to_string()),
                ("b.txt".to_string(), "新增".to_string())
            ]
        );
        // 没有新改动时再算一次，结果一样，也不会多一个空提交。
        assert_eq!(commit_and_diff(&workdir, "修 bug").await.unwrap(), changed);
        // 人的工作目录在验收之前不受影响。
        assert_eq!(
            std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
            "一\n"
        );

        apply_to_workspace(&workspace, &workdir).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
            "二\n"
        );
        assert!(workspace.join("b.txt").exists());

        remove(&workspace, &work_root, &task_id, &workdir).await;
        assert!(!dir.exists());
        let branches = git(&workspace, &["branch", "--list"]).await.unwrap();
        assert!(branches.contains(workdir.branch.as_deref().unwrap()));
    }

    #[tokio::test]
    async fn a_conflicting_merge_is_refused_and_leaves_the_workspace_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let (workspace, work_root) = (tmp.path().join("ws"), tmp.path().join("work"));
        std::fs::create_dir_all(&workspace).unwrap();
        repo(&workspace).await;
        let workdir = provision(&workspace, &work_root, &TaskId::new())
            .await
            .unwrap();
        std::fs::write(Path::new(&workdir.path).join("a.txt"), "任务改的\n").unwrap();
        commit_and_diff(&workdir, "任务").await.unwrap();
        std::fs::write(workspace.join("a.txt"), "人改的\n").unwrap();
        git(&workspace, &["commit", "-q", "-am", "人改的"])
            .await
            .unwrap();

        let error = apply_to_workspace(&workspace, &workdir).await.unwrap_err();
        assert!(error.contains("合并不进"), "{error}");
        assert_eq!(
            std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
            "人改的\n"
        );
        let status = git(&workspace, &["status", "--porcelain"]).await.unwrap();
        assert_eq!(status.trim(), "");
    }

    #[tokio::test]
    async fn a_plain_folder_is_copied_and_accepting_patches_it_back() {
        let tmp = tempfile::tempdir().unwrap();
        let (workspace, work_root) = (tmp.path().join("ws"), tmp.path().join("work"));
        std::fs::create_dir_all(workspace.join("sub")).unwrap();
        std::fs::write(workspace.join("a.txt"), "一\n").unwrap();
        std::fs::write(workspace.join("sub/old.txt"), "旧的\n").unwrap();
        let task_id = TaskId::new();

        let workdir = provision(&workspace, &work_root, &task_id).await.unwrap();
        assert_eq!(workdir.kind, KIND_COPY);
        assert_eq!(workdir.branch, None);
        let dir = PathBuf::from(&workdir.path);
        std::fs::write(dir.join("a.txt"), "二\n").unwrap();
        std::fs::remove_file(dir.join("sub/old.txt")).unwrap();
        std::fs::write(dir.join("sub/new.txt"), "新的\n").unwrap();
        let changed = commit_and_diff(&workdir, "整理").await.unwrap();
        assert_eq!(changed.len(), 3, "{changed:?}");
        assert!(changed.contains(&("sub/old.txt".to_string(), "删除".to_string())));

        apply_to_workspace(&workspace, &workdir).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
            "二\n"
        );
        assert!(!workspace.join("sub/old.txt").exists());
        assert!(workspace.join("sub/new.txt").exists());
        // 人的工作目录不会因此变成 git 仓库。
        assert!(!workspace.join(".git").exists());

        // 同一份改动再打一次打不上：不会悄悄改坏文件。
        assert!(apply_to_workspace(&workspace, &workdir).await.is_err());
        remove(&workspace, &work_root, &task_id, &workdir).await;
        assert!(!dir.exists());
    }
}
