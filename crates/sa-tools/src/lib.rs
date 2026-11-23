//! 工具：模型能调用的动作。
//!
//! 文件类工具只能看工作目录里面的东西，见 [`fence`]。
//! 技能（写在文件里的做事套路）见 [`skills`]，外部 MCP 服务提供的工具见 [`mcp`]。

mod edit;
mod fence;
mod glob;
mod grep;
mod list_dir;
pub mod mcp;
pub mod process;
mod read;
mod shell;
mod skills;
mod todo;
mod walk;
mod web_fetch;
mod write;

pub use edit::EditTool;
pub use fence::Fence;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use list_dir::ListDirTool;
pub use read::ReadTool;
pub use shell::{hidden_env, hide_env_from_commands, ShellTool};
pub use skills::{Skill, SkillSet, SkillTool, MAX_SKILLS};
pub use todo::TodoTool;
pub use web_fetch::WebFetchTool;
pub use write::WriteTool;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// 工具运行时能看到的环境。
#[derive(Debug, Clone)]
pub struct ToolCtx {
    pub fence: Fence,
    pub cancel: CancellationToken,
    /// 允许 `web_fetch` 访问本机和内网地址。只在测试里打开。
    pub allow_private_network: bool,
}

impl ToolCtx {
    pub fn new(fence: Fence) -> Self {
        Self {
            fence,
            cancel: CancellationToken::new(),
            allow_private_network: false,
        }
    }
}

/// 工具执行失败。文字会原样交给模型，所以要写清楚出了什么事、可以怎么改。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ToolError(pub String);

impl ToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

pub type ToolResult = Result<String, ToolError>;

/// 一次调用能不能直接执行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Permission {
    /// 直接执行。
    Allow,
    /// 先问人。文字是给人看的原因，显示在审批卡上。
    Ask(String),
    /// 不执行。文字交给模型。
    Deny(String),
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;

    /// 给模型看的说明。
    fn description(&self) -> String;

    /// 参数的 JSON Schema。
    fn input_schema(&self) -> Value;

    /// 给人看的一句话，例如「读取 README.md」。参数不合法时也要能给出点什么。
    fn title(&self, input: &Value) -> String;

    /// 这次调用是否只读。写入类工具（M2）要先过审批。
    fn is_read_only(&self, _input: &Value) -> bool {
        false
    }

    /// 能不能和别的调用同时跑。
    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        false
    }

    /// 这次调用要不要先问人。默认：只读的直接执行，其余的都问。
    async fn check_permission(&self, input: &Value, _ctx: &ToolCtx) -> Permission {
        if self.is_read_only(input) {
            Permission::Allow
        } else {
            Permission::Ask("这个操作会改动文件或执行命令".into())
        }
    }

    /// 结果超过这么多字就截断。
    fn max_result_chars(&self) -> usize {
        8_000
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolResult;
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 全部只读工具。
    pub fn read_only() -> Self {
        let mut registry = Self::new();
        registry.register(ReadTool);
        registry.register(ListDirTool);
        registry.register(GlobTool);
        registry.register(GrepTool);
        registry.register(WebFetchTool);
        registry
    }

    /// 只读工具加上写文件、改文件、执行命令、待办清单。
    pub fn standard() -> Self {
        let mut registry = Self::read_only();
        registry.register(WriteTool);
        registry.register(EditTool);
        registry.register(ShellTool);
        registry.register(TodoTool);
        registry
    }

    pub fn register(&mut self, tool: impl Tool + 'static) {
        debug_assert!(self.get(tool.name()).is_none(), "工具重名：{}", tool.name());
        self.tools.push(Arc::new(tool));
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.iter().find(|tool| tool.name() == name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn Tool>> {
        self.tools.iter()
    }
}

/// 把模型给的参数解析成工具自己的结构；失败时告诉模型哪里不对。
pub(crate) fn parse_input<T: DeserializeOwned>(input: Value) -> Result<T, ToolError> {
    serde_json::from_value(input).map_err(|e| ToolError(format!("参数不对：{e}")))
}

/// 在阻塞线程上跑文件操作，不占用异步运行时。
pub(crate) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ToolError> + Send + 'static,
) -> Result<T, ToolError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ToolError(format!("工具内部出错：{e}")))?
}

/// 取 `input[key]` 的字符串值，用来拼标题。
pub(crate) fn arg<'a>(input: &'a Value, key: &str) -> &'a str {
    input.get(key).and_then(Value::as_str).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_exposes_read_only_tools_with_object_schemas() {
        let registry = ToolRegistry::read_only();
        let names: Vec<_> = registry.iter().map(|t| t.name()).collect();
        assert_eq!(names, ["read", "list_dir", "glob", "grep", "web_fetch"]);
        for tool in registry.iter() {
            let schema = tool.input_schema();
            assert_eq!(schema["type"], "object", "{}", tool.name());
            assert!(tool.is_read_only(&Value::Null), "{}", tool.name());
            assert!(tool.is_concurrency_safe(&Value::Null), "{}", tool.name());
            assert!(!tool.description().is_empty());
            // 参数不合法时标题也不能 panic。
            let _ = tool.title(&Value::Null);
        }
        assert!(registry.get("write").is_none());
    }
}
