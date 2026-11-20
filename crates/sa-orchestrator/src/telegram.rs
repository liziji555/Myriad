//! Telegram：把那边对机器人说的话收进房间，把房间里的回复送回去。
//!
//! 收和送是两条互不相干的循环。收：长轮询，一条更新处理完才把位置往前挪（也就是向 Telegram 确认）；
//! 更新的编号又是幂等键，重启后重新收到的不会变成两条消息。
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
const HINT_LOCKED: &str = "配对码不对。试的次数太多了，过十分钟再试。";

/// 配对码只有六位，不能让人一直猜：这么长的一段时间里……
const PAIR_WINDOW_MS: i64 = 10 * 60 * 1000;
/// ……一个聊天最多试错这么多次，
const PAIR_TRIES_PER_CHAT: u32 = 5;
/// 所有聊天加起来最多试错这么多次（换着聊天猜也一样被挡住）。
const PAIR_TRIES_TOTAL: u32 = 20;

/// 记着这一段时间里配对码试错了几次。超了的在这段时间过去之前不再理会。
#[derive(Debug, Default)]
struct PairGuard {
    started: i64,
    total: u32,
    per_chat: HashMap<String, u32>,
}

impl PairGuard {
    /// 这个聊天现在还能试错几次。
    fn left(&mut self, chat: &str, now: i64) -> u32 {
        if now - self.started >= PAIR_WINDOW_MS {
            *self = Self {
                started: now,
                ..Self::default()
            };
        }
        let mine = self.per_chat.get(chat).copied().unwrap_or(0);
        (PAIR_TRIES_PER_CHAT.saturating_sub(mine)).min(PAIR_TRIES_TOTAL.saturating_sub(self.total))
    }

    fn wrong(&mut self, chat: &str) {
        self.total += 1;
        *self.per_chat.entry(chat.to_string()).or_default() += 1;
    }
}

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
        // 位置只用来告诉 Telegram「这之前的都收到了」，不用存：不带位置去取，拿到的就是还没确认的。
        // 重启后重新拿到的那几条靠幂等键认出来。
        let mut offset: i64 = 0;
        let mut guard = PairGuard::default();
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
            if updates.is_empty() {
                // 都确认过了。下一次不带位置去取：Telegram 隔久了会重新编号，
                // 新的编号可能比记着的位置小，带着旧位置去取会把它们当成已经收到的丢掉。
                offset = 0;
            }
            for update in updates {
                let next = update.update_id + 1;
                if let Err(error) = self.telegram_update(update, id, me, &mut guard).await {
                    // 库写不进去：不往前挪，等一会儿从这一条重新来。
                    tracing::warn!("处理 Telegram 消息出错：{error}");
                    if self.pause(options.backoff).await {
                        return;
                    }
                    break;
                }
                offset = next;
            }
        }
    }

    /// 处理一条更新。
    async fn telegram_update(
        &self,
        update: Update,
        id: i64,
        me: &str,
        guard: &mut PairGuard,
    ) -> Result<(), EngineError> {
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
                // 试错太多的先不理：不验码，也不回话。
                let left = guard.left(&chat, now_ms());
                if !code.is_empty() && left == 0 {
                    return Ok(());
                }
                let target = chat.clone();
                let wrong = db(move || {
                    if !engine.note_trigger(Source::Telegram, &key, "pair", "")? {
                        return Ok(false);
                    }
                    let mut wrong = false;
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
                            None => {
                                wrong = true;
                                let hint = if left == 1 {
                                    HINT_LOCKED
                                } else {
                                    HINT_BAD_CODE
                                };
                                hint.to_string()
                            }
                        }
                    };
                    engine.send_out(Source::Telegram, &chat, &answer, &format!("reply:{key}"))?;
                    Ok(wrong)
                })
                .await?;
                if wrong {
                    guard.wrong(&target);
                }
                Ok(())
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
                    // 取出来之后连接可能已经断开了：每送一段之前再看一眼，作废了的不送。
                    let (engine, id) = (engine.clone(), out.id);
                    match db(move || engine.outbound_pending(id)).await {
                        Ok(true) => {}
                        Ok(false) => {
                            failure = Some((None, "连接断开了".to_string()));
                            break;
                        }
                        Err(error) => {
                            failure = Some((Some(options.backoff), error.to_string()));
                            break;
                        }
                    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrong_pairing_codes_are_capped_per_chat_and_overall_until_the_window_passes() {
        let mut guard = PairGuard::default();
        let start = 1_000_000;
        for used in 0..PAIR_TRIES_PER_CHAT {
            assert_eq!(guard.left("42", start), PAIR_TRIES_PER_CHAT - used);
            guard.wrong("42");
        }
        assert_eq!(guard.left("42", start + 1), 0);
        // 别的聊天不受它连累。
        assert_eq!(guard.left("43", start + 1), PAIR_TRIES_PER_CHAT);

        // 换着聊天猜：总数到了，谁都不能再试。
        for chat in 0..PAIR_TRIES_TOTAL {
            let chat = format!("c{chat}");
            if guard.left(&chat, start + 2) > 0 {
                guard.wrong(&chat);
            }
        }
        assert_eq!(guard.left("fresh", start + 3), 0);

        // 过了这一段时间，重新算。
        assert_eq!(guard.left("fresh", start + PAIR_WINDOW_MS - 1), 0);
        assert_eq!(
            guard.left("42", start + PAIR_WINDOW_MS),
            PAIR_TRIES_PER_CHAT
        );
    }
}
