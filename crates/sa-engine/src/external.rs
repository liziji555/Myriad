//! 外面的世界：房间和 GitHub 仓库、Telegram 聊天的连接，外面送来的事，要送出去的话。
//!
//! 外面送来的每一件事都走 [`Engine::receive`]：先记下再处理，同一件事只收一次。

use crate::messages::add_message;
use crate::{find_mentions, now_ms, to_json, Engine, EngineError, Result, MAX_MESSAGE_CHARS};
use sa_protocol::{
    ArtifactKind, Bot, BotId, EventKind, Link, LinkId, LinkRef, Message, MessageId, MessageKind,
    MessageState, Millis, Origin, Pairing, Room, RoomId, Sender, Source, Task,
};
use sa_store::external::{self, NewTrigger, Outbound};
use sa_store::{bots, events, messages, rooms, tasks, Transaction};
use sha2::{Digest, Sha256};

/// 配对码多久过期。
pub const PAIRING_TTL_MS: Millis = 10 * 60 * 1000;
/// 配对码用的字符：去掉了容易看错的 0/O、1/I。
const PAIRING_ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const PAIRING_LEN: usize = 6;
/// 外面来的事，原文最多留这么多字。
const MAX_EVENT_BODY_CHARS: usize = 8_000;
/// 送到 Telegram 的一条话最多这么多字，再长的截掉（完整内容在房间里）。
const MAX_RELAY_CHARS: usize = 12_000;
/// 收到过的事的记录留多久。只用来认重复投递。
const TRIGGER_KEEP_MS: Millis = 30 * 24 * 60 * 60 * 1000;

/// 外面送来的一件事。适配器把它整理成这个样子再交进来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Incoming {
    pub source: Source,
    /// 来源给的幂等键：GitHub 的 delivery id，Telegram 的 update id。
    pub key: String,
    /// 是什么事，例如 `issues.opened`。只用来记录。
    pub name: String,
    /// 外面那一头：GitHub 是 `owner/name`，Telegram 是聊天 id。
    pub external: String,
    /// 给人看的出处：GitHub 是 `owner/name#12`，Telegram 是说话的人。
    pub label: String,
    pub url: Option<String>,
    pub kind: IncomingKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncomingKind {
    /// 人在外面说的一句话（Telegram）：和在房间里说的一样，照常点名、挑人。
    Chat { text: String },
    /// 一件事（GitHub）。
    Event {
        /// 一句话说明。
        summary: String,
        /// 原文。外人写的，只当资料。
        body: String,
        /// 这件事是哪个 PR 上的。和某个任务开的 PR 对上了，就交给那个任务的负责人。
        pr: Option<u64>,
        /// 这件事是哪个分支上的（CI 结果）。
        branch: Option<String>,
        /// 只关心任务的 PR 和分支：对不上任务的不收。
        task_only: bool,
        /// 要不要 Bot 来看。不要的只摆在时间线上。
        notify: bool,
    },
}

/// 一件事交进来之后的结果。
#[derive(Debug, Clone, PartialEq)]
pub enum Receipt {
    /// 收下了，成了房间里的一条消息。
    Stored(Box<Message>),
    /// 这件事之前送来过。
    Duplicate,
    /// 没收，以及原因。
    Filtered(String),
}

impl Engine {
    /// 外面送来一件事。记录、去重、落成消息在同一个事务里：重复投递不会变成两条消息。
    pub fn receive(&self, incoming: Incoming) -> Result<Receipt> {
        let now = now_ms();
        let (receipt, seq) = self.store.write(|tx| {
            if external::has_trigger(tx, incoming.source, &incoming.key)? {
                return Ok((Receipt::Duplicate, None));
            }
            external::prune_triggers(tx, now - TRIGGER_KEEP_MS)?;
            let external = normalize(incoming.source, &incoming.external);
            let link = external::find_link(tx, incoming.source, &external)?;
            let room = match &link {
                Some(link) => rooms::get(tx, &link.room_id)?,
                None => None,
            };
            let built = match &room {
                Some(room) => build(tx, room, &incoming, &external, now)?,
                None => Err("没有连到房间".to_string()),
            };
            let mut trigger = NewTrigger {
                source: incoming.source,
                key: &incoming.key,
                kind: &incoming.name,
                stored: false,
                note: "",
                room_id: room.as_ref().map(|room| &room.id),
                message_id: None,
            };
            match built {
                Ok(message) => {
                    let (message, seq) = add_message(tx, message)?;
                    trigger.stored = true;
                    trigger.message_id = Some(&message.id);
                    external::record_trigger(tx, &trigger, now)?;
                    Ok((Receipt::Stored(Box::new(message)), Some(seq)))
                }
                Err(note) => {
                    trigger.note = &note;
                    external::record_trigger(tx, &trigger, now)?;
                    Ok((Receipt::Filtered(note), None))
                }
            }
        })?;
        if let Some(seq) = seq {
            self.publish(seq);
        }
        Ok(receipt)
    }

