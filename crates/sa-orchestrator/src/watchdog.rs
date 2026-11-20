//! 看门狗：每隔一阵看一遍有没有不对劲的地方，有就提醒人；跑飞了的任务和转圈的对话直接停下。
//!
//! 它不调用模型，也不往外面发任何东西。它能做的只有三件事：记一条提醒、停下一个任务的运行、
//! 停下一个房间里正在进行的对话。

use crate::room::db;
use crate::{budget, work, Orchestrator, Shared};
use sa_engine::{micros_to_cost, EngineError, NewNotice};
use sa_protocol::{
    MessageKind, MessageState, Notice, NoticeKind, NoticeLevel, RoomId, Sender, TaskId, TaskState,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// 同一类提醒（排队、失败率）隔多久才会再提一次。
const REPEAT_AFTER_MS: i64 = 6 * 60 * 60 * 1000;
const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// 找重复的话时往回看多少条消息。
const LOOP_WINDOW: u32 = 40;

/// 看门狗看什么、多严。
#[derive(Debug, Clone, PartialEq)]
pub struct Watchdog {
    /// 隔多久看一遍。
    pub every: Duration,
    /// 一次运行跑了这么久就提醒一次。
    pub run_warn: Duration,
    /// 一次运行跑了这么久就停下它。
    pub run_limit: Duration,
    /// 正在做的任务这么久没有任何动静就停下它。
    pub idle_limit: Duration,
    /// 人说完一句话之后，Bot 把同一句话说了这么多遍，就停下这个房间里的对话。
    pub repeat_limit: usize,
    /// 一个任务这一段的花费超过做完的任务平均花费的这么多倍，就停下它。
    pub spend_factor: f64,
    /// 至少有这么多个做完的任务，才拿平均花费来比。
    pub spend_samples: usize,
    /// 排队的任务超过这么多个就提醒。
    pub backlog_tasks: usize,
    /// 有任务排队超过这么久就提醒。
    pub backlog_wait: Duration,
    /// 最近一小时结束的运行里，失败的超过这个比例就提醒。
    pub failure_ratio: f64,
    /// 最近一小时至少结束了这么多次运行，才看失败的比例。
    pub failure_min_runs: i64,
    /// 磁盘剩余空间低于这个比例就提醒。
    pub min_free_ratio: f64,
    /// 数据库文件超过这么大就提醒。
    pub max_db_bytes: u64,
    /// 数据库文件在哪。`None` 就不看磁盘和数据库。
    pub db_path: Option<PathBuf>,
}

impl Default for Watchdog {
    fn default() -> Self {
        Self {
            every: Duration::from_secs(60),
            run_warn: Duration::from_secs(60 * 60),
            run_limit: Duration::from_secs(3 * 60 * 60),
            idle_limit: Duration::from_secs(30 * 60),
            repeat_limit: 3,
            spend_factor: 3.0,
            spend_samples: 5,
            backlog_tasks: 50,
            backlog_wait: Duration::from_secs(10 * 60),
            failure_ratio: 0.3,
            failure_min_runs: 3,
            min_free_ratio: 0.1,
            max_db_bytes: 1024 * 1024 * 1024,
            db_path: None,
        }
    }
}

impl Orchestrator {
    /// 启动看门狗：每隔一阵看一遍，直到服务关闭。
    pub fn start_watchdog(self: &Arc<Self>) {
        let this = self.clone();
        tokio::spawn(async move {
            let every = this.shared.settings.watchdog.every;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(every) => {}
                    _ = this.shared.shutdown.cancelled() => return,
                }
                this.watchdog_tick().await;
            }
        });
    }

    /// 看一遍。返回这一遍新记下的提醒。
    pub async fn watchdog_tick(&self) -> Vec<Notice> {
        let shared = &self.shared;
        let mut found = vec![];
        let checks: [(&str, Result<Vec<Notice>, EngineError>); 6] = [
            ("运行时长", long_runs(shared).await),
            ("任务动静", idle_tasks(shared).await),
            ("重复的话", self.loops().await),
            ("任务花费", spend_spikes(shared).await),
            ("排队和失败率", queue_and_failures(shared).await),
            ("磁盘和数据库", storage(shared).await),
        ];
        for (name, result) in checks {
            match result {
                Ok(notices) => found.extend(notices),
                Err(error) => tracing::error!("看门狗没能检查{name}：{error}"),
            }
        }
        // 排着队的任务可能在等额度（新的一天）或者等空位：顺手叫醒调度。
        work::kick(shared);
        found
    }

    /// Bot 把同一句话说了好几遍：停下这个房间里的对话。
    async fn loops(&self) -> Result<Vec<Notice>, EngineError> {
        let shared = &self.shared;
        let limit = shared.settings.watchdog.repeat_limit;
        let busy: Vec<RoomId> = {
            let rooms = self.rooms.lock().unwrap();
            rooms
                .iter()
                .filter(|(_, handle)| handle.link.current.lock().unwrap().is_some())
                .map(|(id, _)| id.clone())
                .collect()
        };
        let mut found = vec![];
        for room_id in busy {
            let (engine, id) = (shared.engine.clone(), room_id.clone());
            let page = db(move || engine.list_messages(&id, None, LOOP_WINDOW)).await?;
            // 只看人最后一句话之后 Bot 说的。
            let since = page
                .messages
                .iter()
                .rposition(|message| message.sender == Sender::User);
            let (anchor, said) = match since {
                Some(at) => (page.messages[at].id.to_string(), &page.messages[at + 1..]),
                None => (String::new(), &page.messages[..]),
            };
            let mut counts: HashMap<String, usize> = HashMap::new();
            for message in said {
                if message.kind != MessageKind::Chat || message.state != MessageState::Complete {
                    continue;
                }
                let text: String = message.content.split_whitespace().collect();
                if !text.is_empty() {
                    *counts.entry(text).or_default() += 1;
                }
            }
            if limit == 0 || counts.values().all(|count| *count < limit) {
                continue;
            }
            let notice = NewNotice {
                kind: NoticeKind::Loop,
                level: NoticeLevel::Warning,
                room_id: Some(room_id.clone()),
                task_id: None,
                text: format!(
                    "Bot 把同一句话说了 {limit} 遍以上，看起来在转圈，这个房间里的对话被停下了。换个说法再问一次，或者点名让某个 Bot 来答。"
                ),
            };
            if let Some(notice) = notify(shared, format!("loop:{room_id}:{anchor}"), notice).await?
            {
                self.stop(&room_id);
                found.push(notice);
            }
        }
        Ok(found)
    }
}

