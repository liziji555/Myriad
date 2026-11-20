use crate::{arg, blocking, parse_input, Tool, ToolCtx, ToolError, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

const MAX_ENTRIES: usize = 500;

pub struct ListDirTool;

#[derive(Deserialize)]
struct Input {
    #[serde(default)]
    path: String,
}

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &'static str {
        "list_dir"
    }

    fn description(&self) -> String {
        format!(
            "列出工作目录里某个目录的直接内容（不递归），目录名以 / 结尾。最多 {MAX_ENTRIES} 项。\
             要按名字在整个目录树里找文件请用 glob。"
        )
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "目录路径，相对工作目录；不填就是工作目录本身" }
            }
        })
    }

    fn title(&self, input: &Value) -> String {
        match arg(input, "path") {
            "" | "." => "列出工作目录".into(),
            path => format!("列出 {path}"),
        }
    }

    fn is_read_only(&self, _: &Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolResult {
        let input: Input = parse_input(input)?;
        let fence = ctx.fence.clone();
        blocking(move || {
            let dir = fence.resolve(&input.path)?;
            let shown = fence.display(&dir);
            if !dir.is_dir() {
                return Err(ToolError(format!("{shown} 不是目录，读文件请用 read。")));
            }
            let entries =
                std::fs::read_dir(&dir).map_err(|e| ToolError(format!("列不了 {shown}：{e}")))?;
            let mut names: Vec<String> = entries
                .filter_map(Result::ok)
                // 不列出围栏不让碰的目录（比如数据目录）。
                .filter(|entry| fence.allows(&entry.path()))
                .map(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    // file_type 不跟符号链接，所以链接按普通条目显示。
                    match entry.file_type() {
                        Ok(kind) if kind.is_dir() => format!("{name}/"),
                        _ => name,
                    }
                })
                .collect();
            if names.is_empty() {
                return Ok(format!("（{shown} 是空目录）"));
            }
            names.sort();
            let total = names.len();
            names.truncate(MAX_ENTRIES);
            let mut out = names.join("\n");
            if total > MAX_ENTRIES {
                out.push_str(&format!(
                    "\n\n（共 {total} 项，只列出前 {MAX_ENTRIES} 项。可以用 glob 缩小范围）"
                ));
            }
            Ok(out)
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
    async fn lists_sorted_entries_and_hides_denied() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::create_dir_all(dir.path().join("data")).unwrap();
        fs::create_dir_all(dir.path().join("empty")).unwrap();
        fs::write(dir.path().join("b.txt"), "").unwrap();
        fs::write(dir.path().join(".hidden"), "").unwrap();
        let fence = Fence::new(dir.path(), &[dir.path().join("data")]).unwrap();
        let ctx = ToolCtx::new(fence);

        let out = ListDirTool.call(json!({}), &ctx).await.unwrap();
        assert_eq!(out, ".hidden\nb.txt\nempty/\nsrc/");
        let empty = ListDirTool.call(json!({"path": "empty"}), &ctx).await;
        assert!(empty.unwrap().contains("空目录"));

        let err = |input| async { ListDirTool.call(input, &ctx).await.unwrap_err().0 };
        assert!(err(json!({"path": "b.txt"})).await.contains("不是目录"));
        assert!(err(json!({"path": "data"})).await.contains("工作目录之外"));
        assert!(err(json!({"path": ".."})).await.contains("工作目录之外"));
    }

    #[tokio::test]
    async fn truncates_huge_directories() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..MAX_ENTRIES + 5 {
            fs::write(dir.path().join(format!("f{i:04}")), "").unwrap();
        }
        let ctx = ToolCtx::new(Fence::new(dir.path(), &[]).unwrap());
        let out = ListDirTool.call(json!({"path": "."}), &ctx).await.unwrap();
        assert!(out.contains("f0499") && !out.contains("f0500\n"));
        assert!(out.contains(&format!("共 {} 项", MAX_ENTRIES + 5)));
    }
}
