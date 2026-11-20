//! 没有点名时由谁来接：先用程序能判断的，判断不了再问便宜的模型。

use crate::Shared;
use futures_util::StreamExt;
use sa_llm::{open_with_retry, Accumulator, ChatMessage, ChatRequest, LlmError, RetryPolicy};
use sa_protocol::{Bot, BotId, Message, ModelTier, RoomId, Sender};
use serde::Deserialize;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

const ROUTER_PROMPT: &str = include_str!("../prompts/router.md");
/// 把握低于这个数就换强一档的模型再判一次。
const MIN_CONFIDENCE: f64 = 0.55;
/// 给路由看最近几条对话。
const RECENT_MESSAGES: usize = 6;
/// 每条对话、每段分工说明最多带这么多字。
const CLIP_CHARS: usize = 300;

#[derive(Deserialize)]
struct Verdict {
    bot_id: String,
    #[serde(default)]
    confidence: f64,
}

/// 从 `candidates`（房间里启用着的成员，不为空）里选一个来接 `message`。
///
/// 只有一个候选时不问模型。模型答不上来、答得不合规矩、把握不够（换强模型再问一次之后仍然如此）时，
/// 交给 `fallback`（入口 Bot）。被取消时返回 `None`。
pub(crate) async fn route(
    shared: &Arc<Shared>,
    room_id: &RoomId,
    candidates: &[Bot],
    fallback: &BotId,
    recent: &[Message],
    message: &Message,
    cancel: &CancellationToken,
) -> Option<BotId> {
    if let [only] = candidates {
        return Some(only.id.clone());
    }
    let material = describe(candidates, fallback, recent, message);
    for tier in [ModelTier::Cheap, ModelTier::Strong] {
        match ask(shared, room_id, tier, &material, cancel).await {
            Err(LlmError::Cancelled) => return None,
            Err(error) => {
                tracing::warn!(room = %room_id, "路由（{}）没给出结果：{error}", tier.as_str());
            }
            Ok(text) => match parse(&text, candidates) {
                Some((bot_id, confidence)) if confidence >= MIN_CONFIDENCE => return Some(bot_id),
                Some((_, confidence)) => {
                    tracing::info!(room = %room_id, "路由（{}）把握不够：{confidence}", tier.as_str());
                }
                None => {
                    tracing::warn!(room = %room_id, "路由（{}）的回答不合规矩：{text}", tier.as_str());
                }
            },
        }
    }
    Some(fallback.clone())
}

/// 调一次模型，返回它说的话。
async fn ask(
    shared: &Arc<Shared>,
    room_id: &RoomId,
    tier: ModelTier,
    material: &str,
    cancel: &CancellationToken,
) -> Result<String, LlmError> {
    let model = shared.models.resolve(tier).map_err(LlmError::Auth)?;
    let request = ChatRequest {
        model: model.model.clone(),
        messages: vec![
            ChatMessage::system(ROUTER_PROMPT.to_string()),
            ChatMessage::user(material.to_string()),
        ],
        tools: vec![],
        max_tokens: Some(300),
        // 分类不需要思考，要的是快。
        thinking: Some(false),
        session_id: None,
    };
    let mut reply = Accumulator::default();
    let provider = crate::budget::guard(shared, model.provider.clone(), None);
    let mut stream =
        open_with_retry(provider.as_ref(), &request, cancel, RetryPolicy::default()).await?;
    let failure = loop {
        let event = tokio::select! {
            biased;
            _ = cancel.cancelled() => break Some(LlmError::Cancelled),
            event = stream.next() => event,
        };
        match event {
            Some(Ok(event)) => reply.push(&event),
            Some(Err(error)) => break Some(error),
            None => break None,
        }
    };
    if let Some(usage) = reply.usage {
        crate::budget::record(shared, room_id, None, None, &model.model, usage).await;
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(reply.text),
    }
}

/// 给路由看的材料。
fn describe(candidates: &[Bot], fallback: &BotId, recent: &[Message], message: &Message) -> String {
    let name_of = |id: &BotId| {
        candidates
            .iter()
            .find(|bot| &bot.id == id)
            .map_or("已经不在房间里的 Bot", |bot| bot.name.as_str())
    };
    let mut out = String::from("【房间里的 Bot】\n");
    for bot in candidates {
        let role = if bot.persona.is_empty() {
            "（没有写分工）".to_string()
        } else {
            clip(&bot.persona)
        };
        let entry = if &bot.id == fallback {
            "，入口 Bot"
        } else {
            ""
        };
        out.push_str(&format!(
            "- id: {}，名字: {}{entry}，分工: {role}\n",
            bot.id, bot.name
        ));
    }
    out.push_str("\n【最近的对话】\n");
    let earlier: Vec<&Message> = recent
        .iter()
        .filter(|m| m.id != message.id && !m.content.trim().is_empty())
        .collect();
    if earlier.is_empty() {
        out.push_str("（还没有）\n");
    }
    let skip = earlier.len().saturating_sub(RECENT_MESSAGES);
    for earlier in &earlier[skip..] {
        let who = match &earlier.sender {
            Sender::User => "用户",
            Sender::Bot { bot_id } => name_of(bot_id),
        };
        out.push_str(&format!("{who}：{}\n", clip(&earlier.content)));
    }
    out.push_str(&format!(
        "\n【用户刚发的消息】\n{}\n",
        clip(&message.content)
    ));
    out
}

