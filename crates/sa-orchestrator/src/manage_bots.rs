//! 主控团队管理工具（借鉴 ProkBot / GrokBot 的 `CreateAgent` / `UpdateAgent` 理念）：
//! - `create_bot`：主控按用户需求自主创建新的专职子 Bot（支持内置模型或外部 CLI Harness），并可自动拉入当前房间协同；
//! - `update_bot`：主控更新已有子 Bot 的名称、职责说明或模型档位（有意不提供删除工具，防止误删用户 Bot）；
//! - `create_room`：主控按需把多位已有 Bot 组建进一个新的多 Bot 协作房间。

use crate::room::db;
use crate::Shared;
use async_trait::async_trait;
use sa_engine::{BotPatch, BotProfileOptions};
use sa_protocol::{Bot, CreateRoomRequest, Discussion, Harness, ModelTier, RoomId};
use sa_tools::{Permission, Tool, ToolCtx, ToolError, ToolResult};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub(crate) struct CreateBotTool {
    pub shared: Arc<Shared>,
    pub room_id: RoomId,
    pub from: Bot,
}

#[derive(Deserialize)]
struct CreateBotInput {
    name: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    persona: String,
    #[serde(default)]
    tier: Option<ModelTier>,
    #[serde(default)]
    harness: Option<Harness>,
    #[serde(default)]
    command: String,
    #[serde(default)]
    avatar_shape: String,
    #[serde(default)]
    avatar_color: String,
    #[serde(default)]
    add_to_room: bool,
}

#[async_trait]
impl Tool for CreateBotTool {
    fn name(&self) -> &'static str {
        "create_bot"
    }

    fn description(&self) -> String {
        "创建一个新的独立专职子 Bot（支持内置模型或外部 CLI Harness，如 claude_code / codex / gemini / kimi / open_code / acp）。\
         默认创建为独立 Bot（add_to_room=false），立刻出现在用户左侧「我的 Bot」列表里，不会破坏当前 1v1 主控对话；\
         只有在多人协作房间里需要它马上加入讨论时，才传 add_to_room=true 或用 create_room 另建多 Bot 协作房间。"
            .into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "新 Bot 的简短名字（最多 24 字，如「前端专家」「代码审查」）" },
                "role": { "type": "string", "description": "简短职责标签（最多 24 字，如「前端工程师」「架构审查」）" },
                "persona": { "type": "string", "description": "新 Bot 的职责、分工和行事要求" },
                "tier": {
                    "type": "string",
                    "enum": ["cheap", "mid", "strong"],
                    "description": "模型档位，默认 mid"
                },
                "harness": {
                    "type": "string",
                    "enum": ["builtin", "claude_code", "codex", "open_code", "gemini", "kimi", "acp", "command"],
                    "description": "执行引擎，默认 builtin（内置模型）"
                },
                "command": { "type": "string", "description": "当 harness 为 acp 或 command 时的启动命令" },
                "avatar_shape": {
                    "type": "string",
                    "enum": ["orb", "pebble", "squircle", "cloud", "teardrop", "hex", "capsule", "star", "sprout", "cat", "shield", "prism"],
                    "description": "可选专属造型，留空则按 Bot ID 自动分配独特造型"
                },
                "avatar_color": {
                    "type": "string",
                    "enum": ["cobalt", "cyan", "emerald", "amber", "coral", "rose", "violet", "indigo", "mint", "slate", "aurora"],
                    "description": "可选专属配色，留空则按 Bot ID 自动分配"
                },
                "add_to_room": { "type": "boolean", "description": "是否同时拉进当前房间，默认 false（保持独立 Bot）" }
            },
            "required": ["name"]
        })
    }

    fn title(&self, input: &Value) -> String {
        let name = input["name"].as_str().unwrap_or("新 Bot").trim();
        format!("创建 Bot「{name}」")
    }

    async fn check_permission(&self, _input: &Value, _ctx: &ToolCtx) -> Permission {
        Permission::Ask("这个操作会创建新的 Bot 并更新团队配置".into())
    }

    async fn call(&self, input: Value, _ctx: &ToolCtx) -> ToolResult {
        if !self.from.is_master {
            return Err(ToolError("只有中枢主控 Bot 才能创建新 Bot。".into()));
        }
        let input: CreateBotInput =
            serde_json::from_value(input).map_err(|e| ToolError(format!("参数不对：{e}")))?;
        let engine = self.shared.engine.clone();
        let room_id = self.room_id.clone();
        let tier = input.tier.unwrap_or(ModelTier::Mid);
        let harness = input.harness.unwrap_or(Harness::Builtin);
        let add_to_room = input.add_to_room;
        let by = self.from.name.clone();

        let (bot, added, add_note) = db(move || {
            let bot = engine.create_bot_with_profile(
                &input.name,
                tier,
                &input.persona,
                harness,
                &input.command,
                BotProfileOptions {
                    is_master: Some(false),
                    role: (!input.role.trim().is_empty()).then_some(input.role),
                    avatar_shape: (!input.avatar_shape.trim().is_empty())
                        .then_some(input.avatar_shape),
                    avatar_color: (!input.avatar_color.trim().is_empty())
                        .then_some(input.avatar_color),
                    avatar_image: None,
                },
            )?;
            let mut added = false;
            let mut add_note = String::new();
            if add_to_room {
                match engine.add_room_member(&room_id, &bot.id) {
                    Ok(_) => added = true,
                    Err(err) => add_note = format!("（未能拉进当前房间：{err}）"),
                }
            }
            Ok((bot, added, add_note))
        })
        .await
        .map_err(|e| ToolError(format!("创建 Bot 失败：{e}")))?;

        let join_msg = if added {
            "已加入当前房间，现在可以直接用 ask_bot 向它派活。"
        } else if !add_note.is_empty() {
            &add_note
        } else {
            "已创建为独立 Bot，已出现在用户左侧「我的 Bot」列表里，可直接点击 1v1 对话，或用 create_room 拉它组建协作房间。"
        };
        Ok(format!(
            "主控「{by}」已创建 Bot「{}」（id: {}，引擎: {:?}，档位: {:?}）。{join_msg}",
            bot.name, bot.id, bot.harness, bot.tier
        ))
    }
}

