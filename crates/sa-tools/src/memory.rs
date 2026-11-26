//! 跨会话持久记忆（Memory）：支持 Bot 私有记忆与团队共享记忆，
//! 自动把 `MEMORY.md` 索引注入系统提示词，并通过 `memory_list` / `memory_read` / `memory_write` 读写。

use crate::write::write_atomically;
use crate::{arg, blocking, parse_input, Tool, ToolCtx, ToolError, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

const MAX_MEMORY_BYTES: usize = 64 * 1024;
const MAX_FILES_PER_SCOPE: usize = 100;
const INDEX_CHAR_BUDGET: usize = 4_000;

#[derive(Debug)]
pub struct MemoryStore {
    root: PathBuf,
    write_lock: Mutex<()>,
}

impl MemoryStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            write_lock: Mutex::new(()),
        }
    }

    fn scope_dir(&self, scope: &str, bot_id: &str) -> Result<PathBuf, ToolError> {
        match scope {
            "bot" => {
                let safe_bot: String = bot_id
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
                    .collect();
                if safe_bot.is_empty() {
                    return Err(ToolError::new("无效的 Bot 记忆目录。"));
                }
                Ok(self.root.join("bots").join(safe_bot))
            }
            "shared" => Ok(self.root.join("shared")),
            other => Err(ToolError(format!(
                "未知的记忆范围「{other}」，请填 \"bot\"（个人记忆）或 \"shared\"（团队共享记忆）。"
            ))),
        }
    }

    /// 把用户或模型给的记忆键规范成安全的相对 `.md` 路径，防止目录穿越。
    fn resolve_key(
        &self,
        scope: &str,
        bot_id: &str,
        key: &str,
    ) -> Result<(PathBuf, String), ToolError> {
        let normalized = key.trim().replace('\\', "/");
        let trimmed = normalized.trim_matches('/');
        if trimmed.is_empty() {
            return Err(ToolError::new(
                "记忆文件名不能为空，例如 MEMORY.md 或 architecture.md。",
            ));
        }
        let mut parts = Vec::new();
        for seg in trimmed.split('/') {
            // 禁止目录穿越、空字节以及 Windows 盘符前缀（如 C:foo.md）。
            if seg.is_empty()
                || seg == "."
                || seg == ".."
                || seg.contains('\0')
                || seg.contains(':')
            {
                return Err(ToolError(format!("记忆路径不合法：{key}")));
            }
            parts.push(seg);
        }
        if parts.len() > 3 {
            return Err(ToolError::new("记忆路径层级太深（最多 3 层）。"));
        }
        let mut rel = parts.join("/");
        if !rel.ends_with(".md") {
            rel.push_str(".md");
        }
        if !Path::new(&rel)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        {
            return Err(ToolError(format!("记忆路径不合法：{key}")));
        }
        let base = self.scope_dir(scope, bot_id)?;
        Ok((base.join(&rel), rel))
    }

    /// 生成注入系统提示词的精简记忆索引（控制字符预算，不污染上下文）。
    pub fn prompt_block(&self, bot_id: &str) -> String {
        let mut sections = Vec::new();
        let mut budget = INDEX_CHAR_BUDGET;

        for (label, scope) in [("团队共享记忆", "shared"), ("你的持久记忆", "bot")] {
            let Ok(dir) = self.scope_dir(scope, bot_id) else {
                continue;
            };
            let files = list_md_files(&dir);
            if files.is_empty() {
                continue;
            }
            let mut block = format!(
                "### {label}（scope=\"{scope}\"）\n可用记忆文件：{}\n",
                files.join("、")
            );
            let index_path = dir.join("MEMORY.md");
            if let Ok(text) = std::fs::read_to_string(&index_path) {
                let body = text.trim();
                if !body.is_empty() {
                    let take = budget.min(1_800);
                    let clipped: String = body.chars().take(take).collect();
                    block.push_str("\n`MEMORY.md` 摘要：\n");
                    block.push_str(&clipped);
                    if body.chars().count() > take {
                        block.push_str("\n…（更多请用 memory_read 读取）");
                    }
                    block.push('\n');
                }
            }
            let used = block.chars().count();
            if used <= budget || sections.is_empty() {
                budget = budget.saturating_sub(used);
                sections.push(block);
            }
        }

        if sections.is_empty() {
            return String::new();
        }
        format!(
            "## 跨会话持久记忆\n\
             以下是你和团队沉淀下来的持久记忆（重启和切换房间依然保留）。需要详情时用 `memory_read`，\
             遇到值得长期记住的用户偏好、项目约定、架构结论或避坑经验时用 `memory_write` 记录到 `MEMORY.md` 或专题笔记。\n\n{}",
            sections.join("\n")
        )
    }
}