async fn notify(
    shared: &Shared,
    key: String,
    notice: NewNotice,
) -> Result<Option<Notice>, EngineError> {
    let engine = shared.engine.clone();
    db(move || engine.notify(&key, notice)).await
}

fn minutes(duration: Duration) -> u64 {
    duration.as_secs().div_ceil(60)
}

/// 跑得太久的运行：先提醒一次，再久就停下。
async fn long_runs(shared: &Arc<Shared>) -> Result<Vec<Notice>, EngineError> {
    let watchdog = &shared.settings.watchdog;
    let engine = shared.engine.clone();
    let runs = db(move || engine.running_runs()).await?;
    let now = sa_engine::now_ms();
    let mut found = vec![];
    for run in runs {
        let ran = Duration::from_millis((now - run.started_at).max(0) as u64);
        let (engine, id) = (shared.engine.clone(), run.task_id.clone());
        let title = db(move || engine.get_task(&id)).await?.title;
        let limit = minutes(watchdog.run_limit);
        if ran >= watchdog.run_limit {
            let reason = format!(
                "这次运行跑了超过 {limit} 分钟还没结束，被自动停下了。看看它做到哪了，再让它继续，或者把任务拆小一点。"
            );
            if !work::halt(shared, &run.task_id, reason) {
                continue;
            }
            let notice = NewNotice {
                kind: NoticeKind::StuckRun,
                level: NoticeLevel::Warning,
                room_id: Some(run.room_id.clone()),
                task_id: Some(run.task_id.clone()),
                text: format!("任务「{title}」的这次运行跑了超过 {limit} 分钟，被自动停下了。"),
            };
            found.extend(notify(shared, format!("runhalt:{}", run.id), notice).await?);
        } else if ran >= watchdog.run_warn {
            let notice = NewNotice {
                kind: NoticeKind::StuckRun,
                level: NoticeLevel::Info,
                room_id: Some(run.room_id.clone()),
                task_id: Some(run.task_id.clone()),
                text: format!(
                    "任务「{title}」的这次运行已经跑了 {} 分钟。跑到 {limit} 分钟会被自动停下。",
                    ran.as_secs() / 60
                ),
            };
            found.extend(notify(shared, format!("runlong:{}", run.id), notice).await?);
        }
    }
    Ok(found)
}

