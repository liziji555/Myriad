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

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolResult {
        let input: Input = parse_input(input)?;
        if input.content.len() > MAX_BYTES {
            return Err(ToolError::new(
                "内容超过 2 MB，太大了。请分成几个文件，或者只写需要的部分。",
            ));
        }
        let (path, _) = ctx.fence.resolve_for_write(&input.path)?;
        let shown = input.path.trim().to_string();
        blocking(move || {
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
        Ok((_, true)) => Permission::Allow,
        Ok((real, false)) => {
            Permission::Ask(format!("要写的文件在工作目录之外：{}", real.display()))
        }
        Err(error) => Permission::Deny(error.0),
    }
}

/// 先写到同目录的临时文件再改名：中途失败不会留下写了一半的文件。
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("路径没有上级目录"))?;
    std::fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("路径没有文件名"))?
        .to_string_lossy();
    let temp = dir.join(format!(".{name}.sa-tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        // 覆盖已有文件时保留它原来的权限（比如可执行位）。
        if let Ok(meta) = std::fs::metadata(path) {
            std::fs::set_permissions(&temp, meta.permissions())?;
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
}
