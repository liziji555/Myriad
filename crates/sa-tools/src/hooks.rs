//! 生命周期钩子（Hooks）：在工具调用前后、回复结束时自动运行用户配置的本地命令。
//!
//! 退出码约定（对齐 Claude Code Hooks）：
//! - `0`：放行；标准输出若有文字，则作为补充反馈附给模型（适用于 PostToolUse 提示 linter / formatter 结果）。
//! - `2`：拦截或要求继续验证；标准错误（为空时取标准输出）直接反馈给模型。
//! - 其它非零退出码：记录告警但不中断主流程，避免故障钩子把整个对话卡死。

use crate::process::Tree;
use crate::shell::hidden_env;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 15;
const MAX_HOOK_TIMEOUT_SECS: u64 = 120;
const MAX_HOOK_OUTPUT_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    Stop,
}

impl HookEvent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PreToolUse => "pre_tool_use",
            Self::PostToolUse => "post_tool_use",
            Self::Stop => "stop",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookRule {
    pub event: HookEvent,
    /// 匹配工具名：留空或 `"*"` 匹配全部，支持 `"mcp__*"` 前缀通配，否则按精确工具名匹配。
    #[serde(default)]
    pub tool: Option<String>,
    pub command: String,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_timeout_secs() -> u64 {
    DEFAULT_HOOK_TIMEOUT_SECS
}

impl HookRule {
    fn matches(&self, event: HookEvent, tool: Option<&str>) -> bool {
        if self.event != event {
            return false;
        }
        let Some(pattern) = self
            .tool
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return true;
        };
        if pattern == "*" {
            return true;
        }
        let Some(tool_name) = tool else {
            return false;
        };
        if let Some(prefix) = pattern.strip_suffix('*') {
            tool_name.starts_with(prefix)
        } else {
            tool_name == pattern
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    Allow { feedback: Option<String> },
    Block(String),
}

#[derive(Debug, Clone)]
pub struct HookRunner {
    rules: Vec<HookRule>,
    workdir: PathBuf,
}

impl HookRunner {
    pub fn new(rules: Vec<HookRule>, workdir: PathBuf) -> Self {
        Self { rules, workdir }
    }

    /// 克隆一份在指定目录（如任务独立 worktree）执行的钩子运行器。
    pub fn with_workdir(&self, workdir: PathBuf) -> Self {
        Self {
            rules: self.rules.clone(),
            workdir,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub async fn run_pre_tool(
        &self,
        tool: &str,
        input: &Value,
        cancel: &CancellationToken,
    ) -> HookOutcome {
        let payload = json!({
            "event": HookEvent::PreToolUse.as_str(),
            "tool": tool,
            "input": input,
            "workdir": self.workdir.display().to_string(),
        });
        self.run(HookEvent::PreToolUse, Some(tool), &payload, cancel)
            .await
    }

    pub async fn run_post_tool(
        &self,
        tool: &str,
        input: &Value,
        ok: bool,
        output: &str,
        cancel: &CancellationToken,
    ) -> Option<String> {
        let snippet: String = output.chars().take(4_000).collect();
        let payload = json!({
            "event": HookEvent::PostToolUse.as_str(),
            "tool": tool,
            "input": input,
            "ok": ok,
            "output": snippet,
            "workdir": self.workdir.display().to_string(),
        });
        match self
            .run(HookEvent::PostToolUse, Some(tool), &payload, cancel)
            .await
        {
            HookOutcome::Allow { feedback } => feedback,
            HookOutcome::Block(message) => Some(message),
        }
    }

    pub async fn run_stop(&self, reply_text: &str, cancel: &CancellationToken) -> HookOutcome {
        let snippet: String = reply_text.chars().take(4_000).collect();
        let payload = json!({
            "event": HookEvent::Stop.as_str(),
            "reply": snippet,
            "workdir": self.workdir.display().to_string(),
        });
        self.run(HookEvent::Stop, None, &payload, cancel).await
    }

    async fn run(
        &self,
        event: HookEvent,
        tool: Option<&str>,
        payload: &Value,
        cancel: &CancellationToken,
    ) -> HookOutcome {
        let matching: Vec<&HookRule> = self
            .rules
            .iter()
            .filter(|rule| !rule.command.trim().is_empty() && rule.matches(event, tool))
            .collect();
        if matching.is_empty() {
            return HookOutcome::Allow { feedback: None };
        }
        let body = serde_json::to_vec(payload).unwrap_or_default();
        let mut notes = Vec::new();
        for rule in matching {
            if cancel.is_cancelled() {
                break;
            }
            match exec_rule(rule, &self.workdir, event, tool, &body, cancel).await {
                RuleResult::Allow(Some(note)) => notes.push(note),
                RuleResult::Allow(None) => {}
                RuleResult::Block(reason) => return HookOutcome::Block(reason),
            }
        }
        let feedback = (!notes.is_empty()).then(|| notes.join("\n"));
        HookOutcome::Allow { feedback }
    }
}

enum RuleResult {
    Allow(Option<String>),
    Block(String),
}

async fn exec_rule(
    rule: &HookRule,
    workdir: &PathBuf,
    event: HookEvent,
    tool: Option<&str>,
    body: &[u8],
    cancel: &CancellationToken,
) -> RuleResult {
    let limit = Duration::from_secs(rule.timeout_secs.clamp(1, MAX_HOOK_TIMEOUT_SECS));
    let mut command = if cfg!(windows) {
        let mut c = Command::new("powershell");
        c.args(["-NoProfile", "-NonInteractive", "-Command", &rule.command]);
        c
    } else {
        let mut c = Command::new("bash");
        c.args(["-c", &rule.command]);
        c
    };
    for name in hidden_env() {
        command.env_remove(name);
    }
    command
        .current_dir(workdir)
        .env("SA_HOOK_EVENT", event.as_str())
        .env("SA_HOOK_WORKDIR", workdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(tool) = tool {
        command.env("SA_HOOK_TOOL", tool);
    }
    let Ok(mut tree) = Tree::spawn(command) else {
        return RuleResult::Allow(None);
    };
    if let Some(mut stdin) = tree.stdin() {
        let bytes = body.to_vec();
        tokio::spawn(async move {
            let _ = stdin.write_all(&bytes).await;
            let _ = stdin.shutdown().await;
        });
    }
    let stdout = tree.stdout().expect("hook stdout piped");
    let stderr = tree.stderr().expect("hook stderr piped");
    let work = async {
        let ((out, err), status) = tokio::join!(
            async { tokio::join!(read_capped(stdout), read_capped(stderr)) },
            tree.wait()
        );
        (out, err, status)
    };
    let finished = tokio::select! {
        res = work => Some(res),
        _ = tokio::time::sleep(limit) => None,
        _ = cancel.cancelled() => None,
    };
    let Some((out, err, Ok(status))) = finished else {
        tree.kill();
        return RuleResult::Allow(None);
    };
    tree.disarm();
    let out_text = String::from_utf8_lossy(&out).trim().to_string();
    let err_text = String::from_utf8_lossy(&err).trim().to_string();
    match status.code() {
        Some(0) => RuleResult::Allow((!out_text.is_empty()).then_some(out_text)),
        Some(2) => {
            let reason = if !err_text.is_empty() {
                err_text
            } else if !out_text.is_empty() {
                out_text
            } else {
                format!("钩子（{}）拦截了本次操作。", event.as_str())
            };
            RuleResult::Block(reason)
        }
        _ => RuleResult::Allow(None),
    }
}

async fn read_capped(mut reader: impl AsyncRead + Unpin) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chunk = [0u8; 4096];
    while let Ok(n) = reader.read(&mut chunk).await {
        if n == 0 {
            break;
        }
        let remaining = MAX_HOOK_OUTPUT_BYTES.saturating_sub(out.len());
        if remaining > 0 {
            out.extend_from_slice(&chunk[..n.min(remaining)]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_matching_supports_exact_wildcard_and_prefix() {
        let all = HookRule {
            event: HookEvent::PreToolUse,
            tool: None,
            command: "true".into(),
            timeout_secs: 5,
        };
        assert!(all.matches(HookEvent::PreToolUse, Some("edit")));
        assert!(!all.matches(HookEvent::PostToolUse, Some("edit")));

        let prefix = HookRule {
            event: HookEvent::PreToolUse,
            tool: Some("mcp__*".into()),
            command: "true".into(),
            timeout_secs: 5,
        };
        assert!(prefix.matches(HookEvent::PreToolUse, Some("mcp__github__create")));
        assert!(!prefix.matches(HookEvent::PreToolUse, Some("edit")));

        let exact = HookRule {
            event: HookEvent::PostToolUse,
            tool: Some("write".into()),
            command: "true".into(),
            timeout_secs: 5,
        };
        assert!(exact.matches(HookEvent::PostToolUse, Some("write")));
        assert!(!exact.matches(HookEvent::PostToolUse, Some("write_file")));
    }

    #[tokio::test]
    async fn pre_tool_exit_2_blocks_with_stderr_feedback() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = if cfg!(windows) {
            "[Console]::Error.WriteLine('禁止改动该文件'); exit 2"
        } else {
            "echo '禁止改动该文件' >&2; exit 2"
        };
        let runner = HookRunner::new(
            vec![HookRule {
                event: HookEvent::PreToolUse,
                tool: Some("edit".into()),
                command: cmd.into(),
                timeout_secs: 10,
            }],
            dir.path().to_path_buf(),
        );
        let outcome = runner
            .run_pre_tool(
                "edit",
                &json!({"path": "secret.txt"}),
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(outcome, HookOutcome::Block("禁止改动该文件".into()));
    }

    #[tokio::test]
    async fn post_tool_exit_0_returns_stdout_feedback() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = if cfg!(windows) {
            "Write-Output 'fmt-ok'"
        } else {
            "printf 'fmt-ok'"
        };
        let runner = HookRunner::new(
            vec![HookRule {
                event: HookEvent::PostToolUse,
                tool: Some("edit".into()),
                command: cmd.into(),
                timeout_secs: 10,
            }],
            dir.path().to_path_buf(),
        );
        let note = runner
            .run_post_tool("edit", &json!({}), true, "ok", &CancellationToken::new())
            .await;
        assert_eq!(note.as_deref(), Some("fmt-ok"));
    }
}
