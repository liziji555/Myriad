//! Bot 模板：把几个 Bot（可以连同它们所在的房间）导出成一份不带 ID、时间和密钥的描述，在别处导入。
//!
//! 导入默认「只加不改」：已经有同名的 Bot 就跳过，不覆盖它；要照模板改它们得明说（`update_existing`）。
//! 模板有任何一处不对，什么都不建、什么都不改。

use crate::{Audit, BotPatch, Engine, EngineError, Result, MAX_ROOM_MEMBERS};
use sa_protocol::{Bot, BotId, BotTemplate, ImportReport, RoomId, RoomTemplate, TeamTemplate};

/// 现在认的模板格式版本。
pub const TEMPLATE_VERSION: u32 = 1;
/// 一份模板里最多这么多个 Bot。
pub const MAX_TEMPLATE_BOTS: usize = 50;

/// 模板文件最大这么多字节。
pub const MAX_TEMPLATE_BYTES: usize = 1024 * 1024;

/// 模板写成文件的样子（TOML）。
pub fn template_text(team: &TeamTemplate) -> String {
    let body = toml::to_string_pretty(team).expect("模板里只有字符串、数字和布尔值，总能写成 TOML");
    format!(
        "# SuperAgent 的 Bot 模板。在「成员」页导入，或者用 `sa bot import <这个文件>`。\n\n{body}"
    )
}

/// 读一份模板文件。认 TOML；以 `{` 开头的按 JSON 读。
pub fn parse_template(text: &str) -> Result<TeamTemplate> {
    if text.len() > MAX_TEMPLATE_BYTES {
        return Err(EngineError::Invalid("模板文件太大了（超过 1 MB）".into()));
    }
    let text = text.trim_start_matches('\u{feff}');
    let parsed = if text.trim_start().starts_with('{') {
        serde_json::from_str(text).map_err(|e| e.to_string())
    } else {
        toml::from_str(text).map_err(|e| e.message().to_string())
    };
    parsed.map_err(|why| EngineError::Invalid(format!("这不是一份能认的模板：{why}")))
}

