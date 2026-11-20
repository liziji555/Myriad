//! 花费和提醒：记了多少用量、花了多少钱、有哪些事要告诉人。

use crate::{now_ms, to_json, Engine, EngineError, Result};
use sa_protocol::{
    EventKind, Millis, Notice, NoticeId, NoticeKind, NoticeLevel, RoomId, Spend, SpendRow, TaskId,
};
use sa_store::usage::{self, GroupBy, Sum};
use sa_store::{bots, notices, tasks};

/// 一次最多给出多少条还没收起的提醒。
const MAX_OPEN_NOTICES: u32 = 50;
/// 「今天花得最多的任务」列几个。
const TOP_TASKS: usize = 10;

/// 要记的一条提醒。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewNotice {
    pub kind: NoticeKind,
    pub level: NoticeLevel,
    /// 关于哪个房间。`None` 表示和所有房间都有关。
    pub room_id: Option<RoomId>,
    pub task_id: Option<TaskId>,
    pub text: String,
}

/// 一个任务花了多少，单位是百万分之一个货币单位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskSpend {
    /// 从开始到现在一共花的。
    pub total: i64,
    /// 人上一次让它继续时已经花的。
    pub floor: i64,
}

impl TaskSpend {
    /// 人上一次让它继续之后花的。任务的花费上限管的是这个数。
    pub fn since_resumed(self) -> i64 {
        (self.total - self.floor).max(0)
    }
}

/// 「用量」面板里和配置无关的那部分。
#[derive(Debug, Clone, PartialEq)]
pub struct UsageBreakdown {
    pub today: Spend,
    pub days: Vec<SpendRow>,
    pub models: Vec<SpendRow>,
    pub bots: Vec<SpendRow>,
    pub tasks: Vec<SpendRow>,
}

impl Engine {
    /// `since` 之后一共花了多少（百万分之一个货币单位）。
    pub fn spent_since(&self, since: Millis) -> Result<i64> {
        Ok(self.store.read(|c| usage::cost_since(c, since))?)
    }

    pub fn task_spend(&self, id: &TaskId) -> Result<TaskSpend> {
        Ok(self.store.read(|c| {
            Ok(TaskSpend {
                total: usage::task_cost(c, id)?,
                floor: tasks::spend_floor(c, id)?,
            })
        })?)
    }

    /// 每个调用过模型的任务一共花了多少。
    pub fn task_costs(&self) -> Result<Vec<(TaskId, i64)>> {
        Ok(self.store.read(usage::task_costs)?)
    }

    /// 记一条提醒并广播。`key` 相同的提醒只记一次：已经记过时返回 `None`。
    pub fn notify(&self, key: &str, new: NewNotice) -> Result<Option<Notice>> {
        let notice = Notice {
            id: NoticeId::new(),
            kind: new.kind,
            level: new.level,
            room_id: new.room_id,
            task_id: new.task_id,
            text: new.text,
            created_at: now_ms(),
            dismissed_at: None,
        };
        let seq = self.store.write(|tx| {
            if !notices::insert(tx, key, &notice)? {
                return Ok(None);
            }
            Ok(Some(sa_store::events::append(
                tx,
                notice.room_id.as_ref(),
                EventKind::NoticeCreated,
                &to_json(&notice),
                notice.created_at,
            )?))
        })?;
        let Some(seq) = seq else {
            return Ok(None);
        };
        self.publish(seq);
        // 看门狗和预算守卫发现的事（停下了什么、到了什么上限）也留底。
        let mut entry =
            crate::Audit::system(&format!("notice.{}", notice.kind.as_str()), &notice.text)
                .task(notice.task_id.as_ref());
        if let Some(room_id) = &notice.room_id {
            entry = entry.room(room_id);
        }
        self.audit(entry);
        Ok(Some(notice))
    }

    /// 还没收起来的提醒，新的在前。
    pub fn list_notices(&self) -> Result<Vec<Notice>> {
        Ok(self
            .store
            .read(|c| notices::list_open(c, MAX_OPEN_NOTICES))?)
    }

    /// 人把一条提醒收起来。已经收起的原样返回。
    pub fn dismiss_notice(&self, id: &NoticeId) -> Result<Notice> {
        let now = now_ms();
        let (notice, seq) = self
            .store
            .write(|tx| {
                let Some(mut notice) = notices::get(tx, id)? else {
                    return Ok(None);
                };
                if !notices::dismiss(tx, id, now)? {
                    return Ok(Some((notice, None)));
                }
                notice.dismissed_at = Some(now);
                let seq = sa_store::events::append(
                    tx,
                    notice.room_id.as_ref(),
                    EventKind::NoticeUpdated,
                    &to_json(&notice),
                    now,
                )?;
                Ok(Some((notice, Some(seq))))
            })?
            .ok_or_else(|| EngineError::NotFound("没有这条提醒".into()))?;
        if let Some(seq) = seq {
            self.publish(seq);
        }
        Ok(notice)
    }

    /// 按天、按模型、按 Bot、按任务的用量。
    ///
    /// `days` 是每一天的 `(日期, 开始, 结束)`，从早到晚，最后一个是今天；
    /// 哪里算一天的开始由调用的人定（这一层不知道时区）。
    pub fn usage_breakdown(&self, days: &[(String, Millis, Millis)]) -> Result<UsageBreakdown> {
        let Some((_, from, to)) = days.last().cloned() else {
            return Err(EngineError::Invalid("至少要有今天这一天".into()));
        };
        Ok(self.store.read(|c| {
            let mut rows = Vec::with_capacity(days.len());
            for (date, from, to) in days {
                rows.push(SpendRow {
                    key: date.clone(),
                    label: date.clone(),
                    spend: spend(usage::sum_between(c, *from, *to)?),
                });
            }
            let today = rows.last().map(|row| row.spend.clone()).unwrap_or_default();

            let models = usage::grouped(c, GroupBy::Model, from, to)?
                .into_iter()
                .map(|(model, sum)| SpendRow {
                    label: model.clone(),
                    key: model,
                    spend: spend(sum),
                })
                .collect();

            let mut by_bot = Vec::new();
            for (id, sum) in usage::grouped(c, GroupBy::Bot, from, to)? {
                let label = if id.is_empty() {
                    "挑选由谁接话".to_string()
                } else {
                    bots::get(c, &id.clone().into())?
                        .map(|bot| bot.name)
                        .unwrap_or_else(|| "已删除的 Bot".into())
                };
                by_bot.push(SpendRow {
                    key: id,
                    label,
                    spend: spend(sum),
                });
            }

            let mut by_task = Vec::new();
            for (id, sum) in usage::grouped(c, GroupBy::Task, from, to)?
                .into_iter()
                .take(TOP_TASKS)
            {
                let label = tasks::get(c, &id.clone().into())?
                    .map(|task| task.title)
                    .unwrap_or_else(|| "已删除的任务".into());
                by_task.push(SpendRow {
                    key: id,
                    label,
                    spend: spend(sum),
                });
            }

            Ok(UsageBreakdown {
                today,
                days: rows,
                models,
                bots: by_bot,
                tasks: by_task,
            })
        })?)
    }
}

/// 百万分之一个货币单位换成货币单位。
pub fn micros_to_cost(micros: i64) -> f64 {
    micros as f64 / 1_000_000.0
}

fn spend(sum: Sum) -> Spend {
    Spend {
        cost: micros_to_cost(sum.cost_micros),
        input_tokens: sum.in_tokens,
        output_tokens: sum.out_tokens,
        cached_tokens: sum.cached_tokens,
        calls: sum.calls,
    }
}
