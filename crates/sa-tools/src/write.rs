use crate::{arg, blocking, parse_input, Permission, Tool, ToolCtx, ToolError, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;

/// 一次最多写这么多字节。
const MAX_BYTES: usize = 2 * 1024 * 1024;

pub struct WriteTool;

#[derive(Deserialize)]
struct Input {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }

    fn description(&self) -> String {
        "把内容写进一个文本文件：文件不存在就新建（连同缺的上级目录），存在就整个覆盖。\
         只改文件里的一部分时用 edit，不要用它重写整个文件。写工作目录之外的路径需要用户批准。"
            .into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "文件路径，相对工作目录" },
                "content": { "type": "string", "description": "文件的完整内容" }
            },
            "required": ["path", "content"]
        })
    }

    fn title(&self, input: &Value) -> String {
        format!("写入 {}", arg(input, "path"))
    }

    async fn check_permission(&self, input: &Value, ctx: &ToolCtx) -> Permission {
        write_permission(arg(input, "path"), ctx)
    }

    // 写文件一下子就完：已经开始写了，就等它写完再停。
    fn finishes_once_started(&self) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolResult {
        let input: Input = parse_input(input)?;
        if input.content.len() > MAX_BYTES {
            return Err(ToolError::new(
                "内容超过 2 MB，太大了。请分成几个文件，或者只写需要的部分。",
            ));
        }
        let (path, _) = ctx.fence.resolve_for_write(&input.path)?;
        let shown = input.path.trim().to_string();
        let cancel = ctx.cancel.clone();
        blocking(move || {
            let _lock = file_lock();
            if cancel.is_cancelled() {
                return Err(ToolError::new(STOPPED));
            }
            if path.is_dir() {
                return Err(ToolError(format!("{shown} 是目录，不能当文件写。")));
            }
            let existed = path.exists();
            write_atomically(&path, input.content.as_bytes())
                .map_err(|e| ToolError(format!("写不进 {shown}：{e}")))?;
            let lines = input.content.lines().count();
            Ok(format!(
                "已{} {shown}（{lines} 行）。",
                if existed { "覆盖" } else { "新建" }
            ))
        })
        .await
    }
}

/// 写文件类工具共用的判断：工作目录里面直接写，外面先问人，数据目录不让写。
pub(crate) fn write_permission(path: &str, ctx: &ToolCtx) -> Permission {
    match ctx.fence.resolve_for_write(path) {
        // git 的配置里可以写要执行的命令，改它等于换一种方式执行命令，所以和执行命令一样先问人。
        Ok((real, true)) if in_git_dir(&real) => Permission::Ask(format!(
            "要写的是 git 的内部文件（里面的配置能让 git 执行命令）：{}",
            ctx.fence.display(&real)
        )),
        Ok((_, true)) => Permission::Allow,
        Ok((real, false)) => {
            Permission::Ask(format!("要写的文件在工作目录之外：{}", real.display()))
        }
        Err(error) => Permission::Deny(error.0),
    }
}

/// 路径里有没有一段是 `.git`。
fn in_git_dir(real: &std::path::Path) -> bool {
    real.components()
        .any(|part| part.as_os_str().eq_ignore_ascii_case(".git"))
}

pub(crate) const STOPPED: &str = "已停止，文件没有改动。";

