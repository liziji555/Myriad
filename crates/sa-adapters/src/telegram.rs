//! Telegram Bot API：收消息（长轮询）、发消息，以及「这条消息要不要理」的判断。

use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

/// Telegram 一条消息最多 4096 个字符，留一点余量。
pub const MAX_TEXT_CHARS: usize = 4000;

#[derive(Debug, thiserror::Error)]
pub enum TelegramError {
    /// 发得太快了：过 `retry_after` 秒再试。
    #[error("Telegram 让等 {retry_after} 秒再发")]
    TooFast { retry_after: u64 },
    /// 这个请求不会成功（聊天不存在、被拉黑、令牌不对），重试没有用。
    #[error("Telegram 拒绝了：{0}")]
    Rejected(String),
    /// 网络或者 Telegram 那边的临时问题，可以再试。
    #[error("连不上 Telegram：{0}")]
    Unreachable(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<TgMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TgMessage {
    pub message_id: i64,
    pub chat: Chat,
    #[serde(default)]
    pub from: Option<User>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub reply_to_message: Option<Box<TgMessage>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Chat {
    pub id: i64,
    /// `private`、`group`、`supergroup`、`channel`。
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct User {
    pub id: i64,
    #[serde(default)]
    pub is_bot: bool,
    #[serde(default)]
    pub first_name: String,
    #[serde(default)]
    pub last_name: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
}

impl User {
    /// 给人看的名字。
    pub fn display(&self) -> String {
        let full = match &self.last_name {
            Some(last) if !last.is_empty() => format!("{} {last}", self.first_name),
            _ => self.first_name.clone(),
        };
        match (full.trim(), &self.username) {
            ("", Some(username)) => username.clone(),
            ("", None) => format!("用户 {}", self.id),
            (name, _) => name.to_string(),
        }
    }
}

/// 一条消息看过之后该怎么办。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// `/start 配对码` 或者 `/pair 配对码`。
    Pair(String),
    /// 对机器人说的话（已经去掉了 `@机器人`）。
    Say(String),
    /// 不是对机器人说的，或者不是文字。
    Skip(&'static str),
}

/// `me` 是机器人自己的用户名（不带 `@`）。私聊里的文字都算对它说的；
/// 群里只有 `@它` 的、回复它的才算，别人之间聊天不掺和。
pub fn intent(message: &TgMessage, me: &str) -> Intent {
    if message.from.as_ref().is_some_and(|from| from.is_bot) {
        return Intent::Skip("机器人发的");
    }
    let Some(text) = message.text.as_deref().map(str::trim) else {
        return Intent::Skip("不是文字");
    };
    let at_me = format!("@{}", me.to_lowercase());
    if let Some(rest) = text.strip_prefix('/') {
        let (command, argument) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        // 群里的命令可以写成 `/pair@机器人`；写给别的机器人的不管。
        let (name, to) = command.split_once('@').unwrap_or((command, me));
        if !to.eq_ignore_ascii_case(me) {
            return Intent::Skip("写给别的机器人的命令");
        }
        return match name.to_lowercase().as_str() {
            "start" | "pair" => Intent::Pair(argument.trim().to_string()),
            _ => Intent::Skip("不认识的命令"),
        };
    }
    let lower = text.to_lowercase();
    let mentioned = !me.is_empty() && lower.contains(&at_me);
    let replied = message
        .reply_to_message
        .as_ref()
        .and_then(|replied| replied.from.as_ref())
        .and_then(|from| from.username.as_deref())
        .is_some_and(|username| username.eq_ignore_ascii_case(me));
    if message.chat.kind != "private" && !mentioned && !replied {
        return Intent::Skip("群里不是对机器人说的");
    }
    let said = if mentioned {
        strip_mention(text, &at_me)
    } else {
        text.to_string()
    };
    if said.trim().is_empty() {
        return Intent::Skip("没有文字");
    }
    Intent::Say(said.trim().to_string())
}

/// 去掉文字里的 `@机器人`（不分大小写）。
fn strip_mention(text: &str, at_me: &str) -> String {
    let lower = text.to_lowercase();
    let mut out = String::new();
    let mut from = 0;
    // 小写之后字节位置可能和原文对不上（极少数字符）：对不上就原样留着。
    if lower.len() != text.len() {
        return text.to_string();
    }
    while let Some(at) = lower[from..].find(at_me) {
        out.push_str(&text[from..from + at]);
        from += at + at_me.len();
    }
    out.push_str(&text[from..]);
    out
}

/// 把一段话切成 Telegram 一条装得下的几段，尽量在换行的地方切。
pub fn split_text(text: &str) -> Vec<String> {
    let mut parts = vec![];
    let mut rest: Vec<char> = text.chars().collect();
    while rest.len() > MAX_TEXT_CHARS {
        let window = &rest[..MAX_TEXT_CHARS];
        let cut = window
            .iter()
            .rposition(|c| *c == '\n')
            .filter(|at| *at > MAX_TEXT_CHARS / 2)
            .map_or(MAX_TEXT_CHARS, |at| at + 1);
        parts.push(rest[..cut].iter().collect::<String>());
        rest.drain(..cut);
    }
    if !rest.is_empty() {
        parts.push(rest.into_iter().collect());
    }
    parts
}

/// Telegram Bot API 的客户端。
#[derive(Clone)]
pub struct Telegram {
    http: reqwest::Client,
    base: String,
    token: String,
}

#[derive(Deserialize)]
struct Envelope<T> {
    ok: bool,
    #[serde(default = "Option::default")]
    result: Option<T>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    parameters: Option<Parameters>,
}

#[derive(Deserialize)]
struct Parameters {
    #[serde(default)]
    retry_after: Option<u64>,
}

impl Telegram {
    /// `base` 是接口地址（`https://api.telegram.org`），`token` 是 BotFather 给的令牌。
    pub fn new(base: &str, token: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
        }
    }

    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        body: serde_json::Value,
        timeout: Duration,
    ) -> Result<T, TelegramError> {
        let url = format!("{}/bot{}/{method}", self.base, self.token);
        // 报错里不能带地址：地址里有令牌。
        let unreachable =
            |error: reqwest::Error| TelegramError::Unreachable(error.without_url().to_string());
        let response = self
            .http
            .post(url)
            .timeout(timeout)
            .json(&body)
            .send()
            .await
            .map_err(unreachable)?;
        let status = response.status();
        let envelope: Envelope<T> = response.json().await.map_err(unreachable)?;
        if envelope.ok {
            return envelope
                .result
                .ok_or_else(|| TelegramError::Unreachable("回应里没有结果".into()));
        }
        let description = envelope.description.unwrap_or_default();
        if let Some(retry_after) = envelope.parameters.and_then(|p| p.retry_after) {
            return Err(TelegramError::TooFast { retry_after });
        }
        if status.is_server_error() {
            return Err(TelegramError::Unreachable(description));
        }
        Err(TelegramError::Rejected(description))
    }

    /// 机器人自己是谁。
    pub async fn me(&self) -> Result<User, TelegramError> {
        self.call("getMe", json!({}), Duration::from_secs(15)).await
    }

    /// 取 `offset` 及之后的更新；没有新的就最多等 `wait` 秒。
    pub async fn updates(&self, offset: i64, wait: u64) -> Result<Vec<Update>, TelegramError> {
        let body = json!({ "offset": offset, "timeout": wait, "allowed_updates": ["message"] });
        self.call("getUpdates", body, Duration::from_secs(wait + 15))
            .await
    }

    /// 往一个聊天里发一段纯文字。
    pub async fn send(&self, chat_id: &str, text: &str) -> Result<(), TelegramError> {
        let body = json!({ "chat_id": chat_id, "text": text });
        let _: serde_json::Value = self
            .call("sendMessage", body, Duration::from_secs(30))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(kind: &str, text: &str) -> TgMessage {
        serde_json::from_value(json!({
            "message_id": 1,
            "chat": { "id": 42, "type": kind },
            "from": { "id": 7, "first_name": "三", "last_name": "张" },
            "text": text,
        }))
        .unwrap()
    }

    #[test]
    fn private_text_is_for_the_bot_and_groups_need_a_mention_or_a_reply() {
        let say = |text: &str| Intent::Say(text.into());
        assert_eq!(intent(&message("private", " 你好 "), "sa_bot"), say("你好"));
        assert_eq!(
            intent(&message("group", "今天吃什么"), "sa_bot"),
            Intent::Skip("群里不是对机器人说的")
        );
        assert_eq!(
            intent(&message("supergroup", "@SA_Bot 看下日志 @Coder"), "sa_bot"),
            say("看下日志 @Coder")
        );
        let mut reply = message("group", "再详细点");
        reply.reply_to_message = Some(Box::new(
            serde_json::from_value(json!({
                "message_id": 0,
                "chat": { "id": 42, "type": "group" },
                "from": { "id": 1, "is_bot": true, "first_name": "SA", "username": "SA_bot" },
            }))
            .unwrap(),
        ));
        assert_eq!(intent(&reply, "sa_bot"), say("再详细点"));

        let mut photo = message("private", "");
        photo.text = None;
        assert_eq!(intent(&photo, "sa_bot"), Intent::Skip("不是文字"));
        let mut bot = message("private", "hi");
        bot.from.as_mut().unwrap().is_bot = true;
        assert_eq!(intent(&bot, "sa_bot"), Intent::Skip("机器人发的"));
        assert_eq!(
            intent(&message("group", "@sa_bot"), "sa_bot"),
            Intent::Skip("没有文字")
        );
    }

    #[test]
    fn pairing_commands_are_recognised() {
        let pair = |code: &str| Intent::Pair(code.into());
        assert_eq!(
            intent(&message("private", "/start ABC234"), "sa_bot"),
            pair("ABC234")
        );
        assert_eq!(intent(&message("private", "/start"), "sa_bot"), pair(""));
        assert_eq!(
            intent(&message("group", "/pair@SA_bot  abc234 "), "sa_bot"),
            pair("abc234")
        );
        assert_eq!(
            intent(&message("group", "/pair@other_bot abc234"), "sa_bot"),
            Intent::Skip("写给别的机器人的命令")
        );
        assert_eq!(
            intent(&message("private", "/help"), "sa_bot"),
            Intent::Skip("不认识的命令")
        );
    }

    #[test]
    fn names_fall_back_sensibly() {
        let user = |value: serde_json::Value| serde_json::from_value::<User>(value).unwrap();
        assert_eq!(
            user(json!({ "id": 7, "first_name": "三", "last_name": "张" })).display(),
            "三 张"
        );
        assert_eq!(
            user(json!({ "id": 7, "first_name": "", "username": "zs" })).display(),
            "zs"
        );
        assert_eq!(user(json!({ "id": 7 })).display(), "用户 7");
    }

    #[test]
    fn long_text_is_cut_at_line_breaks_without_losing_anything() {
        assert_eq!(split_text("你好"), ["你好"]);
        assert!(split_text("").is_empty());
        let line = "字".repeat(999) + "\n";
        let text = line.repeat(9);
        let parts = split_text(&text);
        assert_eq!(parts.len(), 3);
        assert!(parts
            .iter()
            .all(|part| part.chars().count() <= MAX_TEXT_CHARS));
        assert!(parts[0].ends_with('\n'));
        assert_eq!(parts.concat(), text);
        // 没有换行可切的就硬切。
        let solid = "字".repeat(MAX_TEXT_CHARS + 5);
        let parts = split_text(&solid);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts.concat(), solid);
    }
}
