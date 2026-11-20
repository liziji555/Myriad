//! 审计日志：谁在什么时候做了一件要留底的事。
//!
//! 记的是「动了手」和「做了主」的事：审批、执行命令、推送、开 PR、调用外部服务、改 Bot 和房间、
//! 验收任务、看门狗和预算守卫停下了什么。普通的对话不记在这里，那在时间线上。
//! 记录只追加；记不上不影响正事（只打一条日志）。

use crate::{now_ms, Engine, Result};
use sa_protocol::{AuditActor, AuditEntry, AuditPage, RoomId, TaskId};
use sa_store::audit;

pub use sa_store::audit::Filter as AuditFilter;

/// 一句话最多这么长，细节最多这么长：命令的输出、文件内容不往这里放。
pub const MAX_AUDIT_SUMMARY_CHARS: usize = 300;
pub const MAX_AUDIT_DETAIL_CHARS: usize = 4_000;
/// 审计记录留多久。
pub const AUDIT_KEEP_DAYS: i64 = 180;
const MAX_AUDIT_PAGE: u32 = 200;

/// 要记的一条。用 [`Audit::user`] 这几个开头，再接着补房间、任务、细节。
#[derive(Debug, Clone)]
pub struct Audit {
    actor: AuditActor,
    actor_name: String,
    action: String,
    room_id: Option<RoomId>,
    task_id: Option<TaskId>,
    summary: String,
    detail: Option<String>,
}

impl Audit {
    fn new(actor: AuditActor, name: &str, action: &str, summary: impl Into<String>) -> Self {
        Self {
            actor,
            actor_name: name.to_string(),
            action: action.to_string(),
            room_id: None,
            task_id: None,
            summary: summary.into(),
            detail: None,
        }
    }

    /// 人做的。
    pub fn user(action: &str, summary: impl Into<String>) -> Self {
        Self::new(AuditActor::User, "", action, summary)
    }

    /// 名叫 `name` 的 Bot 做的。
    pub fn bot(name: &str, action: &str, summary: impl Into<String>) -> Self {
        Self::new(AuditActor::Bot, name, action, summary)
    }

    /// 程序自己做的。
    pub fn system(action: &str, summary: impl Into<String>) -> Self {
        Self::new(AuditActor::System, "", action, summary)
    }

    /// 外面来的，`source` 是「GitHub」「Telegram」。
    pub fn outside(source: &str, action: &str, summary: impl Into<String>) -> Self {
        Self::new(AuditActor::Outside, source, action, summary)
    }

    pub fn room(mut self, room_id: &RoomId) -> Self {
        self.room_id = Some(room_id.clone());
        self
    }

    pub fn task(mut self, task_id: Option<&TaskId>) -> Self {
        self.task_id = task_id.cloned();
        self
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        self.detail = (!detail.trim().is_empty()).then_some(detail);
        self
    }
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut clipped: String = text.chars().take(max).collect();
    clipped.push('…');
    clipped
}

impl Engine {
    /// 记一条审计。记不上只打日志：留底出了毛病，不能让正事跟着失败。
    pub fn audit(&self, entry: Audit) {
        let row = AuditEntry {
            id: 0,
            at: now_ms(),
            actor: entry.actor,
            actor_name: entry.actor_name,
            action: entry.action,
            room_id: entry.room_id,
            task_id: entry.task_id,
            summary: clip(&entry.summary, MAX_AUDIT_SUMMARY_CHARS),
            detail: entry
                .detail
                .map(|detail| clip(&detail, MAX_AUDIT_DETAIL_CHARS)),
        };
        if let Err(error) = self.store.write(|tx| audit::insert(tx, &row)) {
            tracing::warn!(action = %row.action, "审计记录没写上：{error}");
        }
    }

    /// 一页审计记录，新的在前。
    pub fn list_audit(&self, filter: &AuditFilter, limit: u32) -> Result<AuditPage> {
        let limit = limit.clamp(1, MAX_AUDIT_PAGE);
        // 多拿一条，用来知道更早的还有没有。
        let mut entries = self.store.read(|c| audit::list(c, filter, limit + 1))?;
        let has_more = entries.len() > limit as usize;
        entries.truncate(limit as usize);
        Ok(AuditPage { entries, has_more })
    }

    /// 启动时调用：清掉太旧的审计记录。返回清了几条。
    pub fn prune_audit(&self) -> Result<usize> {
        let before = now_ms() - AUDIT_KEEP_DAYS * 24 * 3_600_000;
        Ok(self.store.write(|tx| audit::prune(tx, before))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sa_store::Store;

    #[test]
    fn entries_are_clipped_listed_newest_first_and_paged() {
        let engine = Engine::new(Store::open_in_memory().unwrap()).unwrap();
        let room = RoomId::new();
        engine.audit(Audit::user("bot.created", "新建了 Bot「Coder」"));
        engine.audit(
            Audit::bot("Coder", "tool.shell", "x".repeat(1_000))
                .room(&room)
                .detail("cargo test"),
        );
        engine.audit(Audit::system("watchdog.stopped", "停下了任务").detail("  "));

        let page = engine.list_audit(&AuditFilter::default(), 2).unwrap();
        assert!(page.has_more);
        let actions: Vec<&str> = page.entries.iter().map(|e| e.action.as_str()).collect();
        assert_eq!(actions, ["watchdog.stopped", "tool.shell"]);
        // 空白的细节不记；太长的一句话截短。
        assert_eq!(page.entries[0].detail, None);
        assert_eq!(
            page.entries[1].summary.chars().count(),
            MAX_AUDIT_SUMMARY_CHARS + 1
        );
        assert_eq!(page.entries[1].actor_name, "Coder");
        assert_eq!(page.entries[1].detail.as_deref(), Some("cargo test"));

        let older = AuditFilter {
            before: Some(page.entries[1].id),
            ..Default::default()
        };
        let rest = engine.list_audit(&older, 2).unwrap();
        assert!(!rest.has_more);
        assert_eq!(rest.entries[0].action, "bot.created");

        let in_room = AuditFilter {
            room_id: Some(room),
            ..Default::default()
        };
        assert_eq!(engine.list_audit(&in_room, 50).unwrap().entries.len(), 1);
        // 刚记的不会被当成太旧的清掉。
        assert_eq!(engine.prune_audit().unwrap(), 0);
    }
}