/// 正在做但很久没有动静的任务：停下。
async fn idle_tasks(shared: &Arc<Shared>) -> Result<Vec<Notice>, EngineError> {
    let limit = shared.settings.watchdog.idle_limit;
    let now = sa_engine::now_ms();
    let mut found = vec![];
    for (task_id, last) in work::pulses(shared) {
        if now - last < limit.as_millis() as i64 {
            continue;
        }
        let mins = minutes(limit);
        let reason = format!(
            "{mins} 分钟没有任何动静（没有输出，也没有调用工具），被自动停下了。可能是模型服务没有回应，或者某一步卡死了；让它继续会从停下的地方接着做。"
        );
        if !work::halt(shared, &task_id, reason) {
            continue;
        }
        let (engine, id) = (shared.engine.clone(), task_id.clone());
        let task = db(move || engine.get_task(&id)).await?;
        let notice = NewNotice {
            kind: NoticeKind::StuckTask,
            level: NoticeLevel::Warning,
            room_id: Some(task.room_id),
            task_id: Some(task_id.clone()),
            text: format!(
                "任务「{}」{mins} 分钟没有任何动静，被自动停下了。",
                task.title
            ),
        };
        found.extend(notify(shared, format!("idle:{task_id}:{last}"), notice).await?);
    }
    Ok(found)
}

/// 花得比平常多得多的任务：停下，等人决定要不要继续。
async fn spend_spikes(shared: &Arc<Shared>) -> Result<Vec<Notice>, EngineError> {
    let watchdog = &shared.settings.watchdog;
    let engine = shared.engine.clone();
    let (costs, finished) = db(move || {
        let mut finished = engine.tasks_in_state(TaskState::Done)?;
        finished.extend(engine.tasks_in_state(TaskState::Review)?);
        Ok((engine.task_costs()?, finished))
    })
    .await?;
    let costs: HashMap<TaskId, i64> = costs.into_iter().collect();
    let samples: Vec<i64> = finished
        .iter()
        .filter_map(|task| costs.get(&task.id).copied())
        .filter(|cost| *cost > 0)
        .collect();
    if samples.is_empty() || samples.len() < watchdog.spend_samples {
        return Ok(vec![]);
    }
    let mean = samples.iter().sum::<i64>() as f64 / samples.len() as f64;
    let currency = &shared.settings.budget.currency;
    let mut found = vec![];
    for (task_id, _) in work::pulses(shared) {
        let (engine, id) = (shared.engine.clone(), task_id.clone());
        let (task, spend) =
            db(move || Ok((engine.get_task(&id)?, engine.task_spend(&id)?))).await?;
        let spent = spend.since_resumed();
        if (spent as f64) <= mean * watchdog.spend_factor {
            continue;
        }
        let amounts = format!(
            "{:.2} {currency}，做完的任务平均 {:.2} {currency}",
            micros_to_cost(spent),
            mean / 1_000_000.0
        );
        let reason = format!(
            "这个任务花得比平常多得多（这一段花了 {amounts}），被自动停下了。看看它在做什么，值得就让它继续。"
        );
        if !work::halt(shared, &task_id, reason) {
            continue;
        }
        let notice = NewNotice {
            kind: NoticeKind::Spend,
            level: NoticeLevel::Warning,
            room_id: Some(task.room_id),
            task_id: Some(task_id.clone()),
            text: format!(
                "任务「{}」花得比平常多得多（这一段花了 {amounts}），被自动停下了。",
                task.title
            ),
        };
        found.extend(notify(shared, format!("spend:{task_id}:{}", spend.floor), notice).await?);
    }
    Ok(found)
}