pub(crate) struct UpdateBotTool {
    pub shared: Arc<Shared>,
}

#[derive(Deserialize)]
struct UpdateBotInput {
    bot: String,
    #[serde(default)]
    new_name: Option<String>,
    #[serde(default)]
    persona: Option<String>,
    #[serde(default)]
    tier: Option<ModelTier>,
}

#[async_trait]
impl Tool for UpdateBotTool {
    fn name(&self) -> &'static str {
        "update_bot"
    }

    fn description(&self) -> String {
        "修改已有 Bot 的名字、职责说明（persona）或模型档位。不能删除 Bot。".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "bot": { "type": "string", "description": "要修改的 Bot 名字或 ID" },
                "new_name": { "type": "string", "description": "新名字（可选）" },
                "persona": { "type": "string", "description": "新的职责说明（可选）" },
                "tier": { "type": "string", "enum": ["cheap", "mid", "strong"] }
            },
            "required": ["bot"]
        })
    }

    fn title(&self, input: &Value) -> String {
        let name = input["bot"].as_str().unwrap_or("Bot").trim();
        format!("调整 Bot「{name}」")
    }

    async fn check_permission(&self, _input: &Value, _ctx: &ToolCtx) -> Permission {
        Permission::Ask("这个操作会创建新的 Bot 并更新团队配置".into())
    }

    async fn call(&self, input: Value, _ctx: &ToolCtx) -> ToolResult {
        let input: UpdateBotInput =
            serde_json::from_value(input).map_err(|e| ToolError(format!("参数不对：{e}")))?;
        let wanted = input.bot.trim().trim_start_matches('@').to_lowercase();
        let engine = self.shared.engine.clone();
        let updated = db(move || {
            let bots = engine.list_bots()?;
            let target = bots
                .into_iter()
                .find(|b| {
                    b.id.as_str().eq_ignore_ascii_case(&wanted) || b.name.to_lowercase() == wanted
                })
                .ok_or_else(|| {
                    sa_engine::EngineError::NotFound(format!(
                        "找不到叫「{}」的 Bot",
                        input.bot.trim()
                    ))
                })?;
            engine.update_bot(
                &target.id,
                BotPatch {
                    name: input.new_name,
                    persona: input.persona,
                    tier: input.tier,
                    ..BotPatch::default()
                },
            )
        })
        .await
        .map_err(|e| ToolError(format!("修改 Bot 失败：{e}")))?;

        Ok(format!(
            "已更新 Bot「{}」（id: {}，档位: {:?}）。",
            updated.name, updated.id, updated.tier
        ))
    }
}