    /// 记下一件不变成消息的事（配对、没连接的聊天里的话）。之前记过返回 `false`。
    pub fn note_trigger(&self, source: Source, key: &str, name: &str, note: &str) -> Result<bool> {
        let trigger = NewTrigger {
            source,
            key,
            kind: name,
            stored: false,
            note,
            room_id: None,
            message_id: None,
        };
        Ok(self
            .store
            .write(|tx| external::record_trigger(tx, &trigger, now_ms()))?)
    }

    /// 把一个 GitHub 仓库连到房间：这个仓库的事件进这个房间。
    pub fn link_repo(&self, room_id: &RoomId, repo: &str) -> Result<Link> {
        let repo = repo.trim();
        let valid = |part: &str| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        match repo.split_once('/') {
            Some((owner, name)) if valid(owner) && valid(name) => {}
            _ => {
                return Err(EngineError::Invalid(format!(
                    "仓库要写成 owner/name，现在是「{repo}」"
                )))
            }
        }
        self.add_link(
            room_id,
            Source::Github,
            &normalize(Source::Github, repo),
            repo,
            false,
        )
    }

    /// 断开一个连接。
    pub fn delete_link(&self, id: &LinkId) -> Result<()> {
        let seq = self
            .store
            .write(|tx| {
                let Some(link) = external::get_link(tx, id)? else {
                    return Ok(None);
                };
                Ok(Some(remove_link(tx, &link)?))
            })?
            .ok_or_else(|| EngineError::NotFound("没有这个连接".into()))?;
        self.publish(seq);
        Ok(())
    }

    pub fn list_links(&self) -> Result<Vec<Link>> {
        Ok(self.store.read(external::list_links)?)
    }

    /// 外面这一头连着哪个房间。
    pub fn find_link(&self, source: Source, external: &str) -> Result<Option<Link>> {
        let external = normalize(source, external);
        Ok(self
            .store
            .read(|c| external::find_link(c, source, &external))?)
    }

    /// 给房间生成一个 Telegram 配对码。房间原来的那个作废。码只在这里给一次，库里只存哈希。
    pub fn create_pairing(&self, room_id: &RoomId) -> Result<Pairing> {
        let code = new_code();
        let expires_at = now_ms() + PAIRING_TTL_MS;
        self.store
            .write(|tx| {
                if rooms::get(tx, room_id)?.is_none() {
                    return Ok(None);
                }
                external::put_pairing(tx, &hash_code(&code), room_id, expires_at)?;
                Ok(Some(()))
            })?
            .ok_or_else(|| EngineError::NotFound("没有这个房间".into()))?;
        Ok(Pairing {
            code,
            expires_at,
            bot_username: None,
        })
    }

    /// 在 Telegram 聊天里用配对码：对得上就把这个聊天连到房间（原来连着别的房间的改连过来）。
    /// 码不对或者过期了返回 `None`。
    pub fn redeem_pairing(&self, code: &str, chat_id: &str, title: &str) -> Result<Option<Link>> {
        let code: String = code
            .trim()
            .chars()
            .map(|c| c.to_ascii_uppercase())
            .collect();
        let hash = hash_code(&code);
        let now = now_ms();
        let found = self.store.write(|tx| {
            Ok(match external::take_pairing(tx, &hash, now)? {
                Some(room_id) => rooms::get(tx, &room_id)?,
                None => None,
            })
        })?;
        let Some(room) = found else {
            return Ok(None);
        };
        self.add_link(&room.id, Source::Telegram, chat_id, title, true)
            .map(Some)
    }