/// 写文件、改文件一次只做一个。
///
/// 两个房间同时改同一个文件时，「读出来、改好、写回去」不能交错，否则后写的会盖掉先写的改动。
/// 这些操作都很快，所以不按文件分锁，整个进程共用一把。
pub(crate) fn file_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// 先写到同目录的临时文件再改名：中途失败不会留下写了一半的文件。
///
/// 临时文件名每次都不一样，并且只在它不存在时才创建：
/// 不会和别的写入撞名，也不会顺着别人事先放好的符号链接写到别处去。
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("路径没有上级目录"))?;
    std::fs::create_dir_all(dir)?;
    if path.file_name().is_none() {
        return Err(std::io::Error::other("路径没有文件名"));
    }
    // 名字里不带目标文件名：目标的名字已经到了文件系统的长度上限时，临时文件也建得出来。
    let temp = dir.join(format!(".sa-tmp-{}", uuid::Uuid::new_v4().simple()));
    let original = std::fs::metadata(path).ok().map(|meta| meta.permissions());
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        // 覆盖已有文件时，临时文件先只让自己读写：原文件可能是私有的，换上去之前别人读不到新内容。
        #[cfg(unix)]
        if original.is_some() {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        // 覆盖已有文件时保留它原来的权限（比如可执行位）。
        if let Some(permissions) = original {
            std::fs::set_permissions(&temp, permissions)?;
        }
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Fence;
    use std::fs;

    fn rig() -> (tempfile::TempDir, ToolCtx) {
        let outer = tempfile::tempdir().unwrap();
        fs::create_dir_all(outer.path().join("work/data")).unwrap();
        let fence = Fence::new(
            &outer.path().join("work"),
            &[outer.path().join("work/data")],
        )
        .unwrap();
        (outer, ToolCtx::new(fence))
    }

    #[tokio::test]
    async fn creates_then_overwrites_and_leaves_no_temp_files() {
        let (outer, ctx) = rig();
        let work = outer.path().join("work");
        let input = json!({"path": "src/新文件.txt", "content": "第一行\n第二行\n"});
        assert_eq!(
            WriteTool.check_permission(&input, &ctx).await,
            Permission::Allow
        );
        let out = WriteTool.call(input, &ctx).await.unwrap();
        assert_eq!(out, "已新建 src/新文件.txt（2 行）。");
        assert_eq!(
            fs::read_to_string(work.join("src/新文件.txt")).unwrap(),
            "第一行\n第二行\n"
        );

        let out = WriteTool
            .call(json!({"path": "src/新文件.txt", "content": "换了"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out, "已覆盖 src/新文件.txt（1 行）。");
        assert_eq!(
            fs::read_to_string(work.join("src/新文件.txt")).unwrap(),
            "换了"
        );
        let names: Vec<_> = fs::read_dir(work.join("src"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["新文件.txt"]);
    }

    #[tokio::test]
    async fn outside_asks_and_data_dir_is_refused() {
        let (outer, ctx) = rig();
        let outside = json!({"path": "../外面.txt", "content": "x"});
        match WriteTool.check_permission(&outside, &ctx).await {
            Permission::Ask(reason) => assert!(reason.contains("工作目录之外"), "{reason}"),
            other => panic!("{other:?}"),
        }
        // 人批准之后才会走到 call，这时照写。
        WriteTool.call(outside, &ctx).await.unwrap();
        assert_eq!(
            fs::read_to_string(outer.path().join("外面.txt")).unwrap(),
            "x"
        );

        let secret = json!({"path": "data/token", "content": "x"});
        assert!(matches!(
            WriteTool.check_permission(&secret, &ctx).await,
            Permission::Deny(_)
        ));
        let error = WriteTool.call(secret, &ctx).await.unwrap_err().0;
        assert!(error.contains("数据目录"), "{error}");
        assert!(!outer.path().join("work/data/token").exists());
    }

    #[tokio::test]
    async fn rejects_directories_bad_input_and_huge_content() {
        let (_outer, ctx) = rig();
        let err = |input| async { WriteTool.call(input, &ctx).await.unwrap_err().0 };
        assert!(err(json!({"path": "data/../data", "content": ""}))
            .await
            .contains("数据目录"));
        fs::create_dir(ctx.fence.root().join("dir")).unwrap();
        assert!(err(json!({"path": "dir", "content": ""}))
            .await
            .contains("是目录"));
        assert!(err(json!({"path": "a.txt"})).await.contains("参数不对"));
        let huge = "x".repeat(MAX_BYTES + 1);
        assert!(err(json!({"path": "a.txt", "content": huge}))
            .await
            .contains("2 MB"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlinked_directory_pointing_outside_counts_as_outside() {
        let (outer, ctx) = rig();
        fs::create_dir(outer.path().join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(
            outer.path().join("elsewhere"),
            outer.path().join("work/link"),
        )
        .unwrap();
        let input = json!({"path": "link/a.txt", "content": "x"});
        assert!(matches!(
            WriteTool.check_permission(&input, &ctx).await,
            Permission::Ask(_)
        ));
    }

    /// 同时写同一个文件：最后留下的是其中一份完整的内容，不会混在一起，也不留临时文件。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_writes_never_mix_and_leave_no_temp_files() {
        let (outer, ctx) = rig();
        let writes = (0..24u8).map(|i| {
            let ctx = ctx.clone();
            tokio::spawn(async move {
                let content = char::from(b'a' + i).to_string().repeat(200_000);
                let input = json!({"path": "same.txt", "content": content});
                WriteTool.call(input, &ctx).await.unwrap()
            })
        });
        for write in writes.collect::<Vec<_>>() {
            write.await.unwrap();
        }
        let work = outer.path().join("work");
        let text = fs::read_to_string(work.join("same.txt")).unwrap();
        assert_eq!(text.len(), 200_000);
        let first = text.chars().next().unwrap();
        assert!(text.chars().all(|c| c == first), "两次写入的内容混在一起了");
        let leftovers: Vec<_> = fs::read_dir(&work)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("sa-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// 临时文件只在不存在时创建：事先放在那里的符号链接不会被顺着写出去。
    #[cfg(unix)]
    #[test]
    fn temp_file_is_never_opened_through_an_existing_link() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret.txt");
        fs::write(&secret, "原样").unwrap();
        // 旧的命名方式（进程号）下，这个链接会被当成临时文件打开并截断。
        let planted = dir
            .path()
            .join(format!(".note.md.sa-tmp-{}", std::process::id()));
        std::os::unix::fs::symlink(&secret, &planted).unwrap();
        write_atomically(&dir.path().join("note.md"), b"new").unwrap();
        assert_eq!(fs::read_to_string(&secret).unwrap(), "原样");
        assert_eq!(
            fs::read_to_string(dir.path().join("note.md")).unwrap(),
            "new"
        );
    }

    /// git 的内部文件即使在工作目录里也要先问：改它的配置等于换一种方式执行命令。
    #[test]
    fn writing_git_internals_asks_first() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        let ctx = ToolCtx::new(Fence::new(dir.path(), &[]).unwrap());
        for path in [".git/config", "sub/.git", "sub/.GIT/hooks/pre-commit"] {
            let permission = write_permission(path, &ctx);
            assert!(
                matches!(permission, Permission::Ask(_)),
                "{path}: {permission:?}"
            );
        }
        for path in ["a.txt", ".gitignore", "sub/.github/x.yml"] {
            assert_eq!(write_permission(path, &ctx), Permission::Allow, "{path}");
        }
    }

    /// 文件名已经到了常见文件系统的上限（255 字节）也能写。
    #[cfg(unix)]
    #[test]
    fn longest_allowed_file_names_can_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a".repeat(255));
        write_atomically(&path, b"x").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "x");
    }

    #[tokio::test]
    async fn a_stopped_write_changes_nothing() {
        let (outer, ctx) = rig();
        ctx.cancel.cancel();
        let input = json!({"path": "late.txt", "content": "x"});
        let error = WriteTool.call(input, &ctx).await.unwrap_err();
        assert!(error.0.contains("已停止"), "{}", error.0);
        assert!(!outer.path().join("work/late.txt").exists());
    }
}
