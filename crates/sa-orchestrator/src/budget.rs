//! 花费的上限：每次调用模型之前先看还有没有额度，调用之后记下花了多少。
//!
//! 两道上限：所有房间加起来每天花多少，和每个任务花多少。到了上限，下一次模型调用不会发出去；
//! 已经发出去的那一次照常收完，所以实际花费可能比上限多出一两次调用。

use crate::room::db;
use crate::Shared;
use async_trait::async_trait;
use chrono::{Days, Local, NaiveDate, TimeZone};
use sa_engine::{micros_to_cost, EngineError, NewNotice, UsageRecord};
use sa_llm::{ChatRequest, EventStream, LlmError, ModelCaps, Provider, Usage};
use sa_protocol::{
    BotId, Millis, NoticeKind, NoticeLevel, RoomId, SessionId, TaskId, UsageSummary,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// 花到每日上限的这个比例时提醒一次。
const WARN_RATIO: f64 = 0.8;
/// 「用量」面板里列最近几天。
const SUMMARY_DAYS: u64 = 7;

/// 一个模型的价格：每一百万 token 多少钱。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Price {
    /// 没有命中缓存的输入。
    pub input: f64,
    /// 命中缓存（cache read）的输入。
    pub cached_input: f64,
    /// 写入缓存（Anthropic `cache_creation_input_tokens`）的输入。
    pub cache_write: f64,
    pub output: f64,
}

/// 花费的上限和算钱用的价格。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Budget {
    pub currency: String,
    /// 所有房间加起来每天最多花多少。0 表示不限。
    pub daily_limit: f64,
    /// 一个任务最多花多少；人让它继续一次，就再给一份。0 表示不限。
    pub task_limit: f64,
    /// 各个模型的价格，按模型名找。
    pub prices: HashMap<String, Price>,
    /// 没有单独写价格的模型按这个算。
    pub default_price: Price,
}

impl Budget {
    /// 这次调用花了多少，单位是百万分之一个货币单位。
    pub fn cost_micros(&self, model: &str, usage: Usage) -> i64 {
        let price = self.prices.get(model).unwrap_or(&self.default_price);
        let cached = usage.cached_tokens.min(usage.input_tokens);
        let cache_write = usage
            .cache_write_tokens
            .min(usage.input_tokens.saturating_sub(cached));
        let fresh = usage.input_tokens - cached - cache_write;
        // 价格是每百万 token 的，乘上 token 数正好是百万分之一个货币单位。
        let micros = fresh as f64 * price.input
            + cached as f64 * price.cached_input
            + cache_write as f64 * price.cache_write
            + usage.output_tokens as f64 * price.output;
        micros.round().max(0.0) as i64
    }
}

/// 今天（本机时区）从哪一刻开始。
pub(crate) fn day_start(now: Millis) -> Millis {
    let today = Local
        .timestamp_millis_opt(now)
        .single()
        .map(|time| time.date_naive())
        .unwrap_or_default();
    start_of(today)
}

fn start_of(date: NaiveDate) -> Millis {
    let midnight = date.and_hms_opt(0, 0, 0).expect("零点总是合法的时间");
    // 换夏令时的那天零点可能不存在或者有两个：取最早的那个，实在没有就按 UTC 算。
    Local
        .from_local_datetime(&midnight)
        .earliest()
        .map(|time| time.timestamp_millis())
        .unwrap_or_else(|| midnight.and_utc().timestamp_millis())
}

/// 最近几天每一天的 `(日期, 开始, 结束)`，从早到晚，最后一个是今天。
fn recent_days(now: Millis) -> Vec<(String, Millis, Millis)> {
    let today = Local
        .timestamp_millis_opt(now)
        .single()
        .map(|time| time.date_naive())
        .unwrap_or_default();
    (0..SUMMARY_DAYS)
        .rev()
        .filter_map(|back| {
            let date = today.checked_sub_days(Days::new(back))?;
            let next = date.checked_add_days(Days::new(1))?;
            Some((date.to_string(), start_of(date), start_of(next)))
        })
        .collect()
}

fn money(micros: i64) -> String {
    format!("{:.2}", micros_to_cost(micros))
}

fn to_micros(cost: f64) -> i64 {
    (cost * 1_000_000.0).round() as i64
}

