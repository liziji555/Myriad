use crate::walk::walk;
use crate::{arg, blocking, parse_input, Tool, ToolCtx, ToolError, ToolResult};
use async_trait::async_trait;
use globset::GlobBuilder;
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::path::Path;

const DEFAULT_RESULTS: usize = 100;
const MAX_RESULTS: usize = 500;
const MAX_LINE_CHARS: usize = 300;
const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;

pub struct GrepTool;

#[derive(Deserialize)]
struct Input {
    pattern: String,
    #[serde(default)]
    path: String,
    glob: Option<String>,
    #[serde(default)]
    case_insensitive: bool,
    max_results: Option<usize>,
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> String {
        format!(
            "用正则表达式在工作目录的文件内容里搜索，返回「路径:行号: 内容」。path 可以是目录或单个文件。\
             遵守 .gitignore，跳过隐藏文件和二进制文件。默认最多 {DEFAULT_RESULTS} 条结果。"
        )
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "正则表达式（Rust regex 语法）" },
                "path": { "type": "string", "description": "在哪个目录或文件里搜，相对工作目录；不填就是整个工作目录" },
                "glob": { "type": "string", "description": "只搜名字匹配的文件，例如 *.rs 或 **/*.md" },
                "case_insensitive": { "type": "boolean", "description": "忽略大小写" },
                "max_results": { "type": "integer", "minimum": 1, "maximum": MAX_RESULTS, "description": "最多返回多少条" }
            },
            "required": ["pattern"]
        })
    }

    fn title(&self, input: &Value) -> String {
        format!("搜索 {}", arg(input, "pattern"))
    }

    fn is_read_only(&self, _: &Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    fn max_result_chars(&self) -> usize {
        20_000
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolResult {
        let input: Input = parse_input(input)?;
        let fence = ctx.fence.clone();
        let cancel = ctx.cancel.clone();
        blocking(move || {
            let start = fence.resolve(&input.path)?;
            let regex = RegexBuilder::new(&input.pattern)
                .case_insensitive(input.case_insensitive)
                .build()
                .map_err(|e| ToolError(format!("正则表达式不对：{e}")))?;
            let filter = input
                .glob
                .as_deref()
                .map(|glob| {
                    // 不含 / 的模式（*.rs）按文件名匹配，含 / 的按相对路径匹配。
                    let pattern = if glob.contains('/') {
                        glob.to_string()
                    } else {
                        format!("**/{glob}")
                    };
                    GlobBuilder::new(&pattern)
                        .literal_separator(true)
                        .build()
                        .map(|g| g.compile_matcher())
                        .map_err(|e| ToolError(format!("glob 模式不对：{e}")))
                })
                .transpose()?;
            let limit = input.max_results.unwrap_or(DEFAULT_RESULTS).clamp(1, MAX_RESULTS);

            let files: Box<dyn Iterator<Item = std::path::PathBuf>> = if start.is_dir() {
                Box::new(walk(&fence, &start).filter(|f| !f.is_dir).map(|f| f.path))
            } else {
                Box::new(std::iter::once(start.clone()))
            };
            let mut hits = vec![];
            let mut more = false;
            'files: for file in files {
                if cancel.is_cancelled() {
                    return Err(ToolError::new("已停止"));
                }
                let relative = file.strip_prefix(&start).unwrap_or(&file);
                if filter.as_ref().is_some_and(|f| !f.is_match(relative)) {
                    continue;
                }
                let shown = fence.display(&file);
                // 多要一条，用来判断「后面还有」；再多的不读进内存。
                let wanted = limit - hits.len() + 1;
                for (number, line) in matching_lines(&file, &regex, wanted) {
                    if hits.len() == limit {
                        more = true;
                        break 'files;
                    }
                    hits.push(format!("{shown}:{number}: {line}"));
                }
            }
            if hits.is_empty() {
                return Ok(format!(
                    "没有找到匹配 {} 的内容。注意：被 .gitignore 忽略的文件、隐藏文件和二进制文件不在搜索范围内。",
                    input.pattern
                ));
            }
            let mut out = hits.join("\n");
            if more {
                out.push_str(&format!(
                    "\n\n（结果超过 {limit} 条，只列出前 {limit} 条。请把正则写得更具体，或用 path / glob 缩小范围）"
                ));
            }
            Ok(out)
        })
        .await
    }
}

