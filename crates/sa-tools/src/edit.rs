use crate::write::{file_lock, write_atomically, write_permission, STOPPED};
use crate::{arg, blocking, parse_input, Permission, Tool, ToolCtx, ToolError, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;

pub struct EditTool;

#[derive(Deserialize)]
struct Input {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> String {
        "修改一个已有的文本文件：把 old_string 换成 new_string。old_string 要和文件里的内容一字不差\
         （包括缩进和换行），并且在文件里只出现一次；出现多次时要么多带几行上下文让它唯一，\
         要么把 replace_all 设为 true 全部替换。改之前先用 read 看过文件。"
            .into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "文件路径，相对工作目录" },
                "old_string": { "type": "string", "description": "要被替换的原文" },
                "new_string": { "type": "string", "description": "替换成的内容" },
                "replace_all": { "type": "boolean", "description": "替换所有出现的地方，默认 false" }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    fn title(&self, input: &Value) -> String {
        format!("修改 {}", arg(input, "path"))
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
        if input.old_string.is_empty() {
            return Err(ToolError::new(
                "old_string 不能为空。要新建文件或整个重写，请用 write。",
            ));
        }
        if input.old_string == input.new_string {
            return Err(ToolError::new(
                "old_string 和 new_string 一样，没有东西可改。",
            ));
        }
        let (path, _) = ctx.fence.resolve_for_write(&input.path)?;
        let shown = input.path.trim().to_string();
        let cancel = ctx.cancel.clone();
        let fence = ctx.fence.clone();
        let checkpoint = ctx.checkpoint.clone();
        blocking(move || {
            // 从读到写回都拿着锁：别的写入不会夹在中间。
            let _lock = file_lock();
            let io_error = |e: std::io::Error| match e.kind() {
                std::io::ErrorKind::NotFound => {
                    ToolError(format!("{shown} 不存在。要新建文件请用 write。"))
                }
                _ => ToolError(format!("改不了 {shown}：{e}")),
            };
            let meta = std::fs::metadata(&path).map_err(io_error)?;
            if !meta.is_file() {
                return Err(ToolError(format!("{shown} 不是普通文件，改不了。")));
            }
            if meta.len() > MAX_FILE_BYTES {
                return Err(ToolError(format!("{shown} 超过 20 MB，太大了改不了。")));
            }
            let bytes = std::fs::read(&path).map_err(io_error)?;
            let Ok(text) = String::from_utf8(bytes) else {
                return Err(ToolError(format!("{shown} 不是 UTF-8 文本文件，改不了。")));
            };
            let count = text.matches(&input.old_string).count();
            if count == 0 {
                return Err(ToolError(format!(
                    "在 {shown} 里没有找到 old_string。它必须和文件内容一字不差（包括缩进和换行），\
                     请先用 read 看一下现在的内容。"
                )));
            }
            if count > 1 && !input.replace_all {
                return Err(ToolError(format!(
                    "old_string 在 {shown} 里出现了 {count} 次。请多带几行上下文让它只出现一次，\
                     或者把 replace_all 设为 true。"
                )));
            }
            let updated = text.replace(&input.old_string, &input.new_string);
            if cancel.is_cancelled() {
                return Err(ToolError::new(STOPPED));
            }
            write_atomically(&path, updated.as_bytes()).map_err(io_error)?;
            if let Some(cp) = checkpoint {
                cp.record_write(
                    &fence,
                    &path,
                    crate::checkpoint::BeforeState::Bytes(text.as_bytes()),
                    updated.as_bytes(),
                );
            }
            Ok(format!("已修改 {shown}（替换了 {count} 处）。"))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Fence;
    use std::fs;

    fn rig() -> (tempfile::TempDir, ToolCtx) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "fn main() {\n    let x = 1;\n    let y = 1;\n}\n",
        )
        .unwrap();
        let ctx = ToolCtx::new(Fence::new(dir.path(), &[]).unwrap());
        (dir, ctx)
    }

    #[tokio::test]
    async fn replaces_a_unique_match_and_keeps_the_rest() {
        let (dir, ctx) = rig();
        let input =
            json!({"path": "a.rs", "old_string": "let x = 1;", "new_string": "let x = 42;"});
        assert_eq!(
            EditTool.check_permission(&input, &ctx).await,
            Permission::Allow
        );
        let out = EditTool.call(input, &ctx).await.unwrap();
        assert_eq!(out, "已修改 a.rs（替换了 1 处）。");
        assert_eq!(
            fs::read_to_string(dir.path().join("a.rs")).unwrap(),
            "fn main() {\n    let x = 42;\n    let y = 1;\n}\n"
        );
    }

    #[tokio::test]
    async fn ambiguous_matches_need_replace_all() {
        let (dir, ctx) = rig();
        let ambiguous = json!({"path": "a.rs", "old_string": "= 1;", "new_string": "= 2;"});
        let error = EditTool.call(ambiguous, &ctx).await.unwrap_err().0;
        assert!(error.contains("出现了 2 次"), "{error}");
        let all = json!({"path": "a.rs", "old_string": "= 1;", "new_string": "= 2;", "replace_all": true});
        assert_eq!(
            EditTool.call(all, &ctx).await.unwrap(),
            "已修改 a.rs（替换了 2 处）。"
        );
        let text = fs::read_to_string(dir.path().join("a.rs")).unwrap();
        assert_eq!(text.matches("= 2;").count(), 2);
    }

    #[tokio::test]
    async fn explains_what_is_wrong_without_touching_the_file() {
        let (dir, ctx) = rig();
        let before = fs::read_to_string(dir.path().join("a.rs")).unwrap();
        fs::write(dir.path().join("bin.dat"), [0xff, 0xfe, 0x00]).unwrap();
        let err = |input| async { EditTool.call(input, &ctx).await.unwrap_err().0 };
        let edit = |path: &str, old: &str, new: &str| json!({"path": path, "old_string": old, "new_string": new});

        assert!(err(edit("a.rs", "let z", "x")).await.contains("没有找到"));
        assert!(err(edit("a.rs", "", "x")).await.contains("不能为空"));
        assert!(err(edit("a.rs", "let x", "let x"))
            .await
            .contains("没有东西可改"));
        assert!(err(edit("没有.rs", "a", "b")).await.contains("不存在"));
        assert!(err(edit("bin.dat", "a", "b")).await.contains("UTF-8"));
        assert!(err(json!({"path": "a.rs"})).await.contains("参数不对"));
        assert_eq!(fs::read_to_string(dir.path().join("a.rs")).unwrap(), before);
    }

    #[tokio::test]
    async fn outside_the_workdir_asks_first() {
        let (_dir, ctx) = rig();
        let input = json!({"path": "../x.txt", "old_string": "a", "new_string": "b"});
        assert!(matches!(
            EditTool.check_permission(&input, &ctx).await,
            Permission::Ask(_)
        ));
    }

    /// 同时改同一个文件的不同地方：每一处改动都要留下，不能互相盖掉。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_edits_of_one_file_all_land() {
        let (dir, ctx) = rig();
        let lines: String = (0..40).map(|i| format!("旧{i:02}\n")).collect();
        fs::write(dir.path().join("many.txt"), lines).unwrap();
        let edits = (0..40).map(|i| {
            let ctx = ctx.clone();
            tokio::spawn(async move {
                let input = json!({
                    "path": "many.txt",
                    "old_string": format!("旧{i:02}"),
                    "new_string": format!("新{i:02}"),
                });
                EditTool.call(input, &ctx).await.unwrap()
            })
        });
        for edit in edits.collect::<Vec<_>>() {
            edit.await.unwrap();
        }
        let expected: String = (0..40).map(|i| format!("新{i:02}\n")).collect();
        assert_eq!(
            fs::read_to_string(dir.path().join("many.txt")).unwrap(),
            expected
        );
    }

    #[tokio::test]
    async fn a_stopped_edit_changes_nothing() {
        let (dir, ctx) = rig();
        let before = fs::read_to_string(dir.path().join("a.rs")).unwrap();
        ctx.cancel.cancel();
        let input = json!({"path": "a.rs", "old_string": "let x = 1;", "new_string": "let x = 2;"});
        let error = EditTool.call(input, &ctx).await.unwrap_err();
        assert!(error.0.contains("已停止"), "{}", error.0);
        assert_eq!(fs::read_to_string(dir.path().join("a.rs")).unwrap(), before);
    }
}