    fn add_link(
        &self,
        room_id: &RoomId,
        source: Source,
        external: &str,
        title: &str,
        replace: bool,
    ) -> Result<Link> {
        let link = Link {
            id: LinkId::new(),
            source,
            external: external.to_string(),
            room_id: room_id.clone(),
            title: title.chars().take(80).collect(),
            created_at: now_ms(),
        };
        let seq = self.store.write(|tx| {
            if rooms::get(tx, room_id)?.is_none() {
                return Ok(Err(EngineError::NotFound("没有这个房间".into())));
            }
            if let Some(old) = external::find_link(tx, source, external)? {
                if !replace {
                    let room = rooms::get(tx, &old.room_id)?;
                    let name = room.map_or_else(String::new, |room| room.name);
                    return Ok(Err(EngineError::Invalid(format!(
                        "「{external}」已经连到房间「{name}」了"
                    ))));
                }
                remove_link(tx, &old)?;
            }
            external::insert_link(tx, &link)?;
            Ok(Ok(events::append(
                tx,
                Some(room_id),
                EventKind::LinkCreated,
                &to_json(&link),
                link.created_at,
            )?))
        })??;
        self.publish(seq);
        Ok(link)
    }

    /// 现在该送出去的话。
    pub fn pending_outbound(&self, source: Source, limit: u32) -> Result<Vec<Outbound>> {
        let now = now_ms();
        Ok(self
            .store
            .read(|c| external::pending(c, source, now, limit))?)
    }

    pub fn outbound_sent(&self, id: i64) -> Result<()> {
        Ok(self
            .store
            .write(|tx| external::mark_sent(tx, id, now_ms()))?)
    }

    /// 这一次没送成。`retry_at` 给了就到时候再试，没给就不再试了。
    pub fn outbound_failed(&self, id: i64, error: &str, retry_at: Option<Millis>) -> Result<()> {
        Ok(self
            .store
            .write(|tx| external::mark_failed(tx, id, error, retry_at))?)
    }

    /// 直接往外面送一句话（不是哪条消息的转发），例如配对成功的回话。`key` 相同的只送一次。
    pub fn send_out(&self, source: Source, target: &str, body: &str, key: &str) -> Result<bool> {
        Ok(self
            .store
            .write(|tx| external::enqueue(tx, source, target, body, key, now_ms()))?)
    }
}

/// GitHub 的仓库名不分大小写，统一成小写；Telegram 的聊天 id 原样。
fn normalize(source: Source, external: &str) -> String {
    match source {
        Source::Github => external.trim().to_lowercase(),
        Source::Telegram => external.trim().to_string(),
    }
}

fn remove_link(tx: &Transaction<'_>, link: &Link) -> sa_store::Result<i64> {
    external::delete_link(tx, &link.id)?;
    let gone = LinkRef {
        id: link.id.clone(),
        room_id: link.room_id.clone(),
    };
    events::append(
        tx,
        Some(&link.room_id),
        EventKind::LinkDeleted,
        &to_json(&gone),
        now_ms(),
    )
}

/// 把送来的事变成房间里的一条消息。不收的返回原因。
fn build(
    tx: &Transaction<'_>,
    room: &Room,
    incoming: &Incoming,
    external: &str,
    now: Millis,
) -> sa_store::Result<std::result::Result<Message, String>> {
    let members: Vec<Bot> = bots::list(tx)?
        .into_iter()
        .filter(|bot| bot.enabled && room.member_bot_ids.contains(&bot.id))
        .collect();
    let is_member = |id: &BotId| members.iter().any(|bot| &bot.id == id);
    let mut origin = Origin {
        source: incoming.source,
        label: incoming.label.clone(),
        url: incoming.url.clone(),
        reply_to: None,
    };
    let (kind, content, mentions) = match &incoming.kind {
        IncomingKind::Chat { text } => {
            let text = text.trim();
            if text.is_empty() {
                return Ok(Err("没有文字".into()));
            }
            let content: String = text.chars().take(MAX_MESSAGE_CHARS).collect();
            origin.reply_to = Some(external.to_string());
            let mentions = find_mentions(&content, &members);
            (MessageKind::Chat, content, mentions)
        }
        IncomingKind::Event {
            summary,
            body,
            pr,
            branch,
            task_only,
            notify,
        } => {
            let task = tasks::list_in_room(tx, &room.id)?
                .into_iter()
                .rev()
                .find(|task| is_task_of(task, external, *pr, branch.as_deref()));
            if *task_only && task.is_none() {
                return Ok(Err("不是任务开的 PR 或分支".into()));
            }
            let mut summary = summary.trim().to_string();
            if let Some(task) = &task {
                summary.push_str(&format!("（任务「{}」，{}）", task.title, task.id));
            }
            let body: String = body.trim().chars().take(MAX_EVENT_BODY_CHARS).collect();
            // 任务的事交给任务的负责人，别的交给入口 Bot。它们不在了就只摆在时间线上。
            let owner = task.and_then(|task| task.owner_bot_id);
            let target = owner
                .filter(|id| is_member(id))
                .or_else(|| room.host_bot_id.clone().filter(|id| is_member(id)));
            let mentions = target.filter(|_| *notify).into_iter().collect();
            (MessageKind::Event, format!("{summary}\n\n{body}"), mentions)
        }
    };
    Ok(Ok(Message {
        id: MessageId::new(),
        seq: 0,
        room_id: room.id.clone(),
        sender: Sender::User,
        content,
        mentions,
        hop: 0,
        kind,
        task_id: None,
        run_id: None,
        origin: Some(origin),
        steps: vec![],
        state: MessageState::Complete,
        error: None,
        rev: 0,
        created_at: now,
        updated_at: now,
    }))
}

