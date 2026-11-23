//! `ask_bot`：一个 Bot 点名另一个 Bot。只把话带到，不等回答。

use crate::room::db;
use crate::{RoomLink, Shared};
use async_trait::async_trait;
use sa_protocol::{Bot, RoomId};
use sa_tools::{Tool, ToolCtx, ToolError, ToolResult};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// 一句话最多这么多字。要交代的东西更多时，应该写进文件让对方去读。
const MAX_ASK_CHARS: usize = 4_000;

pub(crate) struct AskBotTool {
    pub shared: Arc<Shared>,
    pub link: Arc<RoomLink>,
    pub room_id: RoomId,
    /// 谁在问。
    pub from: Bot,
    /// 它这次发言是转到第几手。
    /// 这次发言转到了第几手。人中途说了新的话会被清零，所以是共享的。
    pub hop: Arc<AtomicU32>,
}

#[derive(Deserialize)]
struct Input {
    bot: String,
    message: String,
}

#[async_trait]
impl Tool for AskBotTool {
    fn name(&self) -> &'static str {
        "ask_bot"
    }

    fn description(&self) -> String {
        "点名房间里的另一个 Bot，请它接手或回答。你的话会作为一条消息出现在房间里，所有人都看得到。\
         这个工具只负责把话带到，不会等对方回答：对方马上会在自己那边开始，回答之后会作为新消息出现。\
         你可以接着做你这边的事，但不要替对方回答，也不要再调一次去催。\
         message 要写清楚请它做什么、需要的背景是什么，对方看不到你的思考过程。"
            .into()
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "bot": { "type": "string", "description": "对方的名字，和房间成员列表里的一样" },
                "message": { "type": "string", "description": "要对它说的话" }
            },
            "required": ["bot", "message"]
        })
    }

    fn title(&self, input: &Value) -> String {
        let name = input["bot"].as_str().unwrap_or("").trim_start_matches('@');
        let message: String = input["message"]
            .as_str()
            .unwrap_or("")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let mut chars = message.chars();
        let mut short: String = chars.by_ref().take(40).collect();
        if chars.next().is_some() {
            short.push('…');
        }
        format!("请 {name}：{short}")
    }

    /// 只是在房间里说一句话，不碰文件也不执行东西，不用人批准。
    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value, _ctx: &ToolCtx) -> ToolResult {
        let input: Input =
            serde_json::from_value(input).map_err(|e| ToolError(format!("参数不对：{e}")))?;
        let wanted = input.bot.trim().trim_start_matches('@').to_lowercase();
        let message = input.message.trim().to_string();
        if message.is_empty() {
            return Err(ToolError::new("message 是空的。要请它做什么，写清楚。"));
        }
        if message.chars().count() > MAX_ASK_CHARS {
            return Err(ToolError(format!(
                "message 超过 {MAX_ASK_CHARS} 个字。长的内容请写进文件，在 message 里告诉它去读哪个文件。"
            )));
        }

        let (engine, room_id) = (self.shared.engine.clone(), self.room_id.clone());
        let members = db(move || {
            let room = engine.get_room(&room_id)?;
            let mut bots = engine.list_bots()?;
            bots.retain(|bot| bot.enabled && room.member_bot_ids.contains(&bot.id));
            Ok(bots)
        })
        .await
        .map_err(|e| ToolError(format!("查不到房间成员：{e}")))?;
        let others: Vec<&Bot> = members
            .iter()
            .filter(|bot| bot.id != self.from.id)
            .collect();
        let Some(target) = others.iter().find(|bot| bot.name.to_lowercase() == wanted) else {
            let names: Vec<&str> = others.iter().map(|bot| bot.name.as_str()).collect();
            return Err(ToolError(if wanted == self.from.name.to_lowercase() {
                "不能找自己。".to_string()
            } else if names.is_empty() {
                "房间里没有别的 Bot 可以找。请直接回答用户。".to_string()
            } else {
                format!(
                    "房间里没有叫「{}」的 Bot。可以找的有：{}。",
                    input.bot.trim(),
                    names.join("、")
                )
            }));
        };

        // 先过上限再发消息：被拒绝的话不会出现在房间里。发出去了才排队：对方开始时要看得到这句话。
        let job = self
            .link
            .episode()
            .ask(
                &self.from.id,
                self.hop.load(Ordering::SeqCst),
                &target.id,
                &message,
            )
            .map_err(ToolError)?;
        let engine = self.shared.engine.clone();
        let (room_id, from, to) = (
            self.room_id.clone(),
            self.from.id.clone(),
            target.id.clone(),
        );
        let content = format!("@{} {message}", target.name);
        let hop = job.hop;
        let posted =
            db(move || engine.post_bot_message(&room_id, &from, &content, vec![to], hop)).await;
        if let Err(error) = posted {
            // 话没发出去，对方就不该被叫起来。
            self.link.episode().cancel();
            return Err(ToolError(format!("这句话没发出去：{error}")));
        }
        // 对方在自己的道上马上开始，不用等这边说完。
        self.link.episode().deliver(job);
        self.link.wake.notify_one();
        Ok(format!(
            "已经把话带给 {}，它已经开始了。它的回答会作为新消息出现在房间里，不用等，也不用再问一次。",
            target.name
        ))
    }
}