/// 模板文件建议叫什么：带上房间的名字，去掉文件名里不能有的字符。
pub fn template_file_name(team: &TeamTemplate) -> String {
    let label = match &team.room {
        Some(room) => room.name.as_str(),
        None => "bots",
    };
    let safe: String = label
        .trim()
        .chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '.' => '-',
            c if c.is_whitespace() || c.is_control() => '-',
            c => c,
        })
        .take(40)
        .collect();
    format!("superagent-{safe}.toml")
}

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
        for (index, id) in ids.iter().enumerate() {
            // 同一个 Bot 写了两次只导出一次：重名的模板是导不进去的。
            if ids[..index].contains(id) {
                continue;
            }
            let bot = all.iter().find(|bot| &bot.id == id);
            let bot = bot.ok_or_else(|| EngineError::NotFound("没有这个 Bot".into()))?;
            bots.push(template(bot));
        }
        // 导出来的文件必须导得回去。
        if bots.len() > MAX_TEMPLATE_BOTS {
            return Err(EngineError::Invalid(format!(
                "一份模板里最多 {MAX_TEMPLATE_BOTS} 个 Bot，现在有 {} 个。请挑着导出，分成几份",
                bots.len()
            )));
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

    /// 导入一份模板。模板里带了房间就新建一个房间，把成员拉进去。
    ///
    /// 已经有同名的 Bot：默认跳过；`update_existing` 为真时照模板改它的档位、分工、是否启用
    /// （换一台机器时，全新数据目录里自带的「助手」就靠这个换成原来那个）。
    pub fn import_team(&self, team: &TeamTemplate, update_existing: bool) -> Result<ImportReport> {
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
        // 说了要改已有的：和模板不一样的才算改，一样的照旧算跳过。
        let changed: Vec<(&Bot, &BotTemplate)> = team
            .bots
            .iter()
            .filter(|_| update_existing)
            .filter_map(|bot| {
                let have = existing.iter().find(|have| same(&have.name, &bot.name))?;
                let differs = bot.tier != have.tier
                    || bot.persona.trim() != have.persona
                    || bot.enabled != have.enabled;
                differs.then_some((have, bot))
            })
            .collect();
        for (_, bot) in &changed {
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
                .filter(|bot| {
                    !changed
                        .iter()
                        .any(|(_, updated)| same(&updated.name, &bot.name))
                })
                .map(|bot| bot.name.trim().to_string())
                .collect(),
            updated: vec![],
            room: None,
        };
        for (have, bot) in changed {
            let patch = BotPatch {
                tier: Some(bot.tier),
                persona: Some(bot.persona.clone()),
                enabled: Some(bot.enabled),
                ..Default::default()
            };
            report.updated.push(self.update_bot(&have.id, patch)?);
        }
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
            // 新房间会自动带上最早的那个 Bot：先请出去，再按模板写的顺序把成员拉进来。
            for auto in room.member_bot_ids.clone() {
                room = self.remove_room_member(&room.id, &auto)?;
            }
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
            "导入了一份模板：新建 {} 个 Bot，跳过 {} 个已有的{}{}",
            report.created.len(),
            report.skipped.len(),
            if report.updated.is_empty() {
                String::new()
            } else {
                format!("，照模板改了 {} 个已有的", report.updated.len())
            },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AuditFilter;
    use sa_protocol::ModelTier;
    use sa_store::Store;

    fn engine() -> Engine {
        Engine::new(Store::open_in_memory().unwrap()).unwrap()
    }

    fn sorted<T: Ord>(mut items: Vec<T>) -> Vec<T> {
        items.sort();
        items
    }

    /// 成员的先后不算数（同一毫秒进房间的几个，先后是不定的），比较之前排好。
    fn in_order(mut team: TeamTemplate) -> TeamTemplate {
        team.bots.sort_by(|a, b| a.name.cmp(&b.name));
        if let Some(room) = &mut team.room {
            room.members.sort();
        }
        team
    }

    fn bot(name: &str, tier: ModelTier, persona: &str) -> BotTemplate {
        BotTemplate {
            name: name.into(),
            tier,
            persona: persona.into(),
            enabled: true,
        }
    }

    #[test]
    fn a_room_exported_here_comes_out_the_same_over_there() {
        let here = engine();
        let first = here
            .create_bot("Lead", ModelTier::Strong, "拆任务")
            .unwrap();
        let coder = here
            .create_bot("Coder", ModelTier::Mid, "写代码\n第二行")
            .unwrap();
        let review = here.create_bot("Reviewer", ModelTier::Cheap, "").unwrap();
        let patch = BotPatch {
            enabled: Some(false),
            ..Default::default()
        };
        here.update_bot(&review.id, patch).unwrap();
        // 房间自动带上了最早的 Lead；换成 Coder 和 Reviewer，入口是 Reviewer 之外的 Coder。
        let room = here.create_room("修 bug").unwrap();
        here.remove_room_member(&room.id, &first.id).unwrap();
        here.add_room_member(&room.id, &review.id).unwrap();
        here.add_room_member(&room.id, &coder.id).unwrap();
        here.update_room(&room.id, None, Some(&coder.id)).unwrap();

        let team = here.export_room(&room.id).unwrap();
        assert_eq!(team.version, TEMPLATE_VERSION);
        let wanted = team.room.clone().unwrap();
        assert_eq!(sorted(wanted.members.clone()), ["Coder", "Reviewer"]);
        assert_eq!(wanted.host.as_deref(), Some("Coder"));
        // 模板里没有 ID 和时间。
        let text = serde_json::to_string(&team).unwrap();
        assert!(!text.contains(coder.id.as_str()) && !text.contains("created_at"));

        // 那边已经有一个别的 Bot，新房间不能把它带进来。
        let there = engine();
        there.create_bot("Old", ModelTier::Mid, "原来就在").unwrap();
        let report = there.import_team(&team, false).unwrap();
        assert!(report.skipped.is_empty());
        let names: Vec<&str> = report.created.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(sorted(names), ["Coder", "Reviewer"]);
        let made = report.room.unwrap();
        assert_eq!(
            in_order(there.export_room(&made.id).unwrap()),
            in_order(team)
        );
        let all = there.list_bots().unwrap();
        let reviewer = all.iter().find(|b| b.name == "Reviewer").unwrap();
        assert!(!reviewer.enabled);
        assert_eq!(reviewer.tier, ModelTier::Cheap);
        let coder = all.iter().find(|b| b.name == "Coder").unwrap();
        assert_eq!(coder.persona, "写代码\n第二行");
        assert_eq!(made.host_bot_id.as_ref(), Some(&coder.id));

        let filter = AuditFilter {
            action: Some("team.".into()),
            ..Default::default()
        };
        let audit = there.list_audit(&filter, 10).unwrap().entries;
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].room_id.as_ref(), Some(&made.id));
        assert!(
            audit[0].summary.contains("新建 2 个 Bot"),
            "{}",
            audit[0].summary
        );
    }

    #[test]
    fn a_template_survives_being_written_to_a_file_and_read_back() {
        let persona = "写代码\n\"引号\"、# 井号、''' 三个单引号、\\ 反斜杠";
        let team = TeamTemplate {
            version: 1,
            bots: vec![
                bot("Coder", ModelTier::Strong, persona),
                BotTemplate {
                    enabled: false,
                    ..bot("审查员", ModelTier::Cheap, "")
                },
            ],
            room: Some(RoomTemplate {
                name: "修 bug / 第 2 轮".into(),
                host: Some("Coder".into()),
                members: vec!["Coder".into(), "审查员".into()],
            }),
        };
        let text = template_text(&team);
        assert!(text.starts_with("# SuperAgent"), "{text}");
        assert_eq!(parse_template(&text).unwrap(), team);
        // 带 BOM 的（Windows 记事本存的）、JSON 写的也认。
        assert_eq!(parse_template(&format!("\u{feff}{text}")).unwrap(), team);
        let json = serde_json::to_string_pretty(&team).unwrap();
        assert_eq!(parse_template(&json).unwrap(), team);
        // 手写的可以只写必须的：没写的人设是空的，默认启用，没有房间。
        let short = "version = 1\n[[bots]]\nname = \"A\"\ntier = \"mid\"\n";
        let parsed = parse_template(short).unwrap();
        assert_eq!(parsed.bots, [bot("A", ModelTier::Mid, "")]);
        assert_eq!(parsed.room, None);

        for bad in [
            "",
            "version = ",
            "[[bots]]\nname = \"A\"\ntier = \"mid\"",
            "{\"version\":1,",
            "version = 1\n[[bots]]\nname = \"A\"\ntier = \"huge\"",
        ] {
            let error = parse_template(bad).unwrap_err().to_string();
            assert!(error.contains("不是一份能认的模板"), "{bad}: {error}");
        }
        let huge = format!("# {}", "x".repeat(MAX_TEMPLATE_BYTES));
        let error = parse_template(&huge).unwrap_err().to_string();
        assert!(error.contains("太大"), "{error}");

        assert_eq!(
            template_file_name(&team),
            "superagent-修-bug---第-2-轮.toml"
        );
        let bots_only = TeamTemplate { room: None, ..team };
        assert_eq!(template_file_name(&bots_only), "superagent-bots.toml");
    }

    #[test]
    fn bots_that_already_exist_are_left_alone() {
        let engine = engine();
        let mine = engine
            .create_bot("coder", ModelTier::Strong, "我自己写的")
            .unwrap();
        let team = TeamTemplate {
            version: 1,
            bots: vec![
                bot("Coder", ModelTier::Cheap, "模板里的"),
                bot("Tester", ModelTier::Mid, "跑测试"),
            ],
            room: Some(RoomTemplate {
                name: "新房间".into(),
                host: None,
                members: vec!["Tester".into(), "Coder".into()],
            }),
        };
        let report = engine.import_team(&team, false).unwrap();
        assert_eq!(report.skipped, ["Coder"]);
        assert_eq!(report.created.len(), 1);
        let kept = engine.get_bot(&mine.id).unwrap();
        assert_eq!(
            (kept.tier, kept.persona.as_str()),
            (ModelTier::Strong, "我自己写的")
        );
        // 已有的同名 Bot 照样进房间；没写入口时第一个成员是入口。
        let room = report.room.unwrap();
        assert_eq!(room.member_bot_ids.len(), 2);
        assert!(room.member_bot_ids.contains(&mine.id));
        assert_eq!(room.host_bot_id.as_ref(), Some(&report.created[0].id));
        // 再导一次：Bot 都在了，只多一个房间。
        let again = engine.import_team(&team, false).unwrap();
        assert!(again.created.is_empty());
        assert_eq!(again.skipped, ["Coder", "Tester"]);
        assert_eq!(engine.list_bots().unwrap().len(), 2);

        // 只导出挑中的 Bot；不挑就是全部。
        let one = engine.export_bots(std::slice::from_ref(&kept.id)).unwrap();
        assert_eq!(one.bots, [bot("coder", ModelTier::Strong, "我自己写的")]);
        assert_eq!(one.room, None);
        assert_eq!(engine.export_bots(&[]).unwrap().bots.len(), 2);
        assert!(engine.export_bots(&[BotId::new()]).is_err());
        // 同一个 Bot 点了两次只导出一次。
        let twice = engine
            .export_bots(&[kept.id.clone(), kept.id.clone()])
            .unwrap();
        assert_eq!(twice.bots.len(), 1);
        // 多到导不回去的不导出：一份模板最多这么多个。
        for n in 0..MAX_TEMPLATE_BOTS - 1 {
            engine
                .create_bot(&format!("b{n}"), ModelTier::Mid, "")
                .unwrap();
        }
        let error = engine.export_bots(&[]).unwrap_err().to_string();
        assert!(error.contains("最多"), "{error}");
        let some: Vec<BotId> = engine
            .list_bots()
            .unwrap()
            .into_iter()
            .take(3)
            .map(|b| b.id)
            .collect();
        let part = engine.export_bots(&some).unwrap();
        engine.import_team(&part, false).unwrap();
    }

    #[test]
    fn existing_bots_are_only_changed_when_asked_to() {
        let team = TeamTemplate {
            version: 1,
            bots: vec![
                bot("助手", ModelTier::Strong, "总管：拆任务、分给别人"),
                bot("Coder", ModelTier::Mid, "写代码"),
            ],
            room: Some(RoomTemplate {
                name: "搬过来的房间".into(),
                host: Some("助手".into()),
                members: vec!["助手".into(), "Coder".into()],
            }),
        };
        // 全新的数据目录里自带一个「助手」。不说要改，它原样留着。
        let engine = engine();
        engine.ensure_defaults().unwrap();
        let stock = engine.list_bots().unwrap().remove(0);
        let report = engine.import_team(&team, false).unwrap();
        assert!(report.updated.is_empty());
        assert_eq!(report.skipped, ["助手"]);
        assert_eq!(engine.get_bot(&stock.id).unwrap(), stock);

        // 说了要改：照模板改它的档位和分工，名字和 ID 不变；一样的那个算跳过。
        let report = engine.import_team(&team, true).unwrap();
        assert!(report.created.is_empty());
        assert_eq!(report.skipped, ["Coder"]);
        assert_eq!(report.updated.len(), 1);
        let moved = engine.get_bot(&stock.id).unwrap();
        assert_eq!(moved.tier, ModelTier::Strong);
        assert_eq!(moved.persona, "总管：拆任务、分给别人");
        assert_eq!(report.room.unwrap().host_bot_id.as_ref(), Some(&stock.id));
        let filter = AuditFilter {
            action: Some("team.".into()),
            ..Default::default()
        };
        let audit = engine.list_audit(&filter, 1).unwrap().entries;
        assert!(
            audit[0].summary.contains("改了 1 个已有的"),
            "{}",
            audit[0].summary
        );
        // 再来一次：都和模板一样了，没有可改的。
        let again = engine.import_team(&team, true).unwrap();
        assert!(again.updated.is_empty());
        assert_eq!(again.skipped, ["助手", "Coder"]);

        // 要改的那个人设不合规矩：什么都不改、什么都不建。
        let bad = TeamTemplate {
            version: 1,
            bots: vec![
                bot("助手", ModelTier::Cheap, &"字".repeat(100_000)),
                bot("New", ModelTier::Mid, ""),
            ],
            room: None,
        };
        assert!(engine.import_team(&bad, true).is_err());
        assert_eq!(engine.get_bot(&stock.id).unwrap(), moved);
        assert_eq!(engine.list_bots().unwrap().len(), 2);
    }

    #[test]
    fn a_template_with_anything_wrong_changes_nothing() {
        let engine = engine();
        let good = || TeamTemplate {
            version: 1,
            bots: vec![bot("A", ModelTier::Mid, ""), bot("B", ModelTier::Mid, "")],
            room: Some(RoomTemplate {
                name: "房间".into(),
                host: Some("A".into()),
                members: vec!["A".into(), "B".into()],
            }),
        };
        type Spoil = Box<dyn Fn(&mut TeamTemplate)>;
        let cases: Vec<(&str, Spoil)> = vec![
            ("版本", Box::new(|t| t.version = 2)),
            ("都叫", Box::new(|t| t.bots[1].name = " a ".into())),
            (
                "空格",
                Box::new(|t| {
                    t.bots[1].name = "B C".into();
                    t.room = None;
                }),
            ),
            (
                "人设",
                Box::new(|t| t.bots[1].persona = "字".repeat(100_000)),
            ),
            (
                "不在模板里",
                Box::new(|t| t.room.as_mut().unwrap().members.push("C".into())),
            ),
            (
                "写了两次",
                Box::new(|t| t.room.as_mut().unwrap().members.push("a".into())),
            ),
            (
                "入口",
                Box::new(|t| t.room.as_mut().unwrap().host = Some("C".into())),
            ),
            (
                "房间名",
                Box::new(|t| t.room.as_mut().unwrap().name = "  ".into()),
            ),
            (
                "最多",
                Box::new(|t| {
                    t.bots = (0..=MAX_TEMPLATE_BOTS)
                        .map(|n| bot(&format!("b{n}"), ModelTier::Mid, ""))
                        .collect();
                    t.room = None;
                }),
            ),
            (
                "最多",
                Box::new(|t| {
                    t.bots = (0..7)
                        .map(|n| bot(&format!("b{n}"), ModelTier::Mid, ""))
                        .collect();
                    t.room.as_mut().unwrap().members = (0..7).map(|n| format!("b{n}")).collect();
                    t.room.as_mut().unwrap().host = None;
                }),
            ),
        ];
        for (expect, spoil) in cases {
            let mut team = good();
            spoil(&mut team);
            let error = engine.import_team(&team, false).unwrap_err().to_string();
            assert!(error.contains(expect), "{expect}: {error}");
            assert!(engine.list_bots().unwrap().is_empty(), "{expect}");
            assert!(engine.list_rooms().unwrap().is_empty(), "{expect}");
        }
        assert_eq!(engine.import_team(&good(), false).unwrap().created.len(), 2);
    }
}
