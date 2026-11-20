//! Telegram：把那边对机器人说的话收进房间，把房间里的回复送回去。
//!
//! 收和送是两条互不相干的循环。收：长轮询，一条更新处理完才把位置往前挪并记下来，
//! 重启后从记下的位置接着收；更新的编号又是幂等键，收重了也不会变成两条消息。
//! 送：从待发送里一条一条取，没送成的按原因决定等多久再试。

use crate::room::db;
use crate::Orchestrator;
use sa_adapters::telegram::{split_text, Intent, Telegram, TelegramError, TgMessage, Update};
use sa_engine::{now_ms, EngineError, Incoming, IncomingKind};
use sa_protocol::Source;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct TelegramOptions {
    /// 长轮询一次最多等多少秒。
    pub wait_secs: u64,
    /// 多久看一遍有没有要送出去的话。
    pub every: Duration,
    /// 出了临时问题之后等多久再试。之后每次翻倍。
    pub backoff: Duration,
}

impl Default for TelegramOptions {
    fn default() -> Self {
        Self {
            wait_secs: 25,
            every: Duration::from_secs(1),
            backoff: Duration::from_secs(5),
        }
    }
}

/// 一条话最多试这么多次。
const MAX_ATTEMPTS: u32 = 8;
/// 两次重试之间最多等这么久。
const MAX_BACKOFF: Duration = Duration::from_secs(300);
/// 放了这么久还没送出去的话不送了：服务停了一夜，第二天一早不该冒出一堆昨天的话。
const STALE_MS: i64 = 60 * 60 * 1000;

const HINT_PAIR: &str =
    "这个聊天还没有连到房间。请在 SuperAgent 的「连接」页里生成配对码，然后在这里发：/pair 配对码";
const HINT_BAD_CODE: &str = "配对码不对，或者已经过期了（十分钟内有效）。请重新生成一个。";

impl Orchestrator {
    /// 连上 Telegram：开始收消息、送回复，直到服务关闭。马上返回。
    pub fn start_telegram(self: &Arc<Self>, client: Telegram, options: TelegramOptions) {
        let this = self.clone();
        tokio::spawn(async move {
            // 先问清楚自己是谁：群里要靠用户名认出哪些话是对机器人说的。
            let mut wait = options.backoff;
            let me = loop {
                match client.me().await {
                    Ok(me) => break me,
                    Err(error) => {
                        tracing::warn!("连不上 Telegram：{error}");
                        if this.pause(wait).await {
                            return;
                        }
                        wait = (wait * 2).min(MAX_BACKOFF);
                    }
                }
            };
            let username = me.username.clone().unwrap_or_default();
            tracing::info!("已连上 Telegram：@{username}");
            *this.shared.telegram_username.lock().unwrap() = Some(username.clone());
            let sender = {
                let (this, client, options) = (this.clone(), client.clone(), options.clone());
                tokio::spawn(async move { this.telegram_send(client, options).await })
            };
            this.telegram_poll(client, options, me.id, &username).await;
            let _ = sender.await;
        });
    }