/// 这个任务开的 PR 是不是 `repo` 的第 `pr` 号，或者它的分支是不是 `branch`。
fn is_task_of(task: &Task, repo: &str, pr: Option<u64>, branch: Option<&str>) -> bool {
    let by_pr = pr.is_some_and(|number| {
        let suffix = format!("/{repo}/pull/{number}");
        task.artifacts.iter().any(|artifact| {
            artifact.kind == ArtifactKind::Pr
                && artifact
                    .reference
                    .trim_end_matches('/')
                    .to_lowercase()
                    .ends_with(&suffix)
        })
    });
    let has_pr = task
        .artifacts
        .iter()
        .any(|artifact| artifact.kind == ArtifactKind::Pr);
    by_pr || (has_pr && branch.is_some() && task.branch.as_deref() == branch)
}

/// 一条 Bot 的话写完了：该送到 Telegram 的记进待发送。和消息定稿在同一个事务里。
///
/// 送不送看它在回谁：回 Telegram 里说的话，送回那个聊天；定时任务和外面来的事引出的话没有人在界面前等着，
/// 送到房间连着的每个 Telegram 聊天；回界面里说的话，不送。
pub(crate) fn relay(tx: &Transaction<'_>, message: &Message) -> sa_store::Result<()> {
    let Sender::Bot { bot_id } = &message.sender else {
        return Ok(());
    };
    if message.kind != MessageKind::Chat {
        return Ok(());
    }
    let text = match message.state {
        MessageState::Complete if !message.content.trim().is_empty() => message.content.clone(),
        MessageState::Failed => {
            let reason = message.error.as_deref().unwrap_or("出错了");
            format!("{}\n（没答成：{reason}）", message.content)
                .trim()
                .to_string()
        }
        _ => return Ok(()),
    };
    let Some(trigger) = messages::last_trigger_before(tx, &message.room_id, message.seq)? else {
        return Ok(());
    };
    let linked = external::room_links(tx, &message.room_id, Source::Telegram)?;
    let from_chat = trigger
        .origin
        .as_ref()
        .filter(|origin| origin.source == Source::Telegram)
        .and_then(|origin| origin.reply_to.clone());
    let targets: Vec<String> = match (trigger.kind, from_chat) {
        // 聊天已经断开了就不再往里送。
        (MessageKind::Chat, Some(chat)) => linked
            .into_iter()
            .filter(|link| link.external == chat)
            .map(|link| link.external)
            .collect(),
        (MessageKind::Routine | MessageKind::Event, _) => {
            linked.into_iter().map(|link| link.external).collect()
        }
        _ => vec![],
    };
    if targets.is_empty() {
        return Ok(());
    }
    // 房间里不止一个 Bot 时带上是谁说的：在 Telegram 里它们用的是同一个号。
    let room = rooms::get(tx, &message.room_id)?;
    let crowded = room.is_some_and(|room| room.member_bot_ids.len() > 1);
    let name = bots::get(tx, bot_id)?.map(|bot| bot.name);
    let mut body = match name {
        Some(name) if crowded => format!("【{name}】{text}"),
        _ => text,
    };
    if body.chars().count() > MAX_RELAY_CHARS {
        body = body.chars().take(MAX_RELAY_CHARS).collect();
        body.push_str("\n…（太长了，完整内容在房间里）");
    }
    for chat in targets {
        let key = format!("relay:{}:{chat}", message.id);
        external::enqueue(tx, Source::Telegram, &chat, &body, &key, now_ms())?;
    }
    Ok(())
}

