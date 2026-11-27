//! Bot 的增删改，房间的成员和入口 Bot。

use crate::{now_ms, to_json, Audit, Engine, EngineError, Result, MAX_ROOM_NAME_CHARS};
use sa_protocol::{
    Bot, BotId, BotKind, BotRef, EventKind, Harness, ModelTier, PermissionMode, Room, RoomId,
    UpdateRoomRequest, MAX_ROUNDS,
};
use sa_store::{bots, events, rooms};

pub const MAX_BOT_NAME_CHARS: usize = 24;
pub const MAX_BOT_ROLE_CHARS: usize = 24;
pub const MAX_PERSONA_CHARS: usize = 4_000;
pub const MAX_AVATAR_IMAGE_BYTES: usize = 256 * 1024;
/// 一个房间里最多几个 Bot。再多，谁该接话就说不清了。
pub const MAX_ROOM_MEMBERS: usize = 6;

/// 对一个 Bot 的修改：只改给出的字段。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BotPatch {
    pub name: Option<String>,
    pub tier: Option<ModelTier>,
    pub persona: Option<String>,
    pub enabled: Option<bool>,
    pub harness: Option<Harness>,
    pub harness_command: Option<String>,
    pub is_master: Option<bool>,
    pub role: Option<String>,
    pub avatar_shape: Option<String>,
    pub avatar_color: Option<String>,
    pub avatar_image: Option<String>,
}

/// 创建 Bot 时的可选形象与身份配置。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BotProfileOptions {
    pub is_master: Option<bool>,
    pub role: Option<String>,
    pub avatar_shape: Option<String>,
    pub avatar_color: Option<String>,
    pub avatar_image: Option<String>,
}

impl Engine {
    pub fn create_bot(&self, name: &str, tier: ModelTier, persona: &str) -> Result<Bot> {
        self.create_harness_bot(name, tier, persona, Harness::Builtin, "")
    }

    /// 新建一个 Bot，并说好由谁来干活。
    pub fn create_harness_bot(
        &self,
        name: &str,
        tier: ModelTier,
        persona: &str,
        harness: Harness,
        command: &str,
    ) -> Result<Bot> {
        self.create_bot_with_profile(
            name,
            tier,
            persona,
            harness,
            command,
            BotProfileOptions::default(),
        )
    }

    /// 新建一个带完整形象与主控身份选项的 Bot。
    pub fn create_bot_with_profile(
        &self,
        name: &str,
        tier: ModelTier,
        persona: &str,
        harness: Harness,
        command: &str,
        profile: BotProfileOptions,
    ) -> Result<Bot> {
        let command = check_harness(harness, command)?;
        if profile.is_master == Some(true) && harness != Harness::Builtin {
            return Err(EngineError::Invalid(
                "主控中枢 Bot 需要使用内置引擎（builtin），才能调用创建和管理团队工具".into(),
            ));
        }
        let role = check_role(profile.role.as_deref().unwrap_or(""))?;
        let avatar_shape = check_avatar_token(profile.avatar_shape.as_deref().unwrap_or(""))?;
        let avatar_color = check_avatar_token(profile.avatar_color.as_deref().unwrap_or(""))?;
        let avatar_image = check_avatar_image(profile.avatar_image.as_deref().unwrap_or(""))?;
        let now = now_ms();
        let (bot, seq) = self.store.write(|tx| {
            let existing = bots::list(tx)?;
            let (name, persona) = match (check_name(name, &existing, None), check_persona(persona))
            {
                (Ok(name), Ok(persona)) => (name, persona),
                (Err(e), _) | (_, Err(e)) => return Ok(Err(e)),
            };
            let is_master = profile.is_master.unwrap_or_else(|| {
                harness == Harness::Builtin && existing.iter().all(|b| !b.is_master)
            });
            let bot = Bot {
                id: BotId::new(),
                name,
                kind: BotKind::Assistant,
                tier,
                persona,
                enabled: true,
                created_at: now,
                harness,
                harness_command: command.clone(),
                is_master,
                role: role.clone(),
                avatar_shape: avatar_shape.clone(),
                avatar_color: avatar_color.clone(),
                avatar_image: avatar_image.clone(),
            };
            bots::insert(tx, &bot)?;
            let seq = events::append(tx, None, EventKind::BotCreated, &to_json(&bot), now)?;
            Ok(Ok((bot, seq)))
        })??;
        self.publish(seq);
        self.audit(
            Audit::user("bot.created", format!("新建了 Bot「{}」", bot.name)).detail(&bot.persona),
        );
        Ok(bot)
    }