/// 今天的花费到没到每日上限。到了的话返回 `(已花, 上限)`。
async fn daily_spent(shared: &Shared) -> Result<Option<(i64, i64)>, EngineError> {
    let limit = to_micros(shared.settings.budget.daily_limit);
    if limit <= 0 {
        return Ok(None);
    }
    let engine = shared.engine.clone();
    let since = day_start(sa_engine::now_ms());
    let spent = db(move || engine.spent_since(since)).await?;
    Ok(Some((spent, limit)))
}

/// 今天的额度是不是用完了。查不出来时按没用完算（不能因为查账失败就把所有事停下）。
pub(crate) async fn daily_exhausted(shared: &Shared) -> bool {
    matches!(daily_spent(shared).await, Ok(Some((spent, limit))) if spent >= limit)
}

fn daily_text(shared: &Shared, spent: i64, limit: i64) -> String {
    format!(
        "今天的花费到上限了（已花 {} / 上限 {} {}），模型调用已暂停，排着队的任务也先不开工。明天自动恢复；要现在继续，调高 config.toml 里 [budget] 的 daily_limit 再重启服务。",
        money(spent),
        money(limit),
        shared.settings.budget.currency
    )
}

/// 调用模型之前：还有额度就放行，没有就返回给人看的原因。
async fn admit(shared: &Shared, task: Option<&TaskId>) -> Result<(), LlmError> {
    let failed = |error: EngineError| {
        tracing::error!("查不了花费，这次调用照常放行：{error}");
    };
    match daily_spent(shared).await {
        Ok(Some((spent, limit))) if spent >= limit => {
            let text = daily_text(shared, spent, limit);
            notify_daily(shared, "budget100", NoticeLevel::Warning, text.clone()).await;
            return Err(LlmError::Budget(text));
        }
        Ok(_) => {}
        Err(error) => failed(error),
    }
    let limit = to_micros(shared.settings.budget.task_limit);
    let Some(task_id) = task.filter(|_| limit > 0) else {
        return Ok(());
    };
    let (engine, id) = (shared.engine.clone(), task_id.clone());
    let spend = match db(move || engine.task_spend(&id)).await {
        Ok(spend) => spend,
        Err(error) => {
            failed(error);
            return Ok(());
        }
    };
    if spend.since_resumed() < limit {
        return Ok(());
    }
    let text = format!(
        "这个任务的花费到上限了（这一段花了 {} / 上限 {} {}，从开始到现在一共 {}）。让它继续就是再给它一份额度；觉得不值就取消任务。上限在 config.toml 里 [budget] 的 task_limit。",
        money(spend.since_resumed()),
        money(limit),
        shared.settings.budget.currency,
        money(spend.total),
    );
    Err(LlmError::Budget(text))
}

async fn notify_daily(shared: &Shared, name: &str, level: NoticeLevel, text: String) {
    let key = format!("{name}:{}", day_start(sa_engine::now_ms()));
    let engine = shared.engine.clone();
    let notice = NewNotice {
        kind: NoticeKind::Budget,
        level,
        room_id: None,
        task_id: None,
        text,
    };
    if let Err(error) = db(move || engine.notify(&key, notice).map(drop)).await {
        tracing::error!("花费的提醒没记上：{error}");
    }
}

/// 记下一次模型调用的用量和花费；花费过了提醒线就提醒一次。
pub(crate) async fn record(
    shared: &Shared,
    room_id: &RoomId,
    bot_id: Option<&BotId>,
    session_id: Option<&SessionId>,
    model: &str,
    usage: Usage,
) {
    let record = UsageRecord {
        room_id: room_id.clone(),
        bot_id: bot_id.cloned(),
        session_id: session_id.cloned(),
        model: model.to_string(),
        in_tokens: usage.input_tokens as i64,
        out_tokens: usage.output_tokens as i64,
        cached_tokens: usage.cached_tokens as i64,
        cost_micros: shared.settings.budget.cost_micros(model, usage),
    };
    let engine = shared.engine.clone();
    if let Err(error) = db(move || engine.record_usage(record)).await {
        tracing::error!(room = %room_id, "用量没记上：{error}");
        return;
    }
    match daily_spent(shared).await {
        Ok(Some((spent, limit))) if spent >= limit => {
            let text = daily_text(shared, spent, limit);
            notify_daily(shared, "budget100", NoticeLevel::Warning, text).await;
        }
        Ok(Some((spent, limit))) if spent as f64 >= limit as f64 * WARN_RATIO => {
            let text = format!(
                "今天的花费已经到每日上限的 {:.0}%（已花 {} / 上限 {} {}）。到了上限，模型调用会暂停到明天。",
                spent as f64 / limit as f64 * 100.0,
                money(spent),
                money(limit),
                shared.settings.budget.currency
            );
            notify_daily(shared, "budget80", NoticeLevel::Info, text).await;
        }
        Ok(_) => {}
        Err(error) => tracing::error!("查不了今天的花费：{error}"),
    }
}

