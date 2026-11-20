//! Bot 模板：把几个 Bot（可以连同它们所在的房间）导出成一份不带 ID、时间和密钥的描述，在别处导入。
//!
//! 导入是「只加不改」：已经有同名的 Bot 就跳过，不覆盖它；模板有任何一处不对，什么都不建。

use crate::{Audit, BotPatch, Engine, EngineError, Result, MAX_ROOM_MEMBERS};
use sa_protocol::{Bot, BotId, BotTemplate, ImportReport, RoomId, RoomTemplate, TeamTemplate};

/// 现在认的模板格式版本。
pub const TEMPLATE_VERSION: u32 = 1;
/// 一份模板里最多这么多个 Bot。
pub const MAX_TEMPLATE_BOTS: usize = 50;

fn template(bot: &Bot) -> BotTemplate {
    BotTemplate {
        name: bot.name.clone(),
        tier: bot.tier,
        persona: bot.persona.clone(),
        enabled: bot.enabled,
    }
}

fn same(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

impl Engine {
    /// 导出几个 Bot。`ids` 是空的就导出全部。
    pub fn export_bots(&self, ids: &[BotId]) -> Result<TeamTemplate> {
        let all = self.list_bots()?;
        let mut bots = vec![];
        if ids.is_empty() {
            bots.extend(all.iter().map(template));
        }
        for id in ids {
            let bot = all.iter().find(|bot| &bot.id == id);
            let bot = bot.ok_or_else(|| EngineError::NotFound("没有这个 Bot".into()))?;
            bots.push(template(bot));
        }
        Ok(TeamTemplate {
            version: TEMPLATE_VERSION,
            bots,
            room: None,
        })
    }

    /// 导出一个房间的团队：成员、谁是入口、房间叫什么。
    pub fn export_room(&self, id: &RoomId) -> Result<TeamTemplate> {
        let room = self.get_room(id)?;
        let all = self.list_bots()?;
        let members: Vec<&Bot> = room
            .member_bot_ids
            .iter()
            .filter_map(|id| all.iter().find(|bot| &bot.id == id))
            .collect();
        let host = members
            .iter()
            .find(|bot| Some(&bot.id) == room.host_bot_id.as_ref())
            .map(|bot| bot.name.clone());
        Ok(TeamTemplate {
            version: TEMPLATE_VERSION,
            bots: members.iter().map(|bot| template(bot)).collect(),
            room: Some(RoomTemplate {
                name: room.name,
                host,
                members: members.iter().map(|bot| bot.name.clone()).collect(),
            }),
        })
    }

    /// 导入一份模板。已经有同名的 Bot 跳过；模板里带了房间就新建一个房间，把成员拉进去。
    pub fn import_team(&self, team: &TeamTemplate) -> Result<ImportReport> {
        let invalid = |text: String| Err(EngineError::Invalid(text));
        if team.version != TEMPLATE_VERSION {
            return invalid(format!(
                "模板的版本是 {}，这个程序只认版本 {TEMPLATE_VERSION}",
                team.version
            ));
        }
        if team.bots.len() > MAX_TEMPLATE_BOTS {
            return invalid(format!("一份模板里最多 {MAX_TEMPLATE_BOTS} 个 Bot"));
        }
        for (index, bot) in team.bots.iter().enumerate() {
            if team.bots[..index]
                .iter()
                .any(|other| same(&other.name, &bot.name))
            {
                return invalid(format!("模板里有两个 Bot 都叫「{}」", bot.name.trim()));
            }
        }
        let existing = self.list_bots()?;
        let known = |name: &str| {
            team.bots.iter().any(|bot| same(&bot.name, name))
                || existing.iter().any(|bot| same(&bot.name, name))
        };
        if let Some(room) = &team.room {
            if room.members.len() > MAX_ROOM_MEMBERS {
                return invalid(format!("一个房间最多 {MAX_ROOM_MEMBERS} 个 Bot"));
            }
            for (index, member) in room.members.iter().enumerate() {
                if !known(member) {
                    return invalid(format!(
                        "房间的成员「{member}」不在模板里，也不是已有的 Bot"
                    ));
                }
                if room.members[..index]
                    .iter()
                    .any(|other| same(other, member))
                {
                    return invalid(format!("房间的成员里「{member}」写了两次"));
                }
            }
            if let Some(host) = &room.host {
                if !room.members.iter().any(|member| same(member, host)) {
                    return invalid(format!("入口 Bot「{host}」不在房间的成员里"));
                }
            }
        }
        // 新的 Bot 先全部检查一遍再动手：名字、人设有一个不合规矩的，一个都不建。
        let fresh: Vec<&BotTemplate> = team
            .bots
            .iter()
            .filter(|bot| !existing.iter().any(|have| same(&have.name, &bot.name)))
            .collect();
        for bot in &fresh {
            crate::team::check_bot(&bot.name, &bot.persona)?;
        }
        if let Some(room) = &team.room {
            crate::check_room_name(&room.name)?;
        }

        let mut report = ImportReport {
            created: vec![],
            skipped: team
                .bots
                .iter()
                .filter(|bot| existing.iter().any(|have| same(&have.name, &bot.name)))
                .map(|bot| bot.name.trim().to_string())
                .collect(),
            room: None,
        };
        for bot in fresh {
            let mut created = self.create_bot(&bot.name, bot.tier, &bot.persona)?;
            if !bot.enabled {
                let patch = BotPatch {
                    enabled: Some(false),
                    ..Default::default()
                };
                created = self.update_bot(&created.id, patch)?;
            }
            report.created.push(created);
        }
        if let Some(wanted) = &team.room {
            let all = self.list_bots()?;
            let id_of = |name: &str| {
                all.iter()
                    .find(|bot| same(&bot.name, name))
                    .map(|bot| bot.id.clone())
            };
            let mut room = self.create_room(&wanted.name)?;
            for member in &wanted.members {
                if let Some(bot_id) = id_of(member) {
                    room = self.add_room_member(&room.id, &bot_id)?;
                }
            }
            if let Some(host) = wanted.host.as_deref().and_then(id_of) {
                room = self.update_room(&room.id, None, Some(&host))?;
            }
            report.room = Some(room);
        }
        let summary = format!(
            "导入了一份模板：新建 {} 个 Bot，跳过 {} 个已有的{}",
            report.created.len(),
            report.skipped.len(),
            if report.room.is_some() {
                "，新建了一个房间"
            } else {
                ""
            }
        );
        let mut entry = Audit::user("team.imported", summary);
        if let Some(room) = &report.room {
            entry = entry.room(&room.id);
        }
        self.audit(entry);
        Ok(report)
    }
}