    pub fn update_bot(&self, id: &BotId, patch: BotPatch) -> Result<Bot> {
        let now = now_ms();
        let (bot, seq) = self.store.write(|tx| {
            let existing = bots::list(tx)?;
            let Some(mut bot) = existing.iter().find(|bot| &bot.id == id).cloned() else {
                return Ok(Err(EngineError::NotFound("没有这个 Bot".into())));
            };
            if let Some(name) = &patch.name {
                match check_name(name, &existing, Some(id)) {
                    Ok(name) => bot.name = name,
                    Err(e) => return Ok(Err(e)),
                }
            }
            if let Some(persona) = &patch.persona {
                match check_persona(persona) {
                    Ok(persona) => bot.persona = persona,
                    Err(e) => return Ok(Err(e)),
                }
            }
            if let Some(role) = &patch.role {
                match check_role(role) {
                    Ok(role) => bot.role = role,
                    Err(e) => return Ok(Err(e)),
                }
            }
            if let Some(shape) = &patch.avatar_shape {
                match check_avatar_token(shape) {
                    Ok(shape) => bot.avatar_shape = shape,
                    Err(e) => return Ok(Err(e)),
                }
            }
            if let Some(color) = &patch.avatar_color {
                match check_avatar_token(color) {
                    Ok(color) => bot.avatar_color = color,
                    Err(e) => return Ok(Err(e)),
                }
            }
            if let Some(image) = &patch.avatar_image {
                match check_avatar_image(image) {
                    Ok(image) => bot.avatar_image = image,
                    Err(e) => return Ok(Err(e)),
                }
            }
            let original_harness = bot.harness;
            let original_master = bot.is_master;
            let harness = patch.harness.unwrap_or(bot.harness);
            let command = patch
                .harness_command
                .clone()
                .unwrap_or_else(|| bot.harness_command.clone());
            match check_harness(harness, &command) {
                Ok(command) => (bot.harness, bot.harness_command) = (harness, command),
                Err(e) => return Ok(Err(e)),
            }
            if let Some(is_master) = patch.is_master {
                if !is_master
                    && bot.is_master
                    && existing.len() > 1
                    && !existing.iter().any(|b| b.is_master && &b.id != id)
                {
                    return Ok(Err(EngineError::Invalid(
                        "团队至少需要保留一位主控 Bot，请先将另一位 Bot 设为主控".into(),
                    )));
                }
                bot.is_master = is_master;
            }
            let promoted_to_master = bot.is_master && !original_master;
            let switched_to_external =
                bot.is_master && bot.harness != Harness::Builtin && bot.harness != original_harness;
            if (promoted_to_master && bot.harness != Harness::Builtin) || switched_to_external {
                return Ok(Err(EngineError::Invalid(
                    "主控中枢 Bot 需要使用内置引擎（builtin），才能调用创建和管理团队工具；请先将另一位内置 Bot 设为主控再切换引擎".into(),
                )));
            }
            bot.tier = patch.tier.unwrap_or(bot.tier);
            bot.enabled = patch.enabled.unwrap_or(bot.enabled);
            bots::update(tx, &bot)?;
            let seq = events::append(tx, None, EventKind::BotUpdated, &to_json(&bot), now)?;
            Ok(Ok((bot, seq)))
        })??;
        self.publish(seq);
        let state = if bot.enabled { "启用" } else { "停用" };
        self.audit(
            Audit::user(
                "bot.updated",
                format!("改了 Bot「{}」（{state}）", bot.name),
            )
            .detail(&bot.persona),
        );
        Ok(bot)
    }

