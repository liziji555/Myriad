//! 任务的独立工作目录。
//!
//! 工作目录是 git 仓库时，给任务开一个 worktree 和一条分支；不是的话复制一份出来。
//! 两种情况下任务目录都有 git 记录，用它来算「改了哪些文件」和把改动带回去。

use sa_engine::Workdir;
use sa_protocol::TaskId;
use sa_tools::process::{kill_tree, own_group, TreeGuard};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

pub const KIND_WORKTREE: &str = "worktree";
pub const KIND_COPY: &str = "copy";
/// 复制工作目录时最多复制这么多个文件、这么多字节。
const MAX_COPY_FILES: usize = 50_000;
const MAX_COPY_BYTES: u64 = 200 * 1024 * 1024;

/// 自动提交用的身份；文件名原样输出，不转义中文。
const IDENTITY: [&str; 14] = [
    "-c",
    "maintenance.auto=false",
    "-c",
    "gc.auto=0",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "user.name=SuperAgent",
    "-c",
    "user.email=superagent@localhost",
    "-c",
    "commit.gpgsign=false",
    "-c",
    "core.quotepath=false",
];
/// 不做换行符转换，也不因为换行符报警。复制出来的任务目录用：
/// 这样算出来的补丁和人的工作目录里的文件逐字节对得上（Windows 上 git 默认会把 CRLF 换成 LF）。
const EXACT: [&str; 4] = ["-c", "core.autocrlf=false", "-c", "core.safecrlf=false"];

fn command() -> Command {
    let mut command = Command::new("git");
    // 不继承外面的 git 环境（比如从 git 钩子里启动时）：用哪个仓库由下面明确指定。
    for name in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
        command.env_remove(name);
    }
    command
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    command
}

/// 跑完一条 git 命令，返回标准输出。失败时返回 git 自己说的原因。
async fn finish(mut command: Command, what: &str) -> Result<String, String> {
    let output = command.output().await.map_err(launch_error)?;
    outcome(output, what)
}

fn launch_error(error: std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => "这台电脑上没有装 git，任务需要它".to_string(),
        _ => format!("git 跑不起来：{error}"),
    }
}

fn outcome(output: std::process::Output, what: &str) -> Result<String, String> {
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let said = String::from_utf8_lossy(&output.stderr);
        let said = said.trim();
        Err(if said.is_empty() {
            format!("git {what} 失败了")
        } else {
            said.to_string()
        })
    }
}

/// 在人的工作目录里跑 git，用的是那个仓库自己的配置。
pub async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut command = command();
    command.arg("-C").arg(plain(dir)).args(IDENTITY).args(args);
    finish(command, args.first().copied().unwrap_or("")).await
}

