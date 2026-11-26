//! 项目规范自动发现（`AGENTS.md` / `CLAUDE.md` / `.myriad/RULES.md`）：
//! 每次启动一轮对话或任务时自动从工作目录根部读取项目约定（总上限 32 KiB），
//! 用明确边界包裹后注入系统提示词，避免项目规则覆盖核心安全原则。

use sa_tools::Fence;

const CANDIDATES: &[&str] = &["AGENTS.md", "CLAUDE.md", ".myriad/RULES.md"];
const MAX_TOTAL_BYTES: usize = 32 * 1024;

pub(crate) fn load_project_docs(fence: &Fence) -> String {
    let mut sections = Vec::new();
    let mut remaining = MAX_TOTAL_BYTES;

    for rel in CANDIDATES {
        if remaining == 0 {
            break;
        }
        // 通过 Fence 解析路径，防止仓库内恶意符号链接指向工作区外或数据目录里的敏感文件。
        let Ok(path) = fence.resolve(rel) else {
            continue;
        };
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let clipped = clip_utf8_bytes(trimmed, remaining);
        remaining = remaining.saturating_sub(clipped.len());
        let suffix = if clipped.len() < trimmed.len() {
            "\n…（超出 32 KiB 预算，余下部分已省略，可用 read 查看全文）"
        } else {
            ""
        };
        sections.push(format!("### {rel}\n{clipped}{suffix}"));
    }

    if sections.is_empty() {
        return String::new();
    }
    format!(
        "## 项目规范（来自当前工作目录，请在不违背核心安全规则的前提下遵循）\n\
         <project_instructions>\n{}\n</project_instructions>",
        sections.join("\n\n")
    )
}

fn clip_utf8_bytes(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn loads_agents_and_claude_md_with_cap() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "# 约定\n统一用 pnpm。\n").unwrap();
        fs::write(dir.path().join("CLAUDE.md"), "请跑 cargo test。\n").unwrap();
        fs::create_dir_all(dir.path().join(".myriad")).unwrap();
        fs::write(dir.path().join(".myriad/RULES.md"), "禁止强推 main。\n").unwrap();

        let fence = Fence::new(dir.path(), &[]).unwrap();
        let block = load_project_docs(&fence);
        assert!(block.contains("AGENTS.md"));
        assert!(block.contains("统一用 pnpm"));
        assert!(block.contains("CLAUDE.md"));
        assert!(block.contains(".myriad/RULES.md"));
        assert!(block.contains("<project_instructions>"));
    }

    #[test]
    fn returns_empty_when_no_instruction_files_exist() {
        let dir = tempfile::tempdir().unwrap();
        let fence = Fence::new(dir.path(), &[]).unwrap();
        assert!(load_project_docs(&fence).is_empty());
    }
}
