use crate::{arg, blocking, parse_input, Tool, ToolCtx, ToolError, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs::File;
use std::io::{BufRead, BufReader};

const DEFAULT_LINES: usize = 2000;
const MAX_LINE_CHARS: usize = 2000;
/// 一次最多返回这么多字，留出余量给行号和尾注。
const MAX_CHARS: usize = 40_000;
const BINARY_PROBE: usize = 8192;
const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;

pub struct ReadTool;

#[derive(Deserialize)]
struct Input {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> String {
        format!(
            "读取工作目录里的一个文本文件，返回带行号的内容。默认从第 1 行开始、最多 {DEFAULT_LINES} 行；\
             文件更长时结果末尾会说明下一次该用的 offset。不能读二进制文件和工作目录之外的文件。"
        )
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "文件路径，相对工作目录" },
                "offset": { "type": "integer", "minimum": 1, "description": "从第几行开始读（从 1 数起）" },
                "limit": { "type": "integer", "minimum": 1, "description": "最多读多少行" }
            },
            "required": ["path"]
        })
    }

    fn title(&self, input: &Value) -> String {
        format!("读取 {}", arg(input, "path"))
    }

    fn is_read_only(&self, _: &Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    fn max_result_chars(&self) -> usize {
        MAX_CHARS + 1_000
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolResult {
        let input: Input = parse_input(input)?;
        let fence = ctx.fence.clone();
        blocking(move || {
            let path = fence.resolve(&input.path)?;
            let shown = fence.display(&path);
            if path.is_dir() {
                return Err(ToolError(format!("{shown} 是目录，请用 list_dir。")));
            }
            let io_error = |e: std::io::Error| ToolError(format!("读不了 {shown}：{e}"));
            let file = File::open(&path).map_err(io_error)?;
            if file.metadata().map_err(io_error)?.len() > MAX_FILE_BYTES {
                return Err(ToolError(format!(
                    "{shown} 超过 20 MB，太大了读不了。可以用 grep 在里面找需要的部分。"
                )));
            }
            let mut reader = BufReader::new(file);
            if reader.fill_buf().map_err(io_error)?.iter().take(BINARY_PROBE).any(|b| *b == 0) {
                return Err(ToolError(format!("{shown} 是二进制文件，读不了。")));
            }

            let start = input.offset.unwrap_or(1).max(1);
            let limit = input.limit.unwrap_or(DEFAULT_LINES).max(1);
            let mut out = String::new();
            let mut number = 0usize;
            let mut shown_lines = 0usize;
            let mut stopped_early = false;
            let mut raw = Vec::new();
            loop {
                raw.clear();
                // 按字节读行：遇到不是 UTF-8 的内容也不报错，替换成占位符。
                if reader.read_until(b'\n', &mut raw).map_err(io_error)? == 0 {
                    break;
                }
                number += 1;
                if number < start {
                    continue;
                }
                if shown_lines >= limit || out.len() >= MAX_CHARS {
                    // 只需要知道「后面还有」，不必为了数总行数把整个文件读完。
                    stopped_early = true;
                    break;
                }
                let line = String::from_utf8_lossy(&raw);
                let line = line.trim_end_matches(['\n', '\r']);
                out.push_str(&format!("{number:>6}\t"));
                if line.chars().count() > MAX_LINE_CHARS {
                    out.extend(line.chars().take(MAX_LINE_CHARS));
                    out.push_str(" …（这一行太长，后面省略）");
                } else {
                    out.push_str(line);
                }
                out.push('\n');
                shown_lines += 1;
            }
            let last_shown = start + shown_lines.saturating_sub(1);

            if number == 0 {
                return Ok(format!("（{shown} 是空文件）"));
            }
            if shown_lines == 0 {
                return Err(ToolError(format!(
                    "{shown} 只有 {number} 行，offset={start} 超出了范围。"
                )));
            }
            if stopped_early {
                out.push_str(&format!(
                    "\n（{shown}：这里是第 {start}–{last_shown} 行，后面还有。接着读请用 offset={}）",
                    last_shown + 1
                ));
            } else if start > 1 {
                out.push_str(&format!(
                    "\n（{shown}：这里是第 {start}–{last_shown} 行，已到文件末尾）"
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

    fn ctx(dir: &std::path::Path) -> ToolCtx {
        ToolCtx::new(Fence::new(dir, &[]).unwrap())
    }

    async fn read(dir: &std::path::Path, input: Value) -> ToolResult {
        ReadTool.call(input, &ctx(dir)).await
    }

    #[tokio::test]
    async fn reads_whole_small_file_with_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "第一行\r\n第二行\n最后没有换行").unwrap();
        let out = read(dir.path(), json!({"path": "a.txt"})).await.unwrap();
        assert_eq!(
            out,
            "     1\t第一行\n     2\t第二行\n     3\t最后没有换行\n"
        );
    }

    #[tokio::test]
    async fn pages_through_long_files() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=10).map(|i| format!("行{i}\n")).collect();
        fs::write(dir.path().join("a.txt"), body).unwrap();

        let first = read(dir.path(), json!({"path": "a.txt", "limit": 3}))
            .await
            .unwrap();
        assert!(first.starts_with("     1\t行1\n     2\t行2\n     3\t行3\n"));
        assert!(!first.contains("行4"));
        assert!(first.contains("offset=4"), "{first}");

        let rest = read(dir.path(), json!({"path": "a.txt", "offset": 9}))
            .await
            .unwrap();
        assert!(rest.starts_with("     9\t行9\n    10\t行10\n"));
        assert!(rest.contains("已到文件末尾"));

        let exact = read(
            dir.path(),
            json!({"path": "a.txt", "offset": 8, "limit": 3}),
        )
        .await
        .unwrap();
        assert!(
            exact.contains("行10") && exact.contains("已到文件末尾"),
            "{exact}"
        );

        let beyond = read(dir.path(), json!({"path": "a.txt", "offset": 11})).await;
        assert!(beyond.unwrap_err().0.contains("只有 10 行"));
    }

    #[tokio::test]
    async fn stops_at_the_character_budget() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (0..3000)
            .map(|_| format!("{}\n", "x".repeat(100)))
            .collect();
        fs::write(dir.path().join("big.txt"), body).unwrap();
        let out = read(dir.path(), json!({"path": "big.txt"})).await.unwrap();
        assert!(out.len() < ReadTool.max_result_chars());
        assert!(out.contains("后面还有"));
    }

    #[tokio::test]
    async fn rejects_binary_directories_missing_and_outside() {
        let outer = tempfile::tempdir().unwrap();
        let dir = outer.path().join("work");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("bin.dat"), [1u8, 0, 2]).unwrap();
        fs::write(dir.join("empty.txt"), "").unwrap();
        fs::write(dir.join("latin1.txt"), [b'a', 0xE9, b'\n']).unwrap();
        fs::write(outer.path().join("secret.txt"), "机密").unwrap();

        let err = |input| async { read(&dir, input).await.unwrap_err().0 };
        assert!(err(json!({"path": "bin.dat"})).await.contains("二进制"));
        assert!(err(json!({"path": "sub"})).await.contains("list_dir"));
        assert!(err(json!({"path": "nope"})).await.contains("不存在"));
        assert!(err(json!({"path": "../secret.txt"}))
            .await
            .contains("工作目录之外"));
        assert!(err(json!({})).await.contains("参数不对"));
        assert!(err(json!({"path": 3})).await.contains("参数不对"));

        assert!(read(&dir, json!({"path": "empty.txt"}))
            .await
            .unwrap()
            .contains("空文件"));
        assert!(read(&dir, json!({"path": "latin1.txt"}))
            .await
            .unwrap()
            .contains("a\u{FFFD}"));
    }
}