/// 在任务目录里跑 git。git 目录和目录树的根都明确指定，不让 git 自己往上找：
/// 任务里的 Bot 能随便写任务目录，如果 git 认它放进来的 `.git`，它就能换掉 git 的配置，
/// 而配置里可以写要执行的命令。真正的 git 目录在它写不到的地方。
async fn task_git(workdir: &Workdir, args: &[&str]) -> Result<String, String> {
    let mut command = command();
    command
        .arg("-C")
        .arg(plain(Path::new(&workdir.path)))
        .env("GIT_DIR", &workdir.git_dir)
        .env("GIT_WORK_TREE", &workdir.root);
    if workdir.kind == KIND_COPY {
        command.args(EXACT);
    }
    command.args(IDENTITY).args(args);
    finish(command, args.first().copied().unwrap_or("")).await
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
    // git 按工作目录解析相对路径，之后的查找按本服务的当前目录：先变成绝对路径。
    let work_root = &std::path::absolute(work_root).map_err(|e| format!("建不了任务目录：{e}"))?;
    let target = work_root.join(task_id.as_str());
    if target.exists() {
        return Err(format!("任务目录 {} 已经存在", target.display()));
    }
    std::fs::create_dir_all(work_root).map_err(|e| format!("建不了任务目录：{e}"))?;
    let source = plain(workspace).display().to_string();
    let root = plain(&target).display().to_string();

    // 工作目录在一个有提交的 git 仓库里：开 worktree。
    if let Ok(head) = git(workspace, &["rev-parse", "--verify", "HEAD"]).await {
        let head = head.trim().to_string();
        let prefix = git(workspace, &["rev-parse", "--show-prefix"]).await?;
        let branch = format!("sa/{}", task_id.as_str().replace('_', "-"));
        git(workspace, &["worktree", "add", "-b", &branch, &root, &head]).await?;
        // 这时任务还没开始，目录里的 `.git` 是 git 刚写的，可以信。
        let git_dir = git(&target, &["rev-parse", "--absolute-git-dir"]).await?;
        // 工作目录是仓库里的一个子目录时，任务也只在对应的子目录里干活。
        let path = target.join(prefix.trim().trim_end_matches('/'));
        return Ok(Workdir {
            path: plain(&path).display().to_string(),
            kind: KIND_WORKTREE.into(),
            branch: Some(branch),
            base_ref: head,
            root,
            git_dir: git_dir.trim().to_string(),
            source,
        });
    }

    let git_dir = work_root.join(format!("{}.git", task_id.as_str()));
    let mut workdir = Workdir {
        path: root.clone(),
        kind: KIND_COPY.into(),
        branch: None,
        base_ref: String::new(),
        root,
        git_dir: plain(&git_dir).display().to_string(),
        source,
    };
    match snapshot_copy(workspace, &target, &workdir).await {
        Ok(head) => {
            workdir.base_ref = head;
            Ok(workdir)
        }
        Err(reason) => {
            // 没准备成：半截的目录不留，下次重试时从头来。
            let _ = tokio::task::spawn_blocking(move || {
                let _ = force_remove(&target);
                let _ = force_remove(&git_dir);
            })
            .await;
            Err(reason)
        }
    }
}