pub(crate) struct CreateRoomTool {
    pub shared: Arc<Shared>,
    pub from: Bot,
}

#[derive(Deserialize)]
struct CreateRoomInput {
    name: String,
    #[serde(default)]
    members: Vec<String>,
    #[serde(default)]
    discussion: Option<Discussion>,
}

#[async_trait]
impl Tool for CreateRoomTool {
    fn name(&self) -> &'static str {
        "create_room"
    }

    fn description(&self) -> String {
        "组建一个新的多 Bot 协作房间，把指定的已有 Bot 拉进房间并设置协作方式（默认主 Agent 派活 lead）。".into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "房间名称（如「前端重构组」）" },
                "members": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "要拉进房间的 Bot 名字或 ID 列表"
                },
                "discussion": {
                    "type": "string",
                    "enum": ["lead", "normal", "parallel", "rounds", "blind"],
                    "description": "房间协作方式，多位 Bot 时默认 lead（主控派活）"
                }
            },
            "required": ["name"]
        })
    }

    fn title(&self, input: &Value) -> String {
        let name = input["name"].as_str().unwrap_or("新房间").trim();
        format!("组建房间「{name}」")
    }

    async fn check_permission(&self, _input: &Value, _ctx: &ToolCtx) -> Permission {
        Permission::Ask("这个操作会创建新的 Bot 并更新团队配置".into())
    }

    async fn call(&self, input: Value, _ctx: &ToolCtx) -> ToolResult {
        let input: CreateRoomInput =
            serde_json::from_value(input).map_err(|e| ToolError(format!("参数不对：{e}")))?;
        let engine = self.shared.engine.clone();
        let host_id = self.from.id.clone();

        let (room, member_names) = db(move || {
            let bots = engine.list_bots()?;
            let mut ids = vec![host_id.clone()];
            let mut names = Vec::new();
            if let Some(h) = bots.iter().find(|b| b.id == host_id) {
                names.push(h.name.clone());
            }
            let mut missing = Vec::new();
            for raw in &input.members {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let wanted = trimmed.trim_start_matches('@').to_lowercase();
                if let Some(found) = bots.iter().find(|b| {
                    b.id.as_str().eq_ignore_ascii_case(&wanted) || b.name.to_lowercase() == wanted
                }) {
                    if !ids.contains(&found.id) {
                        ids.push(found.id.clone());
                        names.push(found.name.clone());
                    }
                } else {
                    missing.push(trimmed.to_string());
                }
            }
            if !missing.is_empty() {
                return Err(sa_engine::EngineError::NotFound(format!(
                    "找不到以下 Bot：{}（请先用 create_bot 创建或核对名字）",
                    missing.join("、")
                )));
            }
            let discussion = input.discussion.or(Some(if ids.len() >= 2 {
                Discussion::Lead
            } else {
                Discussion::Normal
            }));
            let room = engine.create_room_with(&CreateRoomRequest {
                name: input.name,
                member_bot_ids: Some(ids),
                host_bot_id: Some(host_id),
                discussion,
            })?;
            Ok((room, names))
        })
        .await
        .map_err(|e| ToolError(format!("组建房间失败：{e}")))?;

        Ok(format!(
            "已组建协作房间「{}」（id: {}），成员：{}。用户可在左侧「协作房间」中切换进入。",
            room.name,
            room.id,
            member_names.join("、")
        ))
    }
}