    /// 删掉一个 Bot。它发过的消息留着；它是入口的房间换成剩下的第一个成员（没有就空着）。
    /// 如果删掉的是最后一位主控且团队还有其它 Bot，自动把最早的剩余 Bot 接任主控。
    pub fn delete_bot(&self, id: &BotId) -> Result<()> {
        let now = now_ms();
        let seq = self.store.write(|tx| {
            let affected: Vec<Room> = rooms::list(tx)?
                .into_iter()
                .filter(|room| room.member_bot_ids.contains(id))
                .collect();
            let target = bots::get(tx, id)?;
            let was_master = target.as_ref().is_some_and(|bot| bot.is_master);
            let name = target.map(|bot| bot.name);
            if !bots::delete(tx, id)? {
                return Ok(Err(EngineError::NotFound("没有这个 Bot".into())));
            }
            let gone = BotRef { id: id.clone() };
            let mut seq = events::append(tx, None, EventKind::BotDeleted, &to_json(&gone), now)?;
            if was_master {
                let remaining = bots::list(tx)?;
                if !remaining.iter().any(|b| b.is_master) {
                    if let Some(mut successor) = remaining
                        .iter()
                        .find(|b| b.enabled && b.harness == Harness::Builtin)
                        .or_else(|| remaining.iter().find(|b| b.harness == Harness::Builtin))
                        .or_else(|| remaining.iter().find(|b| b.enabled))
                        .or_else(|| remaining.first())
                        .cloned()
                    {
                        successor.is_master = true;
                        bots::update(tx, &successor)?;
                        seq = events::append(
                            tx,
                            None,
                            EventKind::BotUpdated,
                            &to_json(&successor),
                            now,
                        )?;
                    }
                }
            }
            for mut room in affected {
                room.member_bot_ids.retain(|member| member != id);
                if room.host_bot_id.as_ref() == Some(id) {
                    room.host_bot_id = room.member_bot_ids.first().cloned();
                }
                room.updated_at = now;
                rooms::update(tx, &room)?;
                seq = events::append(
                    tx,
                    Some(&room.id),
                    EventKind::RoomUpdated,
                    &to_json(&room),
                    now,
                )?;
            }
            Ok(Ok((seq, name.unwrap_or_default())))
        })??;
        let (seq, name) = seq;
        self.publish(seq);
        self.audit(Audit::user("bot.deleted", format!("删掉了 Bot「{name}」")));
        Ok(())
    }

    /// 改房间的名字或入口 Bot。入口 Bot 必须已经是成员。
    pub fn update_room(
        &self,
        id: &RoomId,
        name: Option<&str>,
        host_bot_id: Option<&BotId>,
    ) -> Result<Room> {
        self.edit_room(
            id,
            &UpdateRoomRequest {
                name: name.map(str::to_string),
                host_bot_id: host_bot_id.cloned(),
                discussion: None,
                rounds: None,
                share: None,
                permission: None,
            },
        )
    }

