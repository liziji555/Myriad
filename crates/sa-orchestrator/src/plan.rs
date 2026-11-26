//! 规划模式（Plan Mode）：
//! 当房间处于 `PermissionMode::Plan` 时，内置 Bot 仅拥有只读探索工具与 `submit_plan`。
//! Bot 调研完毕后调用 `submit_plan` 提交结构化实施方案；Myriad 弹出审批卡请人审阅。
//! 一旦人点「允许」，房间自动切回 `PermissionMode::Ask`，并在同一轮内立刻解锁全部写入/执行工具开始实施。

use crate::room::db;
use crate::Shared;
use async_trait::async_trait;
use sa_protocol::{PermissionMode, RoomId, UpdateRoomRequest};
use sa_tools::{Permission, Tool, ToolCtx, ToolError, ToolResult};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

pub(crate) const PLAN_RULES: &str = "\n\n## 当前处于规划模式（Plan Mode · 只读）\n\
- 你现在只能使用只读工具（read / list_dir / glob / grep / web_fetch / web_search / agent / todo / memory_read），不能修改任何文件，也不能执行命令。\n\
- 请先充分调研代码库和需求，理清要改哪些文件、关键步骤与潜在风险，然后调用 `submit_plan` 提交清晰的结构化方案，等待用户批准。\n\
- 用户批准后房间会自动切回执行模式并解锁写入与命令工具；若用户拒绝，请根据反馈继续完善方案。";

pub(crate) struct SubmitPlanTool {
    pub shared: Arc<Shared>,
    pub room_id: RoomId,
    /// 用户批准后把方案摘要写进来，外层 `room::turn` 据此无缝转入执行阶段。
    pub approved: Arc<Mutex<Option<String>>>,
}

#[derive(Deserialize)]
struct SubmitPlanInput {
    title: String,
    summary: String,
    #[serde(default)]
    steps: Vec<String>,
}

fn format_plan_card(input: &Value) -> String {
    let title = input["title"].as_str().unwrap_or("实施方案").trim();
    let summary = input["summary"].as_str().unwrap_or("").trim();
    let mut out = format!("【方案】{title}");
    if !summary.is_empty() {
        out.push_str(&format!("\n{summary}"));
    }
    if let Some(steps) = input["steps"].as_array() {
        let items: Vec<String> = steps
            .iter()
            .filter_map(|v| v.as_str().map(str::trim).filter(|s| !s.is_empty()))
            .enumerate()
            .map(|(i, s)| format!("{}. {s}", i + 1))
            .collect();
        if !items.is_empty() {
            out.push_str("\n\n实施步骤：\n");
            out.push_str(&items.join("\n"));
        }
    }
    out.push_str("\n\n点击「允许」后将自动退出规划模式并按此方案开始执行。");
    out
}

#[async_trait]
impl Tool for SubmitPlanTool {
    fn name(&self) -> &'static str {
        "submit_plan"
    }

    fn description(&self) -> String {
        "在规划模式下提交结构化实施方案供用户审批。用户批准后，房间自动退出规划模式并解锁写入与执行工具。".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "方案标题" },
                "summary": { "type": "string", "description": "整体思路、改动范围和风险说明" },
                "steps": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "按先后顺序排列的具体实施步骤"
                }
            },
            "required": ["title", "summary", "steps"]
        })
    }

    fn title(&self, input: &Value) -> String {
        let title = input["title"].as_str().unwrap_or("实施方案").trim();
        format!("提交方案：{title}")
    }

    async fn check_permission(&self, input: &Value, _ctx: &ToolCtx) -> Permission {
        Permission::Ask(format_plan_card(input))
    }

    fn finishes_once_started(&self) -> bool {
        true
    }

    async fn call(&self, input: Value, _ctx: &ToolCtx) -> ToolResult {
        let parsed: SubmitPlanInput =
            serde_json::from_value(input).map_err(|e| ToolError(format!("方案参数不对：{e}")))?;
        let title = parsed.title.trim();
        if title.is_empty() {
            return Err(ToolError::new("方案标题 title 不能为空。"));
        }
        let engine = self.shared.engine.clone();
        let room_id = self.room_id.clone();
        db(move || {
            engine.edit_room(
                &room_id,
                &UpdateRoomRequest {
                    name: None,
                    host_bot_id: None,
                    discussion: None,
                    rounds: None,
                    share: None,
                    permission: Some(PermissionMode::Ask),
                },
            )
        })
        .await
        .map_err(|e| ToolError(format!("切回执行模式失败：{e}")))?;

        let mut plan_text = format!("标题：{title}\n摘要：{}", parsed.summary.trim());
        if !parsed.steps.is_empty() {
            plan_text.push_str("\n步骤：\n");
            for (i, step) in parsed.steps.iter().enumerate() {
                plan_text.push_str(&format!("{}. {}\n", i + 1, step.trim()));
            }
        }
        *self.approved.lock().unwrap_or_else(|e| e.into_inner()) = Some(plan_text);
        Ok(
            "用户已批准方案！房间已自动切换为执行模式（Ask）。请简短确认并立即结束当前规划步骤（不要再调用只读工具），系统会立刻为你解锁全部写入与执行工具进入实施阶段。"
                .into(),
        )
    }
}