/// 排队的任务太多或者等太久；最近失败的运行太多。
async fn queue_and_failures(shared: &Arc<Shared>) -> Result<Vec<Notice>, EngineError> {
    let watchdog = &shared.settings.watchdog;
    let now = sa_engine::now_ms();
    let engine = shared.engine.clone();
    let (open, (failed, ended)) = db(move || {
        Ok((
            engine.tasks_in_state(TaskState::Open)?,
            engine.run_counts_since(now - 60 * 60 * 1000)?,
        ))
    })
    .await?;
    let bucket = now / REPEAT_AFTER_MS;
    let mut found = vec![];

    let waited = open
        .iter()
        .map(|task| now - task.updated_at)
        .max()
        .unwrap_or(0);
    if open.len() > watchdog.backlog_tasks || waited >= watchdog.backlog_wait.as_millis() as i64 {
        let why = if budget::daily_exhausted(shared).await {
            "今天的花费到上限了，明天才会开工。".to_string()
        } else {
            format!(
                "同时最多做 {} 个任务，其他的要等前面的做完；也可以取消不急的。",
                work::MAX_PARALLEL_RUNS
            )
        };
        let notice = NewNotice {
            kind: NoticeKind::Backlog,
            level: NoticeLevel::Info,
            room_id: None,
            task_id: None,
            text: format!(
                "有 {} 个任务在排队，等得最久的已经等了 {} 分钟。{why}",
                open.len(),
                waited / 60_000
            ),
        };
        found.extend(notify(shared, format!("backlog:{bucket}"), notice).await?);
    }

    if ended >= watchdog.failure_min_runs.max(1)
        && failed as f64 > ended as f64 * watchdog.failure_ratio
    {
        let notice = NewNotice {
            kind: NoticeKind::Failures,
            level: NoticeLevel::Warning,
            room_id: None,
            task_id: None,
            text: format!(
                "最近一小时结束的 {ended} 次运行里有 {failed} 次失败。原因写在各个任务卡上：多半是模型服务或者网络有问题，也可能是花费到了上限。"
            ),
        };
        found.extend(notify(shared, format!("failures:{bucket}"), notice).await?);
    }
    Ok(found)
}

/// 磁盘快满了，或者数据库太大。
async fn storage(shared: &Arc<Shared>) -> Result<Vec<Notice>, EngineError> {
    let watchdog = shared.settings.watchdog.clone();
    let Some(db_path) = watchdog.db_path.clone() else {
        return Ok(vec![]);
    };
    let problems = tokio::task::spawn_blocking(move || {
        let mut problems = vec![];
        let size: u64 = ["", "-wal"]
            .iter()
            .filter_map(|suffix| {
                let mut name = db_path.clone().into_os_string();
                name.push(suffix);
                std::fs::metadata(name).ok().map(|meta| meta.len())
            })
            .sum();
        if size > watchdog.max_db_bytes {
            problems.push(format!(
                "数据库已经有 {} MB 了（{}）。",
                size / (1024 * 1024),
                db_path.display()
            ));
        }
        let dir = db_path.parent().unwrap_or(&db_path);
        if let (Ok(free), Ok(total)) = (fs4::available_space(dir), fs4::total_space(dir)) {
            if total > 0 && (free as f64) < total as f64 * watchdog.min_free_ratio {
                problems.push(format!(
                    "数据目录所在的磁盘只剩 {:.0}% 的空间（{} MB）。",
                    free as f64 / total as f64 * 100.0,
                    free / (1024 * 1024)
                ));
            }
        }
        problems
    })
    .await
    .unwrap_or_default();
    if problems.is_empty() {
        return Ok(vec![]);
    }
    let notice = NewNotice {
        kind: NoticeKind::Storage,
        level: NoticeLevel::Warning,
        room_id: None,
        task_id: None,
        text: format!(
            "{}可以删掉数据目录里 spill 下面的旧文件，取消不要的任务（它们的独立目录会被删掉）。",
            problems.concat()
        ),
    };
    let key = format!("storage:{}", sa_engine::now_ms() / DAY_MS);
    Ok(notify(shared, key, notice).await?.into_iter().collect())
}
