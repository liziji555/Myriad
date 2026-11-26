//! 原子批量编辑工具：一次调用对同一个文件顺序执行多处替换，任意一处不匹配就全部回滚不落盘。

use crate::write::{file_lock, write_atomically, write_permission, STOPPED};
use crate::{arg, blocking, parse_input, Permission, Tool, ToolCtx, ToolError, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;
const MAX_EDITS_PER_CALL: usize = 50;

pub struct MultiEditTool;

#[derive(Deserialize)]
struct EditOp {
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[derive(Deserialize)]
struct Input {
    path: String,
    edits: Vec<EditOp>,
}

#[async_trait]
impl Tool for MultiEditTool {
    fn name(&self) -> &'static str {
        "multi_edit"
    }

    fn description(&self) -> String {
        "对同一个文本文件一次性原子执行多处修改（按顺序依次应用 edits）。\
         如果其中任何一处 old_string 找不到或不唯一，整个文件不会被改动，避免写到一半出错。\
         比连续多次调用 edit 更省步数、更可靠。改之前先用 read 看过文件。"
            .into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "文件路径，相对工作目录" },
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_EDITS_PER_CALL,
                    "description": "按顺序依次执行的编辑列表",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": { "type": "string", "description": "要被替换的原文" },
                            "new_string": { "type": "string", "description": "替换成的内容" },
                            "replace_all": { "type": "boolean", "description": "是否替换全部出现，默认 false" }
                        },
                        "required": ["old_string", "new_string"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }

    fn title(&self, input: &Value) -> String {
        let count = input
            .get("edits")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if count > 0 {
            format!("批量修改 {}（{count} 处）", arg(input, "path"))
        } else {
            format!("批量修改 {}", arg(input, "path"))
        }
    }

    async fn check_permission(&self, input: &Value, ctx: &ToolCtx) -> Permission {
        write_permission(arg(input, "path"), ctx)
    }

    fn finishes_once_started(&self) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolResult {
        let input: Input = parse_input(input)?;
        if input.edits.is_empty() {
            return Err(ToolError::new("edits 不能为空，至少要提供一处修改。"));
        }
        if input.edits.len() > MAX_EDITS_PER_CALL {
            return Err(ToolError(format!(
                "单次 multi_edit 最多包含 {MAX_EDITS_PER_CALL} 处修改，请分批执行。"
            )));
        }
        for (idx, op) in input.edits.iter().enumerate() {
            let n = idx + 1;
            if op.old_string.is_empty() {
                return Err(ToolError(format!(
                    "第 {n} 处修改的 old_string 不能为空。要新建或整体重写请用 write。"
                )));
            }
            if op.old_string == op.new_string {
                return Err(ToolError(format!(
                    "第 {n} 处修改的 old_string 和 new_string 相同，没有需要改动的内容。"
                )));
            }
        }
        let (path, _) = ctx.fence.resolve_for_write(&input.path)?;
        let shown = input.path.trim().to_string();
        let cancel = ctx.cancel.clone();
        let fence = ctx.fence.clone();
        let checkpoint = ctx.checkpoint.clone();
        blocking(move || {
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
            let Ok(original) = String::from_utf8(bytes) else {
                return Err(ToolError(format!("{shown} 不是 UTF-8 文本文件，改不了。")));
            };
            let mut current = original.clone();
            let before_lines = current.lines().count();
            let mut total_replacements = 0usize;
            for (idx, op) in input.edits.iter().enumerate() {
                let n = idx + 1;
                let count = current.matches(&op.old_string).count();
                if count == 0 {
                    return Err(ToolError(format!(
                        "第 {n} 处修改在 {shown} 里没有找到 old_string，已取消整批修改（文件未被改动）。请先用 read 核对内容。"
                    )));
                }
                if count > 1 && !op.replace_all {
                    return Err(ToolError(format!(
                        "第 {n} 处修改的 old_string 在 {shown} 里出现了 {count} 次，已取消整批修改（文件未被改动）。请增加上下文或设 replace_all: true。"
                    )));
                }
                current = current.replace(&op.old_string, &op.new_string);
                total_replacements += count;
            }
            if cancel.is_cancelled() {
                return Err(ToolError::new(STOPPED));
            }
            let after_lines = current.lines().count();
            write_atomically(&path, current.as_bytes()).map_err(io_error)?;
            if let Some(cp) = checkpoint {
                cp.record_write(&fence, &path, Some(original.as_bytes()), current.as_bytes());
            }
            Ok(format!(
                "已批量修改 {shown}（{} 组编辑、共替换 {total_replacements} 处，行数 {before_lines} → {after_lines}）。",
                input.edits.len()
            ))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Fence;
    use std::fs;

    #[tokio::test]
    async fn applies_multiple_edits_atomically() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("demo.rs"),
            "fn a() { let x = 1; }\nfn b() { let y = 2; }\n",
        )
        .unwrap();
        let ctx = ToolCtx::new(Fence::new(dir.path(), &[]).unwrap());
        let out = MultiEditTool
            .call(
                json!({
                    "path": "demo.rs",
                    "edits": [
                        { "old_string": "let x = 1;", "new_string": "let x = 10;" },
                        { "old_string": "let y = 2;", "new_string": "let y = 20;" }
                    ]
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("2 组编辑"), "{out}");
        assert_eq!(
            fs::read_to_string(dir.path().join("demo.rs")).unwrap(),
            "fn a() { let x = 10; }\nfn b() { let y = 20; }\n"
        );
    }

    #[tokio::test]
    async fn rolls_back_entire_batch_if_later_edit_fails() {
        let dir = tempfile::tempdir().unwrap();
        let original = "let a = 1;\nlet b = 2;\n";
        fs::write(dir.path().join("demo.rs"), original).unwrap();
        let ctx = ToolCtx::new(Fence::new(dir.path(), &[]).unwrap());
        let err = MultiEditTool
            .call(
                json!({
                    "path": "demo.rs",
                    "edits": [
                        { "old_string": "let a = 1;", "new_string": "let a = 99;" },
                        { "old_string": "let z = 3;", "new_string": "let z = 4;" }
                    ]
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.0.contains("第 2 处修改"), "{}", err.0);
        assert_eq!(
            fs::read_to_string(dir.path().join("demo.rs")).unwrap(),
            original
        );
    }
}
