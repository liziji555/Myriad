use crate::walk::walk;
use crate::{arg, blocking, parse_input, Tool, ToolCtx, ToolError, ToolResult};
use async_trait::async_trait;
use globset::GlobBuilder;
use serde::Deserialize;
use serde_json::{json, Value};

const MAX_RESULTS: usize = 200;

pub struct GlobTool;

#[derive(Deserialize)]
struct Input {
    pattern: String,
    #[serde(default)]
    path: String,
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> String {
        format!(
            "按名字在工作目录里找文件，例如 **/*.rs、src/**/*.{{ts,tsx}}、**/Cargo.toml。\
             模式相对搜索起点，* 不跨目录，** 跨目录。遵守 .gitignore，不包含隐藏文件。最多返回 {MAX_RESULTS} 个路径。"
        )
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "glob 模式" },
                "path": { "type": "string", "description": "从哪个目录开始找，相对工作目录；不填就是工作目录" }
            },
            "required": ["pattern"]
        })
    }

    fn title(&self, input: &Value) -> String {
        format!("查找文件 {}", arg(input, "pattern"))
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
        let cancel = ctx.cancel.clone();
        blocking(move || {
            let start = fence.resolve(&input.path)?;
            if !start.is_dir() {
                return Err(ToolError(format!("{} 不是目录。", fence.display(&start))));
            }
            let matcher = GlobBuilder::new(&input.pattern)
                .literal_separator(true)
                .build()
                .map_err(|e| ToolError(format!("glob 模式不对：{e}")))?
                .compile_matcher();
            let mut found = vec![];
            let mut more = false;
            for entry in walk(&fence, &start) {
                if cancel.is_cancelled() {
                    return Err(ToolError::new("已停止"));
                }
                let relative = entry.path.strip_prefix(&start).unwrap_or(&entry.path);
                if entry.is_dir || !matcher.is_match(relative) {
                    continue;
                }
                if found.len() == MAX_RESULTS {
                    more = true;
                    break;
                }
                found.push(fence.display(&entry.path));
            }
            if found.is_empty() {
                return Ok(format!(
                    "没有文件匹配 {}。注意：被 .gitignore 忽略的文件和隐藏文件不在搜索范围内。",
                    input.pattern
                ));
            }
            let mut out = found.join("\n");
            if more {
                out.push_str(&format!(
                    "\n\n（结果超过 {MAX_RESULTS} 个，只列出前 {MAX_RESULTS} 个。请把模式写得更具体）"
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

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for file in [
            "Cargo.toml",
            "src/main.rs",
            "src/util/mod.rs",
            "web/app.tsx",
            "target/debug/build.rs",
            ".git/config.rs",
            "data/state.rs",
        ] {
            let path = dir.path().join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
        }
        fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        dir
    }

    #[tokio::test]
    async fn matches_respecting_gitignore_hidden_and_fence() {
        let dir = tree();
        let fence = Fence::new(dir.path(), &[dir.path().join("data")]).unwrap();
        let ctx = ToolCtx::new(fence);
        let glob = |input| async { GlobTool.call(input, &ctx).await.unwrap() };

        assert_eq!(
            glob(json!({"pattern": "**/*.rs"})).await,
            "src/main.rs\nsrc/util/mod.rs"
        );
        assert_eq!(glob(json!({"pattern": "*.toml"})).await, "Cargo.toml");
        assert_eq!(
            glob(json!({"pattern": "*.rs", "path": "src"})).await,
            "src/main.rs"
        );
        assert_eq!(
            glob(json!({"pattern": "**/*.{rs,tsx}", "path": "."})).await,
            "src/main.rs\nsrc/util/mod.rs\nweb/app.tsx"
        );
        assert!(glob(json!({"pattern": "**/*.py"}))
            .await
            .contains("没有文件匹配"));

        let bad = GlobTool.call(json!({"pattern": "[x"}), &ctx).await;
        assert!(bad.unwrap_err().0.contains("glob 模式不对"));
        let outside = GlobTool
            .call(json!({"pattern": "*", "path": ".."}), &ctx)
            .await;
        assert!(outside.is_err());
    }

    #[tokio::test]
    async fn caps_the_number_of_results() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..MAX_RESULTS + 3 {
            fs::write(dir.path().join(format!("f{i:04}.txt")), "").unwrap();
        }
        let ctx = ToolCtx::new(Fence::new(dir.path(), &[]).unwrap());
        let out = GlobTool
            .call(json!({"pattern": "*.txt"}), &ctx)
            .await
            .unwrap();
        assert_eq!(
            out.lines().filter(|l| l.ends_with(".txt")).count(),
            MAX_RESULTS
        );
        assert!(out.contains("结果超过"));
    }
}