/// 文件里匹配的行。打不开、太大、是二进制或不是普通文件的，当作没有匹配。
/// 找到 `max` 条就停，不管文件里还有多少。
fn matching_lines(path: &Path, regex: &regex::Regex, max: usize) -> Vec<(usize, String)> {
    // 先看类型再打开：打开没有写入方的管道会一直卡住。
    if !std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
        return vec![];
    }
    let Ok(file) = std::fs::File::open(path) else {
        return vec![];
    };
    if file.metadata().map_or(true, |m| m.len() > MAX_FILE_BYTES) {
        return vec![];
    }
    let mut reader = BufReader::new(file);
    let mut hits = vec![];
    let mut raw = Vec::new();
    let mut number = 0;
    loop {
        raw.clear();
        match reader.read_until(b'\n', &mut raw) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if raw.contains(&0) {
            return vec![];
        }
        number += 1;
        let line = String::from_utf8_lossy(&raw);
        let line = line.trim_end_matches(['\n', '\r']);
        if regex.is_match(line) {
            let mut shown: String = line.trim().chars().take(MAX_LINE_CHARS).collect();
            if line.trim().chars().count() > MAX_LINE_CHARS {
                shown.push('…');
            }
            hits.push((number, shown));
            if hits.len() >= max {
                break;
            }
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Fence;
    use std::fs;

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let files: [(&str, &[u8]); 6] = [
            ("src/main.rs", b"fn main() {\n    run_turn();\n}\n"),
            ("src/turn.rs", b"pub fn run_turn() {}\n// Run_Turn again\n"),
            ("docs/note.md", "调用 run_turn 开始一轮\n".as_bytes()),
            ("target/out.rs", b"run_turn\n"),
            ("blob.bin", b"run_turn\0\n"),
            ("data/token", b"run_turn\n"),
        ];
        for (file, body) in files {
            let path = dir.path().join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }
        fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        dir
    }

    #[tokio::test]
    async fn finds_lines_and_respects_filters() {
        let dir = tree();
        let fence = Fence::new(dir.path(), &[dir.path().join("data")]).unwrap();
        let ctx = ToolCtx::new(fence);
        let grep = |input| async { GrepTool.call(input, &ctx).await.unwrap() };

        assert_eq!(
            grep(json!({"pattern": "run_turn"})).await,
            "docs/note.md:1: 调用 run_turn 开始一轮\nsrc/main.rs:2: run_turn();\nsrc/turn.rs:1: pub fn run_turn() {}"
        );
        assert_eq!(
            grep(json!({"pattern": "run_turn", "glob": "*.rs", "case_insensitive": true})).await,
            "src/main.rs:2: run_turn();\nsrc/turn.rs:1: pub fn run_turn() {}\nsrc/turn.rs:2: // Run_Turn again"
        );
        assert_eq!(
            grep(json!({"pattern": r"fn \w+\(\)", "path": "src/turn.rs"})).await,
            "src/turn.rs:1: pub fn run_turn() {}"
        );
        assert_eq!(
            grep(json!({"pattern": "run", "path": "src", "glob": "main.*"})).await,
            "src/main.rs:2: run_turn();"
        );
        let capped = grep(json!({"pattern": "run_turn", "max_results": 1})).await;
        assert!(capped.starts_with("docs/note.md:1:") && capped.contains("结果超过 1 条"));
        assert!(grep(json!({"pattern": "不存在的东西"}))
            .await
            .contains("没有找到"));

        let err = |input| async { GrepTool.call(input, &ctx).await.unwrap_err().0 };
        assert!(err(json!({"pattern": "("}))
            .await
            .contains("正则表达式不对"));
        assert!(err(json!({"pattern": "x", "path": "data/token"}))
            .await
            .contains("工作目录之外"));
        assert!(err(json!({"pattern": "x", "glob": "[a"}))
            .await
            .contains("glob 模式不对"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn does_not_follow_symlinks_out_of_the_workspace() {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("work");
        fs::create_dir_all(&root).unwrap();
        fs::write(outer.path().join("secret.txt"), "password=1\n").unwrap();
        std::os::unix::fs::symlink(outer.path().join("secret.txt"), root.join("link.txt")).unwrap();
        std::os::unix::fs::symlink(outer.path(), root.join("dirlink")).unwrap();
        let ctx = ToolCtx::new(Fence::new(&root, &[]).unwrap());
        let out = GrepTool
            .call(json!({"pattern": "password"}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("没有找到"), "{out}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn skips_named_pipes_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "needle\n").unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(dir.path().join("pipe"))
            .status()
            .unwrap();
        assert!(made.success());
        let ctx = ToolCtx::new(Fence::new(dir.path(), &[]).unwrap());
        let search = |input| {
            let ctx = ctx.clone();
            async move {
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    GrepTool.call(input, &ctx),
                )
                .await
                .expect("搜索卡在管道上了")
                .unwrap()
            }
        };
        assert_eq!(
            search(json!({"pattern": "needle"})).await,
            "a.txt:1: needle"
        );
        // 直接点名管道也不会去打开它。
        let direct = search(json!({"pattern": "needle", "path": "pipe"})).await;
        assert!(direct.contains("没有找到"), "{direct}");
    }

    #[test]
    fn stops_reading_a_file_once_it_has_enough_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dense.log");
        fs::write(&path, "hit\n".repeat(50_000)).unwrap();
        let regex = regex::Regex::new("hit").unwrap();
        let lines = matching_lines(&path, &regex, 3);
        assert_eq!(lines.iter().map(|l| l.0).collect::<Vec<_>>(), [1, 2, 3]);
    }

    #[tokio::test]
    async fn result_limit_still_reports_that_more_exist() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.log"), "hit\n".repeat(5)).unwrap();
        fs::write(dir.path().join("b.log"), "hit\n".repeat(5)).unwrap();
        let ctx = ToolCtx::new(Fence::new(dir.path(), &[]).unwrap());
        let search = |max: usize| {
            let ctx = ctx.clone();
            async move {
                GrepTool
                    .call(json!({"pattern": "hit", "max_results": max}), &ctx)
                    .await
                    .unwrap()
            }
        };
        let cut = search(7).await;
        assert_eq!(cut.matches(": hit").count(), 7, "{cut}");
        assert!(cut.contains("a.log:5: hit") && cut.contains("b.log:2: hit"));
        assert!(cut.contains("结果超过 7 条"));
        // 正好够数时不说「超过」。
        assert!(!search(10).await.contains("结果超过"));
        assert!(search(5).await.contains("结果超过 5 条"));
    }
}