fn clip(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = flat.chars();
    let mut out: String = chars.by_ref().take(CLIP_CHARS).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

/// 从模型的回答里取出选中的 Bot 和把握。回答里可以夹着别的字（有的模型爱加代码块），
/// 但选的必须是候选之一，把握必须在 0 到 1 之间。
fn parse(text: &str, candidates: &[Bot]) -> Option<(BotId, f64)> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    let verdict: Verdict = serde_json::from_str(text.get(start..=end)?).ok()?;
    let bot = candidates
        .iter()
        .find(|bot| bot.id.as_str() == verdict.bot_id)?;
    (0.0..=1.0)
        .contains(&verdict.confidence)
        .then(|| (bot.id.clone(), verdict.confidence))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sa_protocol::{BotKind, MessageId, MessageState};

    fn bot(name: &str, persona: &str) -> Bot {
        Bot {
            id: BotId::from(format!("bot_{name}")),
            name: name.into(),
            kind: BotKind::Assistant,
            tier: ModelTier::Mid,
            persona: persona.into(),
            enabled: true,
            created_at: 0,
        }
    }

    fn message(seq: i64, sender: Sender, content: &str) -> Message {
        Message {
            id: MessageId::from(format!("msg_{seq}")),
            seq,
            room_id: RoomId::from("room_1".to_string()),
            sender,
            content: content.into(),
            mentions: vec![],
            hop: 0,
            kind: sa_protocol::MessageKind::Chat,
            task_id: None,
            run_id: None,
            origin: None,
            steps: vec![],
            state: MessageState::Complete,
            error: None,
            rev: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn verdicts_must_name_a_candidate_with_a_sane_confidence() {
        let team = [bot("coder", ""), bot("writer", "")];
        let pick = |text: &str| parse(text, &team).map(|(id, c)| (id.as_str().to_string(), c));
        assert_eq!(
            pick(r#"{"bot_id":"bot_writer","confidence":0.8,"reason":"写文档"}"#),
            Some(("bot_writer".into(), 0.8))
        );
        // 夹在代码块或别的话里也认。
        assert_eq!(
            pick("好的：\n```json\n{\"bot_id\": \"bot_coder\", \"confidence\": 1}\n```"),
            Some(("bot_coder".into(), 1.0))
        );
        // 没写把握按 0 算（之后会因为把握不够被换掉）。
        assert_eq!(
            pick(r#"{"bot_id":"bot_coder"}"#),
            Some(("bot_coder".into(), 0.0))
        );
        for bad in [
            "",
            "让 coder 来",
            r#"{"bot_id":"bot_nobody","confidence":0.9}"#,
            r#"{"bot_id":"bot_coder","confidence":1.5}"#,
            r#"{"bot_id":"bot_coder","confidence":-0.1}"#,
            r#"{"bot_id":42,"confidence":0.9}"#,
            r#"{"confidence":0.9}"#,
            "} {",
        ] {
            assert_eq!(pick(bad), None, "{bad}");
        }
    }

    #[test]
    fn material_lists_the_team_recent_talk_and_the_new_message() {
        let team = [bot("coder", "写代码\n改 bug"), bot("writer", "")];
        let fallback = team[0].id.clone();
        let new = message(9, Sender::User, "帮我  写一份\n更新说明");
        let recent = [
            message(1, Sender::User, "早"),
            message(
                2,
                Sender::Bot {
                    bot_id: team[1].id.clone(),
                },
                "早上好",
            ),
            message(
                3,
                Sender::Bot {
                    bot_id: BotId::from("bot_gone".to_string()),
                },
                "",
            ),
            new.clone(),
        ];
        assert_eq!(
            describe(&team, &fallback, &recent, &new),
            "【房间里的 Bot】\n\
             - id: bot_coder，名字: coder，入口 Bot，分工: 写代码 改 bug\n\
             - id: bot_writer，名字: writer，分工: （没有写分工）\n\
             \n【最近的对话】\n用户：早\nwriter：早上好\n\
             \n【用户刚发的消息】\n帮我 写一份 更新说明\n"
        );
        let long = message(1, Sender::User, &"长".repeat(CLIP_CHARS + 50));
        let text = describe(&team, &fallback, &[], &long);
        assert!(text.contains("（还没有）"));
        assert!(text.ends_with(&format!("{}…\n", "长".repeat(CLIP_CHARS))));
    }
}