fn new_code() -> String {
    // v4 的 UUID 来自系统的随机数源。每个字节取低 5 位，正好对上 32 个字符，没有偏差。
    let random = uuid::Uuid::new_v4();
    random.as_bytes()[..PAIRING_LEN]
        .iter()
        .map(|byte| PAIRING_ALPHABET[(byte & 31) as usize] as char)
        .collect()
}

fn hash_code(code: &str) -> String {
    hex::encode(Sha256::digest(code.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sa_store::Store;

    fn engine() -> (Engine, RoomId, BotId) {
        let engine = Engine::new(Store::open_in_memory().unwrap()).unwrap();
        engine.ensure_defaults().unwrap();
        let room = engine.list_rooms().unwrap().remove(0);
        let bot = room.host_bot_id.clone().unwrap();
        (engine, room.id, bot)
    }

    fn issue(key: &str, repo: &str) -> Incoming {
        Incoming {
            source: Source::Github,
            key: key.into(),
            name: "issues.opened".into(),
            external: repo.into(),
            label: format!("{repo}#12"),
            url: Some(format!("https://github.com/{repo}/issues/12")),
            kind: IncomingKind::Event {
                summary: "alice 开了 issue #12「登录报错」".into(),
                body: "点登录就 500".into(),
                pr: None,
                branch: None,
                task_only: false,
                notify: true,
            },
        }
    }

    fn said(key: &str, chat: &str, text: &str) -> Incoming {
        Incoming {
            source: Source::Telegram,
            key: key.into(),
            name: "message".into(),
            external: chat.into(),
            label: "张三".into(),
            url: None,
            kind: IncomingKind::Chat { text: text.into() },
        }
    }

    fn reply(engine: &Engine, room: &RoomId, bot: &BotId, text: &str) -> Message {
        let draft = engine.begin_bot_message(room, bot, 0).unwrap();
        engine.append_delta(&draft.id, text);
        engine
            .finish_message(&draft.id, MessageState::Complete, None)
            .unwrap()
    }

    #[test]
    fn an_event_from_a_linked_repo_lands_once_and_goes_to_the_host() {
        let (engine, room, bot) = engine();
        // 没连到房间的仓库：不收，但是记下了，重复送来不再看。
        let receipt = engine.receive(issue("d-0", "Me/App")).unwrap();
        assert_eq!(receipt, Receipt::Filtered("没有连到房间".into()));
        assert_eq!(
            engine.receive(issue("d-0", "Me/App")).unwrap(),
            Receipt::Duplicate
        );

        let link = engine.link_repo(&room, "Me/App").unwrap();
        assert_eq!(link.external, "me/app");
        assert_eq!(link.title, "Me/App");
        let Receipt::Stored(message) = engine.receive(issue("d-1", "me/APP")).unwrap() else {
            panic!("应该收下");
        };
        assert_eq!(message.kind, MessageKind::Event);
        assert_eq!(message.mentions, vec![bot]);
        assert_eq!(
            message.content,
            "alice 开了 issue #12「登录报错」\n\n点登录就 500"
        );
        let origin = message.origin.clone().unwrap();
        assert_eq!(origin.label, "me/APP#12");
        assert_eq!(origin.reply_to, None);

        assert_eq!(
            engine.receive(issue("d-1", "me/app")).unwrap(),
            Receipt::Duplicate
        );
        let page = engine.list_messages(&room, None, 50).unwrap();
        assert_eq!(page.messages, vec![*message]);
    }

    #[test]
    fn repos_are_checked_and_connect_to_one_room() {
        let (engine, room, _) = engine();
        for bad in ["", "app", "me/", "/app", "me/app/x", "me/a b", "../app"] {
            let error = engine.link_repo(&room, bad).unwrap_err();
            assert!(error.to_string().contains("owner/name"), "{bad}: {error}");
        }
        let link = engine.link_repo(&room, " me/app ").unwrap();
        let other = engine.create_room("另一个").unwrap();
        let error = engine.link_repo(&other.id, "ME/app").unwrap_err();
        assert!(error.to_string().contains("已经连到房间"), "{error}");
        assert_eq!(engine.list_links().unwrap(), vec![link.clone()]);

        engine.delete_link(&link.id).unwrap();
        assert!(engine.delete_link(&link.id).is_err());
        assert!(engine.link_repo(&other.id, "me/app").is_ok());
    }

    #[test]
    fn a_pairing_code_connects_a_chat_once() {
        let (engine, room, _) = engine();
        let pairing = engine.create_pairing(&room).unwrap();
        assert_eq!(pairing.code.len(), PAIRING_LEN);
        assert!(pairing.code.bytes().all(|b| PAIRING_ALPHABET.contains(&b)));

        assert_eq!(
            engine.redeem_pairing("AAAAAA0", "42", "张三").unwrap(),
            None
        );
        // 大小写、前后空格不计较。
        let typed = format!(" {} ", pairing.code.to_lowercase());
        let link = engine
            .redeem_pairing(&typed, "42", "张三")
            .unwrap()
            .unwrap();
        assert_eq!(
            (link.source, link.external.as_str()),
            (Source::Telegram, "42")
        );
        assert_eq!(link.room_id, room);
        assert_eq!(
            engine.redeem_pairing(&pairing.code, "43", "李四").unwrap(),
            None
        );

        // 同一个聊天再配到别的房间：改连过去，不会两头都连着。
        let other = engine.create_room("另一个").unwrap();
        let again = engine.create_pairing(&other.id).unwrap();
        let moved = engine
            .redeem_pairing(&again.code, "42", "张三")
            .unwrap()
            .unwrap();
        assert_eq!(engine.list_links().unwrap(), vec![moved]);
    }

    #[test]
    fn replies_go_back_to_the_telegram_chat_they_answer() {
        let (engine, room, bot) = engine();
        let pairing = engine.create_pairing(&room).unwrap();
        engine.redeem_pairing(&pairing.code, "42", "张三").unwrap();

        // 在界面里说的话：回复不往 Telegram 送。
        engine.post_user_message(&room, "你好").unwrap();
        reply(&engine, &room, &bot, "在。");
        assert!(engine
            .pending_outbound(Source::Telegram, 10)
            .unwrap()
            .is_empty());

        let Receipt::Stored(message) = engine.receive(said("u-1", "42", " 帮我看看 ")).unwrap()
        else {
            panic!("应该收下");
        };
        assert_eq!(message.kind, MessageKind::Chat);
        assert_eq!(message.content, "帮我看看");
        assert_eq!(message.origin.unwrap().reply_to.as_deref(), Some("42"));
        // 没写完、被停下、没话说的不送。
        let draft = engine.begin_bot_message(&room, &bot, 0).unwrap();
        engine
            .finish_message(&draft.id, MessageState::Cancelled, None)
            .unwrap();
        assert!(engine
            .pending_outbound(Source::Telegram, 10)
            .unwrap()
            .is_empty());

        let answer = reply(&engine, &room, &bot, "看过了，没问题。");
        let out = engine.pending_outbound(Source::Telegram, 10).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(
            (out[0].target.as_str(), out[0].body.as_str()),
            ("42", "看过了，没问题。")
        );
        engine.outbound_sent(out[0].id).unwrap();
        assert!(engine
            .pending_outbound(Source::Telegram, 10)
            .unwrap()
            .is_empty());
        assert_eq!(answer.state, MessageState::Complete);

        // 没答成也要让那边知道。
        engine.receive(said("u-2", "42", "再看看")).unwrap();
        let draft = engine.begin_bot_message(&room, &bot, 0).unwrap();
        engine
            .finish_message(&draft.id, MessageState::Failed, Some("模型超时".into()))
            .unwrap();
        let out = engine.pending_outbound(Source::Telegram, 10).unwrap();
        assert_eq!(out[0].body, "（没答成：模型超时）");

        // 没连接的聊天里说的话不收。
        assert_eq!(
            engine.receive(said("u-3", "99", "喂")).unwrap(),
            Receipt::Filtered("没有连到房间".into())
        );
    }

    #[test]
    fn what_events_bring_out_goes_to_every_linked_chat_with_the_speaker_named() {
        let (engine, room, bot) = engine();
        let second = engine
            .create_bot("Coder", sa_protocol::ModelTier::Mid, "")
            .unwrap();
        engine.add_room_member(&room, &second.id).unwrap();
        for chat in ["42", "-100"] {
            let pairing = engine.create_pairing(&room).unwrap();
            engine.redeem_pairing(&pairing.code, chat, chat).unwrap();
        }
        engine.link_repo(&room, "me/app").unwrap();
        engine.receive(issue("d-1", "me/app")).unwrap();
        reply(&engine, &room, &bot, "有个新 issue，我来看。");
        let out = engine.pending_outbound(Source::Telegram, 10).unwrap();
        let name = engine.get_bot(&bot).unwrap().name;
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].body, format!("【{name}】有个新 issue，我来看。"));
        let targets: Vec<&str> = out.iter().map(|o| o.target.as_str()).collect();
        assert_eq!(targets, ["42", "-100"]);
    }
}