    /// 改房间的设置：名字、入口 Bot（必须已经是成员）、没点名的话怎么接、轮流讨论转几圈、用哪个共享目录。没写的不变。
    pub fn edit_room(&self, id: &RoomId, edit: &UpdateRoomRequest) -> Result<Room> {
        let room = self.change_room(id, |room, _| {
            if let Some(name) = &edit.name {
                let name = name.trim();
                if name.is_empty() {
                    return Err(EngineError::Invalid("房间名不能为空".into()));
                }
                if name.chars().count() > MAX_ROOM_NAME_CHARS {
                    return Err(EngineError::Invalid(format!(
                        "房间名不能超过 {MAX_ROOM_NAME_CHARS} 个字"
                    )));
                }
                room.name = name.to_string();
            }
            if let Some(host) = &edit.host_bot_id {
                if !room.member_bot_ids.contains(host) {
                    return Err(EngineError::Invalid(
                        "入口 Bot 必须先是这个房间的成员".into(),
                    ));
                }
                room.host_bot_id = Some(host.clone());
            }
            if let Some(rounds) = edit.rounds {
                if !(1..=MAX_ROUNDS).contains(&rounds) {
                    return Err(EngineError::Invalid(format!(
                        "轮流讨论转 1 到 {MAX_ROUNDS} 圈"
                    )));
                }
                room.rounds = rounds;
            }
            if let Some(discussion) = edit.discussion {
                room.discussion = discussion;
            }
            if let Some(share) = &edit.share {
                let share = share.trim();
                if share.chars().count() > sa_protocol::MAX_SHARE_NAME_CHARS {
                    return Err(EngineError::Invalid(format!(
                        "共享目录的名字不能超过 {} 个字",
                        sa_protocol::MAX_SHARE_NAME_CHARS
                    )));
                }
                room.share = (!share.is_empty()).then(|| share.to_string());
            }
            if let Some(permission) = edit.permission {
                room.permission = permission;
            }
            Ok(RoomChange::Keep)
        })?;
        if let Some(permission) = edit.permission {
            let summary = match permission {
                PermissionMode::Ask => "改成了每次都问",
                PermissionMode::Full => "改成了完全权限：Bot 改文件、执行命令不再问",
                PermissionMode::Plan => "改成了规划模式：只读调研，方案批准后再执行",
            };
            self.audit(Audit::user("room.permission", summary).room(&room.id));
        }
        Ok(room)
    }

    /// 把一个 Bot 拉进房间。房间还没有入口 Bot 时，它就是入口。
    pub fn add_room_member(&self, id: &RoomId, bot_id: &BotId) -> Result<Room> {
        self.change_room(id, |room, all| {
            if !all.iter().any(|bot| &bot.id == bot_id) {
                return Err(EngineError::NotFound("没有这个 Bot".into()));
            }
            if room.member_bot_ids.contains(bot_id) {
                return Ok(RoomChange::Keep);
            }
            if room.member_bot_ids.len() >= MAX_ROOM_MEMBERS {
                return Err(EngineError::Invalid(format!(
                    "一个房间最多 {MAX_ROOM_MEMBERS} 个 Bot"
                )));
            }
            room.member_bot_ids.push(bot_id.clone());
            room.host_bot_id.get_or_insert_with(|| bot_id.clone());
            Ok(RoomChange::Add(bot_id.clone()))
        })
    }

    /// 把一个 Bot 请出房间。它是入口时，入口换成剩下的第一个成员。
    pub fn remove_room_member(&self, id: &RoomId, bot_id: &BotId) -> Result<Room> {
        self.change_room(id, |room, _| {
            if !room.member_bot_ids.contains(bot_id) {
                return Err(EngineError::NotFound("这个 Bot 不在房间里".into()));
            }
            room.member_bot_ids.retain(|member| member != bot_id);
            if room.host_bot_id.as_ref() == Some(bot_id) {
                room.host_bot_id = room.member_bot_ids.first().cloned();
            }
            Ok(RoomChange::Remove(bot_id.clone()))
        })
    }

    fn change_room(
        &self,
        id: &RoomId,
        change: impl FnOnce(&mut Room, &[Bot]) -> Result<RoomChange>,
    ) -> Result<Room> {
        let now = now_ms();
        let (room, seq) = self.store.write(|tx| {
            let Some(mut room) = rooms::get(tx, id)? else {
                return Ok(Err(EngineError::NotFound("没有这个房间".into())));
            };
            let all = bots::list(tx)?;
            let membership = match change(&mut room, &all) {
                Ok(membership) => membership,
                Err(e) => return Ok(Err(e)),
            };
            match membership {
                RoomChange::Keep => {}
                RoomChange::Add(bot_id) => rooms::add_member(tx, id, &bot_id, now)?,
                RoomChange::Remove(bot_id) => {
                    rooms::remove_member(tx, id, &bot_id)?;
                }
            }
            room.updated_at = now;
            rooms::update(tx, &room)?;
            let seq = events::append(tx, Some(id), EventKind::RoomUpdated, &to_json(&room), now)?;
            Ok(Ok((room, seq)))
        })??;
        self.publish(seq);
        let members = room.member_bot_ids.len();
        let summary = format!(
            "改了房间「{}」的设置或成员（现在 {members} 个 Bot）",
            room.name
        );
        self.audit(Audit::user("room.updated", summary).room(&room.id));
        Ok(room)
    }
}