    /// 等一会儿。服务在这期间关了返回 `true`。
    async fn pause(&self, wait: Duration) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(wait) => false,
            _ = self.shared.shutdown.cancelled() => true,
        }
    }

    async fn telegram_poll(&self, client: Telegram, options: TelegramOptions, id: i64, me: &str) {
        // 换了机器人，更新的编号从头算：位置和幂等键都带上是哪个机器人。
        let cursor = format!("telegram.offset.{id}");
        let engine = self.shared.engine.clone();
        let saved = {
            let (engine, cursor) = (engine.clone(), cursor.clone());
            db(move || engine.kv_get(&cursor)).await
        };
        let mut offset: i64 = saved
            .ok()
            .flatten()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let mut wait = options.backoff;
        loop {
            let updates = tokio::select! {
                updates = client.updates(offset, options.wait_secs) => updates,
                _ = self.shared.shutdown.cancelled() => return,
            };
            let updates = match updates {
                Ok(updates) => {
                    wait = options.backoff;
                    updates
                }
                Err(error) => {
                    tracing::warn!("收 Telegram 消息出错：{error}");
                    let delay = match error {
                        TelegramError::TooFast { retry_after } => Duration::from_secs(retry_after),
                        _ => wait,
                    };
                    wait = (wait * 2).min(MAX_BACKOFF);
                    if self.pause(delay).await {
                        return;
                    }
                    continue;
                }
            };
            for update in updates {
                let next = update.update_id + 1;
                if let Err(error) = self.telegram_update(update, id, me).await {
                    // 库写不进去：不往前挪，等一会儿从这一条重新来。
                    tracing::warn!("处理 Telegram 消息出错：{error}");
                    if self.pause(options.backoff).await {
                        return;
                    }
                    break;
                }
                offset = offset.max(next);
                let (engine, cursor) = (engine.clone(), cursor.clone());
                let saved = db(move || engine.kv_put(&cursor, &offset.to_string())).await;
                if let Err(error) = saved {
                    tracing::warn!("记不下 Telegram 收到哪了：{error}");
                }
            }
        }
    }

    /// 处理一条更新。
    async fn telegram_update(&self, update: Update, id: i64, me: &str) -> Result<(), EngineError> {
        let Some(message) = update.message else {
            return Ok(());
        };
        let key = format!("{id}:{}", update.update_id);
        let chat = message.chat.id.to_string();
        let engine = self.shared.engine.clone();
        match sa_adapters::telegram::intent(&message, me) {
            Intent::Skip(_) => Ok(()),
            Intent::Pair(code) => {
                let title = chat_title(&message);
                db(move || {
                    if !engine.note_trigger(Source::Telegram, &key, "pair", "")? {
                        return Ok(());
                    }
                    let answer = if code.is_empty() {
                        HINT_PAIR.to_string()
                    } else {
                        match engine.redeem_pairing(&code, &chat, &title)? {
                            Some(link) => {
                                let room = engine.get_room(&link.room_id)?;
                                format!(
                                    "已经连到房间「{}」。之后在这里对我说的话会交给房间里的 Bot。",
                                    room.name
                                )
                            }
                            None => HINT_BAD_CODE.to_string(),
                        }
                    };
                    engine.send_out(Source::Telegram, &chat, &answer, &format!("reply:{key}"))?;
                    Ok(())
                })
                .await
            }
            Intent::Say(text) => {
                let linked = {
                    let (engine, chat) = (engine.clone(), chat.clone());
                    db(move || engine.find_link(Source::Telegram, &chat)).await?
                };
                if linked.is_none() {
                    // 没连接的私聊告诉对方怎么连；群里不出声，免得被拉进陌生的群之后刷屏。
                    if message.chat.kind != "private" {
                        return Ok(());
                    }
                    return db(move || {
                        if engine.note_trigger(Source::Telegram, &key, "message", "没有连到房间")?
                        {
                            let reply = format!("reply:{key}");
                            engine.send_out(Source::Telegram, &chat, HINT_PAIR, &reply)?;
                        }
                        Ok(())
                    })
                    .await;
                }
                let label = message
                    .from
                    .as_ref()
                    .map_or_else(|| "Telegram 用户".to_string(), |from| from.display());
                let incoming = Incoming {
                    source: Source::Telegram,
                    key,
                    name: "message".into(),
                    external: chat,
                    label,
                    url: None,
                    kind: IncomingKind::Chat { text },
                };
                match self.receive(incoming).await {
                    Ok(_) => Ok(()),
                    Err(crate::OrchestratorError::Engine(error)) => Err(error),
                    // 服务正在关：这一条不往前挪，下次启动再收。
                    Err(error) => Err(EngineError::Invalid(error.to_string())),
                }
            }
        }
    }

    async fn telegram_send(&self, client: Telegram, options: TelegramOptions) {
        // 一条长话切成几段送，送到一半没成的，下次从没送的那一段接着送。
        let mut progress: HashMap<i64, usize> = HashMap::new();
        loop {
            if self.pause(options.every).await {
                return;
            }
            let engine = self.shared.engine.clone();
            let batch = {
                let engine = engine.clone();
                db(move || engine.pending_outbound(Source::Telegram, 20)).await
            };
            let batch = match batch {
                Ok(batch) => batch,
                Err(error) => {
                    tracing::warn!("读不出要送到 Telegram 的话：{error}");
                    continue;
                }
            };
            // 这一遍里没送成的聊天：它后面的话先不送，保持先后。
            let mut blocked: HashSet<String> = HashSet::new();
            let mut hold = None;
            for out in batch {
                if blocked.contains(&out.target) {
                    continue;
                }
                let stale = now_ms() - out.created_at > STALE_MS;
                let parts = split_text(&out.body);
                let mut sent = progress.get(&out.id).copied().unwrap_or(0);
                let mut failure = stale.then(|| (None, "放得太久，不送了".to_string()));
                while failure.is_none() && sent < parts.len() {
                    match client.send(&out.target, &parts[sent]).await {
                        Ok(()) => sent += 1,
                        Err(error) => {
                            let tries = out.attempts + 1;
                            let again = match &error {
                                TelegramError::Rejected(_) => None,
                                _ if tries >= MAX_ATTEMPTS => None,
                                TelegramError::TooFast { retry_after } => {
                                    // 整个机器人被限速了：别的话也先停一停。
                                    let wait = Duration::from_secs(*retry_after);
                                    hold = Some(wait);
                                    Some(wait)
                                }
                                TelegramError::Unreachable(_) => {
                                    let factor = 2u32.saturating_pow(out.attempts.min(16));
                                    Some(options.backoff.saturating_mul(factor).min(MAX_BACKOFF))
                                }
                            };
                            failure = Some((again, error.to_string()));
                        }
                    }
                }
                let id = out.id;
                let engine = engine.clone();
                let saved = match failure {
                    None => {
                        progress.remove(&id);
                        db(move || engine.outbound_sent(id)).await
                    }
                    Some((again, error)) => {
                        blocked.insert(out.target.clone());
                        match again {
                            Some(_) => progress.insert(id, sent),
                            None => {
                                tracing::warn!("有一句话没能送到 Telegram：{error}");
                                progress.remove(&id)
                            }
                        };
                        let retry_at = again.map(|wait| now_ms() + wait.as_millis() as i64);
                        db(move || engine.outbound_failed(id, &error, retry_at)).await
                    }
                };
                if let Err(error) = saved {
                    tracing::warn!("记不下送到 Telegram 的结果：{error}");
                }
                if hold.is_some() {
                    break;
                }
            }
            if let Some(wait) = hold {
                if self.pause(wait).await {
                    return;
                }
            }
        }
    }
}

/// 连接显示成什么名字：群用群名，私聊用对方的名字。
fn chat_title(message: &TgMessage) -> String {
    match (&message.chat.title, &message.from) {
        (Some(title), _) if !title.is_empty() => title.clone(),
        (_, Some(from)) => from.display(),
        _ => format!("聊天 {}", message.chat.id),
    }
}