/// 「用量」面板的内容。
pub(crate) async fn summary(shared: &Shared) -> Result<UsageSummary, EngineError> {
    let engine = shared.engine.clone();
    let days = recent_days(sa_engine::now_ms());
    let breakdown = db(move || engine.usage_breakdown(&days)).await?;
    let budget = &shared.settings.budget;
    Ok(UsageSummary {
        currency: budget.currency.clone(),
        daily_limit: budget.daily_limit,
        task_limit: budget.task_limit,
        today: breakdown.today,
        days: breakdown.days,
        models: breakdown.models,
        bots: breakdown.bots,
        tasks: breakdown.tasks,
    })
}

/// 包在模型外面的一层：每次调用之前先看额度。`task` 是这次调用算在哪个任务头上。
pub(crate) fn guard(
    shared: &Arc<Shared>,
    provider: Arc<dyn Provider>,
    task: Option<TaskId>,
) -> Arc<dyn Provider> {
    Arc::new(Guarded {
        shared: shared.clone(),
        inner: provider,
        task,
    })
}

struct Guarded {
    shared: Arc<Shared>,
    inner: Arc<dyn Provider>,
    task: Option<TaskId>,
}

#[async_trait]
impl Provider for Guarded {
    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, LlmError> {
        admit(&self.shared, self.task.as_ref()).await?;
        self.inner.stream(request, cancel).await
    }

    fn capabilities(&self, model: &str) -> ModelCaps {
        self.inner.capabilities(model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, cached: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
            cache_write_tokens: 0,
        }
    }

    #[test]
    fn cost_uses_the_model_price_and_charges_cached_input_separately() {
        let budget = Budget {
            prices: HashMap::from([(
                "big".to_string(),
                Price {
                    input: 2.0,
                    cached_input: 0.5,
                    cache_write: 2.5,
                    output: 8.0,
                },
            )]),
            default_price: Price {
                input: 1.0,
                cached_input: 1.0,
                cache_write: 1.25,
                output: 1.0,
            },
            ..Budget::default()
        };
        // 100 万输入里 40 万命中缓存：60 万 × 2 + 40 万 × 0.5 + 10 万 × 8 = 1.2 + 0.2 + 0.8
        assert_eq!(
            budget.cost_micros("big", usage(1_000_000, 400_000, 100_000)),
            2_200_000
        );
        // Anthropic 缓存写入独立计费（1.25×）：30 万新鲜×2 + 40 万命中×0.5 + 30 万写入×2.5 + 10 万输出×8 = 0.6 + 0.2 + 0.75 + 0.8 = 2.35
        assert_eq!(
            budget.cost_micros(
                "big",
                Usage {
                    input_tokens: 1_000_000,
                    output_tokens: 100_000,
                    cached_tokens: 400_000,
                    cache_write_tokens: 300_000,
                }
            ),
            2_350_000
        );
        assert_eq!(budget.cost_micros("other", usage(1_000, 0, 500)), 1_500);
        // 上报的缓存数比输入还多：按全部命中缓存算，不会算出负数。
        assert_eq!(budget.cost_micros("big", usage(100, 500, 0)), 50);
    }

    #[test]
    fn without_prices_everything_is_free() {
        assert_eq!(
            Budget::default().cost_micros("any", usage(1_000_000, 0, 1_000_000)),
            0
        );
    }

    #[test]
    fn recent_days_end_today_and_do_not_overlap() {
        let now = sa_engine::now_ms();
        let days = recent_days(now);
        assert_eq!(days.len(), SUMMARY_DAYS as usize);
        let (_, from, to) = days.last().unwrap();
        assert_eq!(*from, day_start(now));
        assert!(*from <= now && now < *to);
        for pair in days.windows(2) {
            assert_eq!(pair[0].2, pair[1].1);
        }
    }
}