enum RoomChange {
    Keep,
    Add(BotId),
    Remove(BotId),
}

/// 名字要能跟在 `@` 后面被认出来：不能有空白和 `@`，不能和别的 Bot 重名（不分大小写）。
fn check_name(name: &str, existing: &[Bot], except: Option<&BotId>) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(EngineError::Invalid("Bot 的名字不能为空".into()));
    }
    if name.chars().count() > MAX_BOT_NAME_CHARS {
        return Err(EngineError::Invalid(format!(
            "Bot 的名字不能超过 {MAX_BOT_NAME_CHARS} 个字"
        )));
    }
    if name
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '@')
    {
        return Err(EngineError::Invalid("Bot 的名字里不能有空格和 @".into()));
    }
    let taken = existing
        .iter()
        .any(|bot| Some(&bot.id) != except && bot.name.to_lowercase() == name.to_lowercase());
    if taken {
        return Err(EngineError::Invalid(format!(
            "已经有一个叫「{name}」的 Bot"
        )));
    }
    Ok(name.to_string())
}

/// 一个新 Bot 的名字和人设合不合规矩（不看有没有重名）。
pub(crate) fn check_bot(name: &str, persona: &str) -> Result<()> {
    check_name(name, &[], None)?;
    check_persona(persona).map(drop)
}

fn check_persona(persona: &str) -> Result<String> {
    let persona = persona.trim();
    if persona.chars().count() > MAX_PERSONA_CHARS {
        return Err(EngineError::Invalid(format!(
            "人设不能超过 {MAX_PERSONA_CHARS} 个字"
        )));
    }
    Ok(persona.to_string())
}

pub(crate) fn check_role(role: &str) -> Result<String> {
    let role = role.trim();
    if role.chars().count() > MAX_BOT_ROLE_CHARS {
        return Err(EngineError::Invalid(format!(
            "职责标签不能超过 {MAX_BOT_ROLE_CHARS} 个字"
        )));
    }
    if role.contains(['\n', '\r']) {
        return Err(EngineError::Invalid("职责标签要写在一行里".into()));
    }
    Ok(role.to_string())
}

pub(crate) fn check_avatar_token(token: &str) -> Result<String> {
    let token = token.trim().to_lowercase();
    if token.is_empty() {
        return Ok(String::new());
    }
    if token.len() > 32
        || !token
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'))
    {
        return Err(EngineError::Invalid("形象代号格式不对".into()));
    }
    Ok(token)
}

fn check_avatar_image(image: &str) -> Result<String> {
    let image = image.trim();
    if image.is_empty() {
        return Ok(String::new());
    }
    if image.len() > MAX_AVATAR_IMAGE_BYTES {
        return Err(EngineError::Invalid(
            "自定义头像图片过大（上限 256 KB）".into(),
        ));
    }
    let ok_prefix = image.starts_with("data:image/png;base64,")
        || image.starts_with("data:image/jpeg;base64,")
        || image.starts_with("data:image/webp;base64,");
    if !ok_prefix {
        return Err(EngineError::Invalid(
            "自定义头像仅支持 PNG / JPEG / WebP 图片".into(),
        ));
    }
    Ok(image.to_string())
}

/// 找出文字里 `@名字` 点到的 Bot，按出现的先后，不重复。
///
/// 名字不分大小写；几个名字都对得上时取最长的（有「审查」和「审查员」时，`@审查员` 是后者）。
/// 中文里名字后面常常直接跟着别的字，所以不要求名字后面有空格。
pub fn find_mentions(text: &str, candidates: &[Bot]) -> Vec<BotId> {
    let mut found: Vec<BotId> = vec![];
    for (at, _) in text.match_indices('@') {
        let rest = text[at + 1..].to_lowercase();
        let hit = candidates
            .iter()
            .filter(|bot| rest.starts_with(&bot.name.to_lowercase()))
            .max_by_key(|bot| bot.name.chars().count());
        if let Some(bot) = hit {
            if !found.contains(&bot.id) {
                found.push(bot.id.clone());
            }
        }
    }
    found
}