/// 把工作目录复制成任务目录，并记下开工前的样子。返回那次提交。
async fn snapshot_copy(
    workspace: &Path,
    target: &Path,
    workdir: &Workdir,
) -> Result<String, String> {
    let (from, to) = (workspace.to_path_buf(), target.to_path_buf());
    tokio::task::spawn_blocking(move || copy_tree(&from, &to))
        .await
        .map_err(|e| format!("复制工作目录时出错：{e}"))??;
    task_git(workdir, &["init", "-q"]).await?;
    // 被忽略的文件也记：这份目录不是人的仓库，任务写出来的东西都要能带回去。
    task_git(workdir, &["add", "-A", "-f", "."]).await?;
    task_git(
        workdir,
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
    Ok(task_git(workdir, &["rev-parse", "HEAD"])
        .await?
        .trim()
        .to_string())
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
            if entry.file_name() == ".git" {
                continue;
            }
            let target = dst.join(entry.file_name());
            if kind.is_dir() {
                pending.push((entry.path(), target));
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

/// 把任务目录里现在的改动提交掉，然后列出从开工到现在改了哪些文件（路径、「新增 / 修改 / 删除」）。
pub async fn commit_and_diff(
    workdir: &Workdir,
    message: &str,
) -> Result<Vec<(String, String)>, String> {
    let add: &[&str] = if workdir.kind == KIND_COPY {
        &["add", "-A", "-f", "."]
    } else {
        &["add", "-A", "."]
    };
    task_git(workdir, add).await?;
    // 只看暂存区：工作目录是仓库里的一层时，别的层里有没提交的东西不关这次提交的事。
    // 子模块指针的改动也算：仓库配了 `diff.ignoreSubmodules` 也不能漏。名字全是空格的文件也算，所以不 trim。
    let staged = task_git(
        workdir,
        &[
            "diff",
            "--cached",
            "--name-only",
            "--ignore-submodules=none",
        ],
    )
    .await?;
    if !staged.is_empty() {
        task_git(workdir, &["commit", "-q", "--no-verify", "-m", message]).await?;
    }
    let listed = task_git(
        workdir,
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

/// 人的工作目录里是不是有一个合并做到一半。
async fn merging(workspace: &Path) -> bool {
    git(workspace, &["rev-parse", "-q", "--verify", "MERGE_HEAD"])
        .await
        .is_ok()
}

/// 验收通过：把任务的改动带回它开出来的那个工作目录。
///
/// worktree：把任务分支合并进工作目录当前所在的分支。复制出来的：把改动当补丁打回去。
/// 带不回去（有冲突、工作目录里有没提交的改动挡着）时返回原因，什么都不改。
pub async fn apply_to_workspace(workdir: &Workdir) -> Result<(), String> {
    let workspace = Path::new(&workdir.source);
    if workdir.kind == KIND_WORKTREE {
        // 人自己正合并到一半：不碰。下面失败时的「放弃合并」只能放弃我们自己开始的那一次。
        if merging(workspace).await {
            return Err(
                "工作目录里有一个还没完成的合并。先把它处理完（提交或者放弃），再来验收。".into(),
            );
        }
        let branch = workdir.branch.as_deref().unwrap_or_default();
        let message = format!("合并任务分支 {branch}");
        return match git(workspace, &["merge", "--no-edit", "-m", &message, branch]).await {
            Ok(_) => Ok(()),
            Err(reason) => {
                // 合并到一半停下的话收拾干净，不把冲突留在人的工作目录里。
                if merging(workspace).await {
                    let _ = git(workspace, &["merge", "--abort"]).await;
                }
                Err(format!("改动合并不进工作目录：{reason}"))
            }
        };
    }
    // 补丁直接写进文件，不经过字符串：文件不一定是 UTF-8 的。
    let patch = Path::new(&workdir.git_dir).join("sa-accept.patch");
    let output = format!("--output={}", patch.display());
    task_git(
        workdir,
        &[
            "diff",
            "--binary",
            "--no-renames",
            &output,
            &workdir.base_ref,
            "HEAD",
        ],
    )
    .await?;
    if std::fs::metadata(&patch).map_or(true, |meta| meta.len() == 0) {
        return Ok(());
    }
    let mut apply = command();
    apply.current_dir(plain(workspace));
    // 工作目录可能在一个还没有提交的 git 仓库里的某一层。不让 git 往上找到那个仓库：
    // 否则它会按仓库根来理解补丁里的路径，对不上的悄悄跳过，还报告成功。
    if let Some(parent) = workspace.parent() {
        apply.env("GIT_CEILING_DIRECTORIES", plain(parent));
    }
    apply
        .args(EXACT)
        .args(["apply", "--whitespace=nowarn"])
        .arg(&patch);
    finish(apply, "apply")
        .await
        .map(drop)
        .map_err(|reason| format!("改动打不回工作目录（开工之后那里的文件也变了）：{reason}"))
}

/// 任务目录现在在哪个分支上。不在任何分支上（游离的 HEAD）时是 `None`。
pub async fn current_branch(workdir: &Workdir) -> Option<String> {
    let name = task_git(workdir, &["symbolic-ref", "--short", "-q", "HEAD"])
        .await
        .ok()?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// 目录里切到了别的分支时，把开工时的分支挪到现在的提交上再切回去：带回的是真正做完的那份。
pub async fn back_to_branch(workdir: &Workdir) -> Result<(), String> {
    let Some(branch) = &workdir.branch else {
        return Ok(());
    };
    if current_branch(workdir).await.as_deref() == Some(branch.as_str()) {
        return Ok(());
    }
    task_git(workdir, &["checkout", "-q", "-B", branch])
        .await
        .map(|_| ())
}

/// 从开工到现在，分支上改了工作目录外面的哪些文件（路径相对仓库根）。
/// 工作目录是仓库里的一层时，合并分支会把外面的改动一起带进来，而改动清单里只列了里面的。
pub async fn outside_changes(workdir: &Workdir) -> Result<Vec<String>, String> {
    if workdir.kind != KIND_WORKTREE {
        return Ok(vec![]);
    }
    // 和合并时一样，从分支与人的工作目录分开的地方算起：分支变基到新的提交上之后，
    // 那些提交里本来就有的改动不算它的。
    let target = git(Path::new(&workdir.source), &["rev-parse", "HEAD"]).await?;
    let base = task_git(workdir, &["merge-base", "HEAD", target.trim()]).await?;
    let range = [
        "diff",
        "--name-only",
        "--no-renames",
        "--ignore-submodules=none",
        base.trim(),
        "HEAD",
    ];
    let all = task_git(workdir, &range).await?;
    let inside = task_git(workdir, &[&range[..], &["--", "."]].concat()).await?;
    let inside: std::collections::HashSet<&str> = inside.lines().collect();
    Ok(all
        .lines()
        .filter(|path| !inside.contains(path))
        .map(str::to_string)
        .collect())
}

/// 任务分支现在指向哪个提交。
pub async fn head(workdir: &Workdir) -> Result<String, String> {
    Ok(task_git(workdir, &["rev-parse", "HEAD"])
        .await?
        .trim()
        .to_string())
}

/// 往 `origin` 推送时东西会去的所有地址（一个远端可以配好几个推送地址，`git push` 每个都推）。
/// 没有这个远端时返回给人看的原因。
pub async fn origin_urls(workdir: &Workdir) -> Result<Vec<String>, String> {
    let listed = git(
        Path::new(&workdir.source),
        &["remote", "get-url", "--push", "--all", "origin"],
    )
    .await;
    let urls: Vec<String> = listed
        .unwrap_or_default()
        .lines()
        .map(|url| url.trim().to_string())
        .filter(|url| !url.is_empty())
        .collect();
    if urls.is_empty() {
        return Err("这个仓库没有叫 origin 的远端，没有地方可以推送".into());
    }
    Ok(urls)
}

/// 开 PR 时默认合到哪个分支：人所在分支跟踪的远端分支；没有就用远端的默认分支；再没有就用人所在的分支名。
pub async fn default_base(workdir: &Workdir) -> Option<String> {
    let source = Path::new(&workdir.source);
    let short = |name: String| name.trim().strip_prefix("origin/").map(str::to_string);
    let upstream = &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"];
    if let Some(base) = git(source, upstream).await.ok().and_then(short) {
        return Some(base);
    }
    let remote_head = &["symbolic-ref", "--short", "-q", "refs/remotes/origin/HEAD"];
    if let Some(base) = git(source, remote_head).await.ok().and_then(short) {
        return Some(base);
    }
    let current = git(source, &["symbolic-ref", "--short", "-q", "HEAD"]).await;
    current
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

/// 推送最多等这么久。
const PUSH_TIMEOUT: Duration = Duration::from_secs(120);

/// 把任务分支推到 `origin`。在人的仓库里跑，用的是那个仓库自己的远端和凭据；
/// 要输密码时不等（没有终端可以输），直接算失败。
pub async fn push(workdir: &Workdir, cancel: &CancellationToken) -> Result<(), String> {
    let Some(branch) = &workdir.branch else {
        return Err("这个任务没有分支（工作目录不是 git 仓库），没有东西可以推送".into());
    };
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    let mut command = command();
    command
        .arg("-C")
        .arg(plain(Path::new(&workdir.source)))
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["push", "origin", &refspec])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // 推送会跑人仓库里的钩子（比如 pre-push 里跑测试）：提前结束时要连它们一起结束。
    own_group(&mut command);
    let child = command.spawn().map_err(launch_error)?;
    let pid = child.id();
    let mut tree = TreeGuard(pid);
    let run = child.wait_with_output();
    tokio::pin!(run);
    let gave_up = tokio::select! {
        output = &mut run => {
            tree.0 = None;
            return outcome(output.map_err(launch_error)?, "push").map(drop);
        }
        _ = tokio::time::sleep(PUSH_TIMEOUT) => {
            format!("推送等了 {} 秒还没完成，先放弃了", PUSH_TIMEOUT.as_secs())
        }
        _ = cancel.cancelled() => "推送被停下了，不确定有没有推上去".to_string(),
    };
    kill_tree(pid).await;
    tree.0 = None;
    Err(gave_up)
}

/// 任务结束后删掉它的独立目录。分支留着。删不掉只记日志，不影响任务状态。
pub async fn remove(task_id: &TaskId, workdir: &Workdir) {
    let root = PathBuf::from(&workdir.root);
    if workdir.kind == KIND_WORKTREE && root.exists() {
        let removed = git(
            Path::new(&workdir.source),
            &["worktree", "remove", "--force", &workdir.root],
        )
        .await;
        if let Err(error) = removed {
            tracing::warn!(task = %task_id, "worktree 没删掉：{error}");
        }
    }
    // 复制出来的任务目录，git 目录在它旁边，一起删。worktree 的 git 目录在人的仓库里，归 git 管。
    let mut leftovers = vec![root];
    if workdir.kind == KIND_COPY {
        leftovers.push(PathBuf::from(&workdir.git_dir));
    }
    for path in leftovers.into_iter().filter(|path| path.exists()) {
        let gone = tokio::task::spawn_blocking(move || force_remove(&path)).await;
        if let Ok(Err(error)) = gone {
            tracing::warn!(task = %task_id, "任务目录没删掉：{error}");
        }
    }
}

/// 删掉整个目录。git 的对象文件是只读的，Windows 上只读文件删不掉，所以删不动时先去掉只读再试一次。
fn force_remove(root: &Path) -> std::io::Result<()> {
    if std::fs::remove_dir_all(root).is_ok() {
        return Ok(());
    }
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir)?.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                pending.push(entry.path());
            } else if meta.permissions().readonly() {
                let mut permissions = meta.permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                permissions.set_readonly(false);
                let _ = std::fs::set_permissions(entry.path(), permissions);
            }
        }
    }
    std::fs::remove_dir_all(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn repo(dir: &Path) {
        git(dir, &["init", "-q", "-b", "main"]).await.unwrap();
        std::fs::write(dir.join("a.txt"), "一\n").unwrap();
        // 测试里逐字节比较文件内容：不让 git 转换换行符（Windows 上默认会把 LF 签出成 CRLF）。
        std::fs::write(dir.join(".gitattributes"), "* -text\n").unwrap();
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
        // 名字全是空格的文件也是改动，照样提交。
        #[cfg(unix)]
        {
            std::fs::write(dir.join("   "), "空格\n").unwrap();
            let changed = commit_and_diff(&workdir, "修 bug").await.unwrap();
            assert!(changed.iter().any(|(name, _)| name == "   "), "{changed:?}");
            let status = git(&dir, &["status", "--porcelain"]).await.unwrap();
            assert!(status.trim().is_empty(), "{status}");
            git(&dir, &["rm", "-q", "--", "   "]).await.unwrap();
            git(&dir, &["commit", "-q", "-m", "去掉"]).await.unwrap();
        }
        // 人的工作目录在验收之前不受影响。
        assert_eq!(
            std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
            "一\n"
        );

        apply_to_workspace(&workdir).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
            "二\n"
        );
        assert!(workspace.join("b.txt").exists());

        remove(&task_id, &workdir).await;
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

        let error = apply_to_workspace(&workdir).await.unwrap_err();
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
        std::fs::write(workspace.join("crlf.txt"), "one\r\ntwo\r\n").unwrap();
        let task_id = TaskId::new();

        let workdir = provision(&workspace, &work_root, &task_id).await.unwrap();
        assert_eq!(workdir.kind, KIND_COPY);
        assert_eq!(workdir.branch, None);
        let dir = PathBuf::from(&workdir.path);
        std::fs::write(dir.join("a.txt"), "二\n").unwrap();
        std::fs::remove_file(dir.join("sub/old.txt")).unwrap();
        std::fs::write(dir.join("sub/new.txt"), "新的\n").unwrap();
        std::fs::write(dir.join("crlf.txt"), "one\r\nTWO\r\n").unwrap();
        let changed = commit_and_diff(&workdir, "整理").await.unwrap();
        assert_eq!(changed.len(), 4, "{changed:?}");
        assert!(changed.contains(&("sub/old.txt".to_string(), "删除".to_string())));

        apply_to_workspace(&workdir).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
            "二\n"
        );
        assert!(!workspace.join("sub/old.txt").exists());
        assert!(workspace.join("sub/new.txt").exists());
        // 人的工作目录不会因此变成 git 仓库。
        assert!(!workspace.join(".git").exists());

        // 换行符原样带回来（Windows 上的文件多半是 CRLF）。
        assert_eq!(
            std::fs::read(workspace.join("crlf.txt")).unwrap(),
            b"one\r\nTWO\r\n"
        );

        // 同一份改动再打一次打不上：不会悄悄改坏文件。
        assert!(apply_to_workspace(&workdir).await.is_err());
        remove(&task_id, &workdir).await;
        assert!(!dir.exists());
    }

    /// 人自己正合并到一半：验收不碰它，他解决到一半的冲突还在。
    #[tokio::test]
    async fn accepting_never_aborts_a_merge_the_person_is_in_the_middle_of() {
        let tmp = tempfile::tempdir().unwrap();
        let (workspace, work_root) = (tmp.path().join("ws"), tmp.path().join("work"));
        std::fs::create_dir_all(&workspace).unwrap();
        repo(&workspace).await;
        let workdir = provision(&workspace, &work_root, &TaskId::new())
            .await
            .unwrap();
        std::fs::write(Path::new(&workdir.path).join("b.txt"), "任务加的\n").unwrap();
        commit_and_diff(&workdir, "任务").await.unwrap();

        // 人这边：两条分支改了同一行，合并时冲突，停在半路。
        git(&workspace, &["checkout", "-q", "-b", "other"])
            .await
            .unwrap();
        std::fs::write(workspace.join("a.txt"), "那边\n").unwrap();
        git(&workspace, &["commit", "-q", "-am", "那边"])
            .await
            .unwrap();
        git(&workspace, &["checkout", "-q", "main"]).await.unwrap();
        std::fs::write(workspace.join("a.txt"), "这边\n").unwrap();
        git(&workspace, &["commit", "-q", "-am", "这边"])
            .await
            .unwrap();
        assert!(git(&workspace, &["merge", "other"]).await.is_err());
        std::fs::write(workspace.join("a.txt"), "人解决到一半\n").unwrap();

        let error = apply_to_workspace(&workdir).await.unwrap_err();
        assert!(error.contains("没完成的合并"), "{error}");
        assert!(merging(&workspace).await);
        assert_eq!(
            std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
            "人解决到一半\n"
        );
    }

    /// 复制出来的任务目录：不是 UTF-8 的文件、被忽略的文件，都原样带回去；
    /// 工作目录在一个还没有提交的仓库的某一层时，补丁也打在工作目录上。
    #[tokio::test]
    async fn a_copy_brings_back_odd_encodings_and_ignored_files_even_inside_an_empty_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let (outer, work_root) = (tmp.path().join("repo"), tmp.path().join("work"));
        let workspace = outer.join("sub");
        std::fs::create_dir_all(&workspace).unwrap();
        // 外面一层是刚 init、还没有提交的仓库。
        git(&outer, &["init", "-q"]).await.unwrap();
        // 「中文」的 GBK 编码，不是合法的 UTF-8。
        std::fs::write(workspace.join("gbk.txt"), b"\xd6\xd0\xce\xc4\n").unwrap();
        std::fs::write(workspace.join(".gitignore"), "out/\n").unwrap();
        let task_id = TaskId::new();

        let workdir = provision(&workspace, &work_root, &task_id).await.unwrap();
        assert_eq!(workdir.kind, KIND_COPY);
        let dir = PathBuf::from(&workdir.path);
        // 任务目录里没有 git 的内部文件：它在 Bot 写不到的地方。
        assert!(!dir.join(".git").exists());
        std::fs::write(dir.join("gbk.txt"), b"\xd6\xd0\xce\xc4\xb8\xc4\n").unwrap();
        std::fs::create_dir_all(dir.join("out")).unwrap();
        std::fs::write(dir.join("out/report.txt"), "结果\n").unwrap();
        let changed = commit_and_diff(&workdir, "任务").await.unwrap();
        assert_eq!(changed.len(), 2, "{changed:?}");

        apply_to_workspace(&workdir).await.unwrap();
        assert_eq!(
            std::fs::read(workspace.join("gbk.txt")).unwrap(),
            b"\xd6\xd0\xce\xc4\xb8\xc4\n"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("out/report.txt")).unwrap(),
            "结果\n"
        );
        remove(&task_id, &workdir).await;
        assert!(!dir.exists());
        assert!(!Path::new(&workdir.git_dir).exists());
    }

    /// 任务里的 Bot 往任务目录里放一个假的 `.git`（配置里带着要执行的命令）：
    /// 自动记改动时不认它，命令不会被执行，改动照样算得出来。
    #[cfg(unix)]
    #[tokio::test]
    async fn a_planted_git_dir_is_never_trusted() {
        let tmp = tempfile::tempdir().unwrap();
        for with_repo in [false, true] {
            let name = if with_repo { "git" } else { "plain" };
            let (workspace, work_root) = (tmp.path().join(name), tmp.path().join("work"));
            std::fs::create_dir_all(&workspace).unwrap();
            if with_repo {
                repo(&workspace).await;
            } else {
                std::fs::write(workspace.join("a.txt"), "一\n").unwrap();
            }
            let workdir = provision(&workspace, &work_root, &TaskId::new())
                .await
                .unwrap();
            let dir = PathBuf::from(&workdir.path);

            // 一个真的仓库，配置里让 git 每次看状态都执行一条命令；再让 `.git` 指向它。
            // 在任务目录外面建好，再整个挪进去：任务目录里不跑任何靠 git 自己找仓库的命令。
            let outside = tmp.path().join(format!("evil-{name}"));
            std::fs::create_dir_all(&outside).unwrap();
            git(&outside, &["init", "-q", "--bare"]).await.unwrap();
            let evil = dir.join("evil");
            std::fs::rename(&outside, &evil).unwrap();
            let config = "[core]\n\tbare = false\n\tfsmonitor = touch pwned\n";
            std::fs::write(evil.join("config"), config).unwrap();
            let _ = std::fs::remove_file(dir.join(".git"));
            std::fs::write(dir.join(".git"), "gitdir: evil\n").unwrap();
            std::fs::write(dir.join("a.txt"), "二\n").unwrap();

            let changed = commit_and_diff(&workdir, "任务").await.unwrap();
            assert!(
                changed.contains(&("a.txt".to_string(), "修改".to_string())),
                "{name}: {changed:?}"
            );
            // 用的还是真正的 git 目录，不是假的那个。（不靠「命令有没有被执行」来判断：
            // 开发机上别的程序看到新的 `.git` 也可能自己去跑 git。）
            let used = task_git(&workdir, &["rev-parse", "--absolute-git-dir"])
                .await
                .unwrap();
            assert_eq!(
                Path::new(used.trim()).canonicalize().unwrap(),
                Path::new(&workdir.git_dir).canonicalize().unwrap(),
                "{name}"
            );
        }
    }

    /// 工作目录是仓库里的一层：别的层里有没提交的文件（比如跑测试留下的），不影响记改动。
    #[tokio::test]
    async fn untracked_files_outside_a_subdirectory_workspace_do_not_block_the_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let (outer, work_root) = (tmp.path().join("repo"), tmp.path().join("work"));
        let workspace = outer.join("sub");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("b.txt"), "一\n").unwrap();
        repo(&outer).await;
        let workdir = provision(&workspace, &work_root, &TaskId::new())
            .await
            .unwrap();
        assert!(workdir.path.ends_with("sub"), "{workdir:?}");

        std::fs::write(Path::new(&workdir.path).join("b.txt"), "二\n").unwrap();
        let changed = commit_and_diff(&workdir, "任务").await.unwrap();
        assert_eq!(changed, [("sub/b.txt".to_string(), "修改".to_string())]);
        // 之后在任务目录的上一层冒出一个没提交的文件。
        std::fs::write(Path::new(&workdir.root).join("leftover.log"), "x\n").unwrap();
        assert_eq!(commit_and_diff(&workdir, "任务").await.unwrap(), changed);

        assert!(outside_changes(&workdir).await.unwrap().is_empty());

        apply_to_workspace(&workdir).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(workspace.join("b.txt")).unwrap(),
            "二\n"
        );

        // 在上一层提交了改动（比如 harness 自己 git commit）：合并会带进去，要报出来。
        std::fs::write(Path::new(&workdir.root).join("a.txt"), "外面\n").unwrap();
        task_git(&workdir, &["add", ":/a.txt"]).await.unwrap();
        task_git(&workdir, &["commit", "-q", "-m", "外面"])
            .await
            .unwrap();
        assert_eq!(outside_changes(&workdir).await.unwrap(), ["a.txt"]);

        // 人的工作目录里也改了外面的文件，分支变基上去再改里面的：外面那些不算分支的。
        task_git(&workdir, &["reset", "-q", "--hard", "HEAD~1"])
            .await
            .unwrap();
        std::fs::write(outer.join("c.txt"), "人改的\n").unwrap();
        git(&outer, &["add", "c.txt"]).await.unwrap();
        git(&outer, &["commit", "-q", "-m", "人改的"])
            .await
            .unwrap();
        let main = git(&outer, &["rev-parse", "HEAD"]).await.unwrap();
        task_git(&workdir, &["rebase", "-q", main.trim()])
            .await
            .unwrap();
        std::fs::write(Path::new(&workdir.path).join("b.txt"), "三\n").unwrap();
        commit_and_diff(&workdir, "任务").await.unwrap();
        assert!(outside_changes(&workdir).await.unwrap().is_empty());
    }

    /// 一个有 `origin` 的仓库和它的任务目录，任务分支上有一个提交。`origin` 另有第二个推送地址。
    async fn pushable(tmp: &Path) -> (PathBuf, Workdir, [PathBuf; 2]) {
        let (workspace, work_root) = (tmp.join("ws"), tmp.join("work"));
        let origins = [tmp.join("one.git"), tmp.join("two.git")];
        std::fs::create_dir_all(&workspace).unwrap();
        repo(&workspace).await;
        for origin in &origins {
            std::fs::create_dir_all(origin).unwrap();
            git(origin, &["init", "-q", "--bare", "-b", "main"])
                .await
                .unwrap();
        }
        let [one, two] = origins.each_ref().map(|path| path.to_str().unwrap());
        git(&workspace, &["remote", "add", "origin", one])
            .await
            .unwrap();
        for url in [one, two] {
            git(
                &workspace,
                &["remote", "set-url", "--add", "--push", "origin", url],
            )
            .await
            .unwrap();
        }
        let workdir = provision(&workspace, &work_root, &TaskId::new())
            .await
            .unwrap();
        std::fs::write(Path::new(&workdir.path).join("b.txt"), "新的\n").unwrap();
        commit_and_diff(&workdir, "加 b.txt").await.unwrap();
        (workspace, workdir, origins)
    }

    /// `git push origin` 会推到 origin 的每一个推送地址：问人的时候要全部列出来。
    #[tokio::test]
    async fn every_address_a_push_goes_to_is_listed() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, workdir, origins) = pushable(tmp.path()).await;
        let listed = origin_urls(&workdir).await.unwrap();
        let expected = origins.each_ref().map(|path| path.to_str().unwrap());
        assert_eq!(listed, expected);

        push(&workdir, &CancellationToken::new()).await.unwrap();
        let branch = format!("refs/heads/{}", workdir.branch.as_deref().unwrap());
        for origin in &origins {
            let pushed = git(origin, &["rev-parse", "--verify", &branch]).await;
            assert_eq!(pushed.unwrap().trim(), head(&workdir).await.unwrap());
        }
    }

    #[tokio::test]
    async fn a_repo_without_origin_has_nowhere_to_push() {
        let tmp = tempfile::tempdir().unwrap();
        let (workspace, workdir, _) = pushable(tmp.path()).await;
        git(&workspace, &["remote", "remove", "origin"])
            .await
            .unwrap();
        assert!(origin_urls(&workdir).await.unwrap_err().contains("origin"));
        assert!(push(&workdir, &CancellationToken::new()).await.is_err());
    }

    /// 推送会跑人仓库里的钩子。停下推送时，钩子启动的进程不能留在后台。
    #[cfg(unix)]
    #[tokio::test]
    async fn stopping_a_push_also_stops_what_its_hooks_started() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let (workspace, workdir, origins) = pushable(tmp.path()).await;
        let started = tmp.path().join("started");
        let hook = workspace.join(".git/hooks/pre-push");
        let script = format!(
            "#!/bin/sh\nsleep 300 &\necho $! > '{}'\nwait\n",
            started.display()
        );
        std::fs::write(&hook, script).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let (seen, pushing) = (started.clone(), workdir.clone());
        let pushed = tokio::spawn(async move { push(&pushing, &cancel).await });
        let child = loop {
            let pid = std::fs::read_to_string(&seen).unwrap_or_default();
            if pid.ends_with('\n') {
                break pid.trim().to_string();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        stop.cancel();
        assert!(pushed.await.unwrap().unwrap_err().contains("停下"));

        let alive = || {
            std::process::Command::new("kill")
                .args(["-0", &child])
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        for _ in 0..200 {
            if !alive() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!alive(), "钩子启动的进程 {child} 还在跑");
        let branch = format!("refs/heads/{}", workdir.branch.as_deref().unwrap());
        assert!(git(&origins[0], &["rev-parse", "--verify", &branch])
            .await
            .is_err());
    }
}