fn list_md_files(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    collect_md(dir, dir, &mut found);
    found.sort();
    found.truncate(MAX_FILES_PER_SCOPE);
    found
}

fn collect_md(root: &Path, current: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(current) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_md(root, &path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
}

pub struct MemoryListTool {
    pub store: Arc<MemoryStore>,
    pub bot_id: String,
}

#[derive(Deserialize)]
struct ListInput {
    #[serde(default)]
    scope: Option<String>,
}

#[async_trait]
impl Tool for MemoryListTool {
    fn name(&self) -> &'static str {
        "memory_list"
    }

    fn description(&self) -> String {
        "列出已保存的跨会话持久记忆文件（scope 可选 \"bot\"、\"shared\"，留空则两处都列）。".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "scope": {
                    "type": "string",
                    "enum": ["bot", "shared"],
                    "description": "可选：只看个人记忆（bot）或团队共享记忆（shared）"
                }
            }
        })
    }

    fn title(&self, _input: &Value) -> String {
        "列出持久记忆".into()
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, _ctx: &ToolCtx) -> ToolResult {
        let input: ListInput = parse_input(input)?;
        let store = self.store.clone();
        let bot_id = self.bot_id.clone();
        blocking(move || {
            let scopes: Vec<&str> = match input.scope.as_deref() {
                Some(s) => vec![s],
                None => vec!["bot", "shared"],
            };
            let mut lines = Vec::new();
            for scope in scopes {
                let dir = store.scope_dir(scope, &bot_id)?;
                let files = list_md_files(&dir);
                if files.is_empty() {
                    lines.push(format!("[{scope}] （暂无记忆文件）"));
                } else {
                    lines.push(format!("[{scope}]\n- {}", files.join("\n- ")));
                }
            }
            Ok(lines.join("\n\n"))
        })
        .await
    }
}

pub struct MemoryReadTool {
    pub store: Arc<MemoryStore>,
    pub bot_id: String,
}

#[derive(Deserialize)]
struct ReadInput {
    key: String,
    #[serde(default = "default_bot_scope")]
    scope: String,
}

fn default_bot_scope() -> String {
    "bot".into()
}

#[async_trait]
impl Tool for MemoryReadTool {
    fn name(&self) -> &'static str {
        "memory_read"
    }

    fn description(&self) -> String {
        "读取一条跨会话持久记忆文件的内容（默认从个人记忆 scope=\"bot\" 读，也可读团队共享 scope=\"shared\"）。".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": { "type": "string", "description": "记忆文件名，如 MEMORY.md 或 architecture.md" },
                "scope": { "type": "string", "enum": ["bot", "shared"], "description": "记忆范围，默认 bot" }
            },
            "required": ["key"]
        })
    }

    fn title(&self, input: &Value) -> String {
        format!("读取记忆 {}", arg(input, "key"))
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, _ctx: &ToolCtx) -> ToolResult {
        let input: ReadInput = parse_input(input)?;
        let store = self.store.clone();
        let bot_id = self.bot_id.clone();
        blocking(move || {
            let (path, rel) = store.resolve_key(&input.scope, &bot_id, &input.key)?;
            match std::fs::read_to_string(&path) {
                Ok(text) => Ok(format!("[{}/{rel}]\n{text}", input.scope)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(ToolError(format!(
                    "记忆文件 [{}/{rel}] 还不存在。可以先用 memory_list 查看已有记忆。",
                    input.scope
                ))),
                Err(e) => Err(ToolError(format!("读不了记忆文件：{e}"))),
            }
        })
        .await
    }
}

pub struct MemoryWriteTool {
    pub store: Arc<MemoryStore>,
    pub bot_id: String,
    /// 外链房间（如 Telegram / GitHub）写入持久记忆需经本机用户审批，防止提示词注入投毒记忆。
    pub require_approval: bool,
}

#[derive(Deserialize)]
struct WriteInput {
    key: String,
    content: String,
    #[serde(default = "default_bot_scope")]
    scope: String,
    #[serde(default)]
    append: bool,
}