/// harness 的命令：内置的没有命令；自定义命令必须写；最长 300 个字。
pub(crate) fn check_harness(harness: Harness, command: &str) -> Result<String> {
    let command = command.trim();
    if harness == Harness::Builtin {
        return Ok(String::new());
    }
    if harness == Harness::Command && command.is_empty() {
        return Err(EngineError::Invalid("自定义命令要写上跑哪个程序".into()));
    }
    if harness == Harness::Acp && command.is_empty() {
        return Err(EngineError::Invalid(
            "ACP 通用 Agent 要写上启动命令，例如 cursor-agent acp 或 gemini --acp".into(),
        ));
    }
    if command.chars().count() > 300 || command.contains(['\n', '\r']) {
        return Err(EngineError::Invalid(
            "harness 命令要写在一行里，最多 300 个字".into(),
        ));
    }
    Ok(command.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sa_protocol::{Event, MessageState, Sender};
    use sa_store::Store;

    fn engine() -> Engine {
        let engine = Engine::new(Store::open_in_memory().unwrap()).unwrap();
        engine.ensure_defaults().unwrap();
        engine
    }

    fn kinds(events: &[Event]) -> Vec<&'static str> {
        events.iter().map(|e| e.kind.as_str()).collect()
    }

    #[test]
    fn bots_are_created_validated_updated_and_deleted() {
        let engine = engine();
        let head = engine.event_head();
        let coder = engine
            .create_bot(" Coder ", ModelTier::Strong, " 你负责写代码。 ")
            .unwrap();
        assert_eq!(
            (coder.name.as_str(), coder.persona.as_str()),
            ("Coder", "你负责写代码。")
        );
        assert_eq!(coder.tier, ModelTier::Strong);
        assert!(coder.enabled);

        for bad in [
            "",
            "  ",
            "有 空格",
            "a@b",
            &"长".repeat(MAX_BOT_NAME_CHARS + 1),
            "coder",
            "助手",
        ] {
            let error = engine.create_bot(bad, ModelTier::Mid, "").unwrap_err();
            assert!(matches!(error, EngineError::Invalid(_)), "{bad}: {error}");
        }
        let too_long = "字".repeat(MAX_PERSONA_CHARS + 1);
        assert!(engine
            .create_bot("Writer", ModelTier::Mid, &too_long)
            .is_err());

        // 改名可以保留自己原来的名字（只改大小写），不能撞别人的。
        let patch = |name: &str| BotPatch {
            name: Some(name.into()),
            ..Default::default()
        };
        assert_eq!(
            engine.update_bot(&coder.id, patch("CODER")).unwrap().name,
            "CODER"
        );
        assert!(engine.update_bot(&coder.id, patch("助手")).is_err());
        let updated = engine
            .update_bot(
                &coder.id,
                BotPatch {
                    tier: Some(ModelTier::Cheap),
                    persona: Some("新的分工".into()),
                    enabled: Some(false),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(updated.name, "CODER");
        assert_eq!(updated.tier, ModelTier::Cheap);
        assert_eq!(updated.persona, "新的分工");
        assert!(!updated.enabled);
        assert_eq!(engine.get_bot(&coder.id).unwrap(), updated);
        let missing = engine.update_bot(&BotId::new(), BotPatch::default());
        assert!(matches!(missing, Err(EngineError::NotFound(_))));

        engine.delete_bot(&coder.id).unwrap();
        assert!(matches!(
            engine.get_bot(&coder.id),
            Err(EngineError::NotFound(_))
        ));
        assert!(matches!(
            engine.delete_bot(&coder.id),
            Err(EngineError::NotFound(_))
        ));
        let events = engine.events_after(head, 100).unwrap();
        assert_eq!(
            kinds(&events),
            ["bot.created", "bot.updated", "bot.updated", "bot.deleted"]
        );
    }

    #[test]
    fn room_members_and_host_follow_the_rules() {
        let engine = engine();
        let room = engine.list_rooms().unwrap().remove(0);
        let host = room.host_bot_id.clone().unwrap();
        let coder = engine.create_bot("Coder", ModelTier::Mid, "").unwrap();
        let reviewer = engine.create_bot("Reviewer", ModelTier::Mid, "").unwrap();

        // 不是成员的不能当入口。
        assert!(engine.update_room(&room.id, None, Some(&coder.id)).is_err());
        let joined = engine.add_room_member(&room.id, &coder.id).unwrap();
        assert_eq!(joined.member_bot_ids, [host.clone(), coder.id.clone()]);
        assert_eq!(joined.host_bot_id, Some(host.clone()));
        // 重复拉进来没有变化。
        assert_eq!(
            engine
                .add_room_member(&room.id, &coder.id)
                .unwrap()
                .member_bot_ids
                .len(),
            2
        );
        assert!(matches!(
            engine.add_room_member(&room.id, &BotId::new()),
            Err(EngineError::NotFound(_))
        ));

        let renamed = engine
            .update_room(&room.id, Some(" 新名字 "), Some(&coder.id))
            .unwrap();
        assert_eq!(renamed.name, "新名字");
        assert_eq!(renamed.host_bot_id, Some(coder.id.clone()));
        assert!(engine.update_room(&room.id, Some("  "), None).is_err());

        // 入口被请出去：换成剩下的第一个；全走了就没有入口。
        let left = engine.remove_room_member(&room.id, &coder.id).unwrap();
        assert_eq!(left.host_bot_id, Some(host.clone()));
        assert!(engine.remove_room_member(&room.id, &coder.id).is_err());
        let empty = engine.remove_room_member(&room.id, &host).unwrap();
        assert_eq!((empty.host_bot_id, empty.member_bot_ids.len()), (None, 0));
        // 空房间里第一个进来的就是入口。
        let again = engine.add_room_member(&room.id, &reviewer.id).unwrap();
        assert_eq!(again.host_bot_id, Some(reviewer.id.clone()));
        assert_eq!(engine.get_room(&room.id).unwrap(), again);

        // 成员上限。
        for index in 0..MAX_ROOM_MEMBERS - 1 {
            let bot = engine
                .create_bot(&format!("B{index}"), ModelTier::Mid, "")
                .unwrap();
            engine.add_room_member(&room.id, &bot.id).unwrap();
        }
        let extra = engine.create_bot("多的", ModelTier::Mid, "").unwrap();
        let error = engine.add_room_member(&room.id, &extra.id).unwrap_err();
        assert!(error.to_string().contains("最多"), "{error}");

        // 删掉入口 Bot：房间还在，入口换人，成员里没有它了。
        engine.delete_bot(&reviewer.id).unwrap();
        let after = engine.get_room(&room.id).unwrap();
        assert!(!after.member_bot_ids.contains(&reviewer.id));
        assert_eq!(after.host_bot_id, after.member_bot_ids.first().cloned());
    }

    #[test]
    fn mentions_are_found_by_name() {
        let bot = |name: &str| Bot {
            id: BotId::from(format!("bot_{name}")),
            name: name.into(),
            kind: BotKind::Assistant,
            tier: ModelTier::Mid,
            persona: String::new(),
            enabled: true,
            created_at: 0,
            harness: Default::default(),
            harness_command: String::new(),
            is_master: false,
            role: String::new(),
            avatar_shape: String::new(),
            avatar_color: String::new(),
            avatar_image: String::new(),
        };
        let team = [bot("Coder"), bot("审查"), bot("审查员")];
        let ids = |text: &str| -> Vec<String> {
            find_mentions(text, &team)
                .iter()
                .map(|id| id.as_str().to_string())
                .collect()
        };
        assert_eq!(ids("@Coder 看一下"), ["bot_Coder"]);
        assert_eq!(ids("@coder帮我看看，@CODER 快点"), ["bot_Coder"]);
        assert_eq!(
            ids("请 @审查员 和 @审查 都看看，还有 @Coder"),
            ["bot_审查员", "bot_审查", "bot_Coder"]
        );
        assert!(ids("没有点名，邮箱 a@b.com，@不存在").is_empty());
        assert!(ids("Coder 你好").is_empty());
    }

    #[test]
    fn user_messages_record_who_was_mentioned_and_bot_messages_carry_hops() {
        let engine = engine();
        let room = engine.list_rooms().unwrap().remove(0);
        let host = room.host_bot_id.clone().unwrap();
        let coder = engine.create_bot("Coder", ModelTier::Mid, "").unwrap();
        let outsider = engine.create_bot("Outsider", ModelTier::Mid, "").unwrap();
        engine.add_room_member(&room.id, &coder.id).unwrap();

        let message = engine
            .post_user_message(&room.id, "@Coder 和 @Outsider 还有 @助手")
            .unwrap();
        // 不在房间里的不算点名。
        assert_eq!(message.mentions, [coder.id.clone(), host.clone()]);
        assert_eq!(message.hop, 0);
        // 停用的也不算。
        engine
            .update_bot(
                &coder.id,
                BotPatch {
                    enabled: Some(false),
                    ..Default::default()
                },
            )
            .unwrap();
        let message = engine.post_user_message(&room.id, "@Coder 在吗").unwrap();
        assert!(message.mentions.is_empty());
        let _ = outsider;

        let ask = engine
            .post_bot_message(
                &room.id,
                &host,
                "@Coder 帮我看看",
                vec![coder.id.clone()],
                1,
                vec![],
            )
            .unwrap();
        assert_eq!(ask.state, MessageState::Complete);
        assert_eq!((ask.mentions.clone(), ask.hop), (vec![coder.id.clone()], 1));
        let draft = engine.begin_bot_message(&room.id, &coder.id, 1).unwrap();
        assert_eq!(draft.hop, 1);

        // 读回来的和写进去的一样；撤掉的草稿不见了。
        let all = engine.messages_after(&room.id, 0).unwrap();
        assert_eq!(all.len(), 4);
        assert_eq!(all[2], ask);
        assert!(matches!(all[3].sender, Sender::Bot { .. }));
        let head = engine.event_head();
        engine.discard_message(&draft.id).unwrap();
        assert_eq!(engine.messages_after(&room.id, 0).unwrap().len(), 3);
        assert!(engine.discard_message(&draft.id).is_err());
        let events = engine.events_after(head, 10).unwrap();
        assert_eq!(kinds(&events), ["message.deleted"]);
        assert_eq!(events[0].payload["id"], draft.id.as_str());
    }

    #[test]
    fn master_role_avatar_validation_and_successor_promotion() {
        let engine = engine();
        let bots = engine.list_bots().unwrap();
        assert_eq!(bots.len(), 1);
        let master = &bots[0];
        assert!(master.is_master);
        assert_eq!(master.role, "主控中枢");
        assert_eq!(master.avatar_shape, "orb");
        assert_eq!(master.avatar_color, "obsidian");

        let child = engine
            .create_bot_with_profile(
                "前端专家",
                ModelTier::Strong,
                "写 React",
                Harness::Builtin,
                "",
                BotProfileOptions {
                    is_master: Some(false),
                    role: Some("前端工程师".into()),
                    avatar_shape: Some("cat".into()),
                    avatar_color: Some("aurora".into()),
                    avatar_image: None,
                },
            )
            .unwrap();
        assert!(!child.is_master);
        assert_eq!(child.role, "前端工程师");
        assert_eq!(child.avatar_shape, "cat");
        assert_eq!(child.avatar_color, "aurora");

        assert!(engine
            .update_bot(
                &child.id,
                BotPatch {
                    avatar_image: Some("javascript:alert(1)".into()),
                    ..Default::default()
                },
            )
            .is_err());

        assert!(engine
            .update_bot(
                &master.id,
                BotPatch {
                    is_master: Some(false),
                    ..Default::default()
                },
            )
            .is_err());

        engine.delete_bot(&master.id).unwrap();
        let after = engine.get_bot(&child.id).unwrap();
        assert!(after.is_master);
    }
}