#[async_trait]
impl Tool for MemoryWriteTool {
    fn name(&self) -> &'static str {
        "memory_write"
    }

    fn description(&self) -> String {
        "写入或追加一条跨会话持久记忆（Markdown）。适合沉淀用户偏好、项目规范、架构决策、调试踩坑记录。\
         建议把总览记在 MEMORY.md，详细内容拆分到专题文件。scope=\"bot\" 为个人记忆，scope=\"shared\" 为团队共享记忆。"
            .into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": { "type": "string", "description": "记忆文件名，如 MEMORY.md 或 deploy-notes.md" },
                "content": { "type": "string", "description": "要保存的 Markdown 内容" },
                "scope": { "type": "string", "enum": ["bot", "shared"], "description": "记忆范围，默认 bot" },
                "append": { "type": "boolean", "description": "是否追加到末尾（默认 false 覆盖写入）" }
            },
            "required": ["key", "content"]
        })
    }

    fn title(&self, input: &Value) -> String {
        format!("记录记忆 {}", arg(input, "key"))
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        // 本机房间的记忆写入严格隔离在独立 Markdown 沙箱里无需打断；外链房间则走审批防投毒。
        !self.require_approval
    }

    async fn check_permission(&self, input: &Value, _ctx: &ToolCtx) -> crate::Permission {
        if self.require_approval {
            crate::Permission::Ask(format!(
                "外链房间请求写入跨会话持久记忆：{}",
                arg(input, "key")
            ))
        } else {
            crate::Permission::Allow
        }
    }

    fn finishes_once_started(&self) -> bool {
        true
    }

    async fn call(&self, input: Value, _ctx: &ToolCtx) -> ToolResult {
        let input: WriteInput = parse_input(input)?;
        if input.content.len() > MAX_MEMORY_BYTES {
            return Err(ToolError(format!(
                "单条记忆不能超过 {} KB，请精简后再保存。",
                MAX_MEMORY_BYTES / 1024
            )));
        }
        let store = self.store.clone();
        let bot_id = self.bot_id.clone();
        blocking(move || {
            // 串行化读-改-写与文件数上限校验，避免并行 Bot 同时 append 同一份共享记忆时互相覆盖。
            let _guard = store.write_lock.lock().unwrap_or_else(|e| e.into_inner());
            let dir = store.scope_dir(&input.scope, &bot_id)?;
            let (path, rel) = store.resolve_key(&input.scope, &bot_id, &input.key)?;
            let existing_files = list_md_files(&dir);
            if !path.exists() && existing_files.len() >= MAX_FILES_PER_SCOPE {
                return Err(ToolError(format!(
                    "该记忆空间最多保存 {MAX_FILES_PER_SCOPE} 个文件，请合并更新已有记忆。"
                )));
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| ToolError(format!("创建记忆目录失败：{e}")))?;
            }
            let final_text = if input.append && path.exists() {
                let mut cur = std::fs::read_to_string(&path).unwrap_or_default();
                if !cur.is_empty() && !cur.ends_with('\n') {
                    cur.push('\n');
                }
                cur.push_str(&input.content);
                if cur.len() > MAX_MEMORY_BYTES {
                    return Err(ToolError(format!(
                        "追加后超过单文件上限（{} KB），请改用覆盖精简。",
                        MAX_MEMORY_BYTES / 1024
                    )));
                }
                cur
            } else {
                input.content
            };
            write_atomically(&path, final_text.as_bytes())
                .map_err(|e| ToolError(format!("保存记忆失败：{e}")))?;
            Ok(format!(
                "已保存持久记忆 [{}/{rel}]（{} 字节）。",
                input.scope,
                final_text.len()
            ))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Fence;

    #[tokio::test]
    async fn writes_reads_lists_and_injects_prompt_block() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::new(dir.path().join("memory")));
        let ctx = ToolCtx::new(Fence::new(dir.path(), &[]).unwrap());

        let writer = MemoryWriteTool {
            store: store.clone(),
            bot_id: "bot_1".into(),
            require_approval: false,
        };
        let reader = MemoryReadTool {
            store: store.clone(),
            bot_id: "bot_1".into(),
        };
        let lister = MemoryListTool {
            store: store.clone(),
            bot_id: "bot_1".into(),
        };

        writer
            .call(
                json!({"key": "MEMORY", "content": "- 用户偏好柔和毛玻璃配色\n"}),
                &ctx,
            )
            .await
            .unwrap();
        writer
            .call(
                json!({"key": "MEMORY", "content": "- 后端用 Rust\n", "append": true}),
                &ctx,
            )
            .await
            .unwrap();
        writer
            .call(
                json!({"key": "conventions", "scope": "shared", "content": "总是跑 clippy"}),
                &ctx,
            )
            .await
            .unwrap();

        let text = reader
            .call(json!({"key": "MEMORY.md"}), &ctx)
            .await
            .unwrap();
        assert!(text.contains("毛玻璃配色"));
        assert!(text.contains("Rust"));

        let list = lister.call(json!({}), &ctx).await.unwrap();
        assert!(list.contains("MEMORY.md"));
        assert!(list.contains("conventions.md"));

        let prompt = store.prompt_block("bot_1");
        assert!(prompt.contains("跨会话持久记忆"));
        assert!(prompt.contains("毛玻璃配色"));
        assert!(prompt.contains("conventions.md"));

        // 防止目录穿越。
        let err = writer
            .call(json!({"key": "../escape.md", "content": "x"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.0.contains("不合法"));
    }
}
