//! 端到端：花费的上限和看门狗。真实的 Engine、脚本化的假模型，价格定得很高，几句话就到上限。

use sa_engine::Engine;
use sa_llm::{MockProvider, MockReply};
use sa_orchestrator::{
    Budget, ModelRouter, Orchestrator, Price, ResolvedModel, Settings, Watchdog,
};
use sa_protocol::{
    BotId, CreateTaskRequest, Message, MessageState, ModelTier, NoticeKind, RoomId, RunState,
    Sender, Task, TaskId, TaskState,
};
use sa_store::Store;
use sa_tools::Fence;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

struct Models {
    front: Arc<MockProvider>,
    work: Arc<MockProvider>,
}

impl ModelRouter for Models {
    fn resolve(&self, tier: ModelTier) -> Result<ResolvedModel, String> {
        let provider = match tier {
            ModelTier::Strong => self.work.clone(),
            _ => self.front.clone(),
        };
        Ok(ResolvedModel {
            provider,
            model: format!("mock-{}", tier.as_str()),
            thinking: None,
            context_window: None,
        })
    }
}

struct Rig {
    engine: Arc<Engine>,
    orchestrator: Orchestrator,
    front: Arc<MockProvider>,
    work: Arc<MockProvider>,
    room: RoomId,
    bot: BotId,
}

/// 每个 token 一块钱：一句十来个字的回复就是二十来块。
fn pricey(daily_limit: f64, task_limit: f64) -> Budget {
    Budget {
        currency: "CNY".into(),
        daily_limit,
        task_limit,
        default_price: Price {
            input: 1_000_000.0,
            cached_input: 1_000_000.0,
            output: 1_000_000.0,
        },
        ..Budget::default()
    }
}

fn start(
    dir: &Path,
    front: Vec<MockReply>,
    work: Vec<MockReply>,
    budget: Budget,
    watchdog: Watchdog,
) -> Rig {
    let data = dir.join("data");
    let workspace = dir.join("ws");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    let engine = Arc::new(Engine::new(Store::open(&data.join("state.db")).unwrap()).unwrap());
    engine.ensure_defaults().unwrap();
    let room = engine.list_rooms().unwrap().pop().unwrap();
    let front = Arc::new(MockProvider::scripted(front));
    let work = Arc::new(MockProvider::scripted(work));
    let settings = Settings {
        sessions_dir: data.join("sessions"),
        fence: Fence::new(&workspace, std::slice::from_ref(&data)).unwrap(),
        prompt: None,
        max_steps: 8,
        allow_private_network: false,
        approval_timeout: Duration::from_secs(600),
        work_dir: data.join("work"),
        github: None,
        budget,
        watchdog,
    };
    let models = Arc::new(Models {
        front: front.clone(),
        work: work.clone(),
    });
    Rig {
        orchestrator: Orchestrator::new(engine.clone(), models, settings),
        engine,
        front,
        work,
        bot: room.host_bot_id.clone().unwrap(),
        room: room.id,
    }
}

fn report(summary: &str) -> MockReply {
    let input = serde_json::json!({ "status": "done", "summary": summary }).to_string();
    MockReply::tool_calls("", &[("r1", "report", &input)])
}

impl Rig {
    /// 人说一句话，等 Bot 这一轮收尾，返回 Bot 的那条消息。
    async fn say(&self, text: &str) -> Message {
        let sent = self
            .orchestrator
            .post_message(&self.room, text.into())
            .await
            .unwrap();
        for _ in 0..1000 {
            let messages = self
                .engine
                .list_messages(&self.room, None, 100)
                .unwrap()
                .messages;
            let reply = messages.into_iter().find(|m| {
                m.seq > sent.seq && m.sender != Sender::User && m.state != MessageState::Streaming
            });
            if let Some(reply) = reply {
                return reply;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("Bot 没有回应「{text}」");
    }

    async fn open_task(&self, title: &str) -> Task {
        self.orchestrator
            .create_task(
                &self.room,
                CreateTaskRequest {
                    title: title.into(),
                    scope: "看一眼工作目录".into(),
                    acceptance: None,
                    owner_bot_id: Some(self.bot.clone()),
                },
            )
            .await
            .unwrap()
    }

    /// 等任务走到某个状态，并且它的运行都收了尾。
    async fn task_in(&self, id: &TaskId, state: TaskState) -> Task {
        for _ in 0..1000 {
            let task = self.engine.get_task(id).unwrap();
            let running = self
                .engine
                .task_runs(id)
                .unwrap()
                .iter()
                .any(|run| run.state == RunState::Running);
            if task.state == state && (!running || state == TaskState::Working) {
                return task;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "任务没有变成 {state:?}，现在是：{:#?}",
            self.engine.get_task(id).unwrap()
        );
    }

    async fn work_requests(&self, count: usize) {
        for _ in 0..1000 {
            if self.work.requests().len() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("工作会话只发了 {} 次请求", self.work.requests().len());
    }

    fn notices(&self, kind: NoticeKind) -> Vec<String> {
        self.engine
            .list_notices()
            .unwrap()
            .into_iter()
            .filter(|notice| notice.kind == kind)
            .map(|notice| notice.text)
            .collect()
    }
}

#[tokio::test]
async fn at_the_daily_limit_the_next_model_call_is_not_sent() {
    let dir = tempfile::tempdir().unwrap();
    // 第一句回复：输入 10 + 输出 2 = 12 块，过了 10 块的上限。
    let rig = start(
        dir.path(),
        vec![MockReply::text("你好"), MockReply::text("不该说出来")],
        vec![],
        pricey(10.0, 0.0),
        Watchdog::default(),
    );
    let first = rig.say("在吗").await;
    assert_eq!(first.state, MessageState::Complete);
    assert_eq!(first.content, "你好");

    let second = rig.say("再说一句").await;
    assert_eq!(second.state, MessageState::Failed);
    let error = second.error.unwrap();
    assert!(error.contains("今天的花费到上限了"), "{error}");
    assert!(error.contains("已花 12.00 / 上限 10.00 CNY"), "{error}");
    // 模型只被调用了一次：第二次在发出去之前就被拦下了。
    assert_eq!(rig.front.requests().len(), 1);
    assert_eq!(rig.front.remaining(), 1);

    // 提醒只有一条，说的是到上限了；没有再补一条「到 80%」。
    let notices = rig.notices(NoticeKind::Budget);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("今天的花费到上限了"));

    let summary = rig.orchestrator.usage_summary().await.unwrap();
    assert_eq!(summary.today.cost, 12.0);
    assert_eq!(summary.today.calls, 1);
    assert_eq!(summary.daily_limit, 10.0);
    assert_eq!(summary.days.len(), 7);
    assert_eq!(summary.days.last().unwrap().spend.cost, 12.0);
    assert_eq!(summary.models[0].label, "mock-mid");
    assert_eq!(summary.bots.len(), 1);
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn reaching_80_percent_of_the_daily_limit_is_announced_once() {
    let dir = tempfile::tempdir().unwrap();
    // 每句 12 块，上限 30：第一句 40%，第二句 80%，第三句 120%。
    let rig = start(
        dir.path(),
        vec![
            MockReply::text("一一"),
            MockReply::text("二二"),
            MockReply::text("三三"),
        ],
        vec![],
        pricey(30.0, 0.0),
        Watchdog::default(),
    );
    rig.say("一").await;
    assert!(rig.notices(NoticeKind::Budget).is_empty());
    rig.say("二").await;
    let notices = rig.notices(NoticeKind::Budget);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("80%"), "{notices:?}");
    assert!(notices[0].contains("已花 24.00 / 上限 30.00 CNY"));

    // 第三句照常说完（开口之前还没到上限），说完之后到了上限。
    let third = rig.say("三").await;
    assert_eq!(third.state, MessageState::Complete);
    let notices = rig.notices(NoticeKind::Budget);
    assert_eq!(notices.len(), 2, "{notices:?}");
    assert!(notices.iter().any(|text| text.contains("到上限了")));
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn a_task_stops_at_its_limit_and_continuing_grants_another_share() {
    let dir = tempfile::tempdir().unwrap();
    // 工作会话每次调用 15 块（输入 10 + 输出 5），任务上限 20：第二次调用之后超了。
    let list = MockReply::tool_calls("", &[("l1", "list_dir", r#"{"path":"."}"#)]);
    let rig = start(
        dir.path(),
        vec![],
        vec![list.clone(), list.clone(), list, report("看完了")],
        pricey(0.0, 20.0),
        Watchdog::default(),
    );
    let task = rig.open_task("看目录").await;
    let blocked = rig.task_in(&task.id, TaskState::Blocked).await;
    assert!(
        blocked.note.contains("这个任务的花费到上限了"),
        "{}",
        blocked.note
    );
    assert!(
        blocked.note.contains("这一段花了 30.00 / 上限 20.00 CNY"),
        "{}",
        blocked.note
    );
    assert_eq!(rig.work.requests().len(), 2);

    // 人让它继续：再给 20 块。第三次调用 15 块，第四次交了结果，又到了上限；
    // 收尾那句话说不成，但结果已经交了，任务照常等验收。
    rig.orchestrator
        .resume_task(&task.id, "接着做".into(), None)
        .await
        .unwrap();
    let done = rig.task_in(&task.id, TaskState::Review).await;
    assert_eq!(done.state, TaskState::Review);
    assert_eq!(rig.work.requests().len(), 4);
    let spend = rig.engine.task_spend(&task.id).unwrap();
    assert_eq!(spend.total, 60_000_000);
    assert_eq!(spend.floor, 30_000_000);

    let summary = rig.orchestrator.usage_summary().await.unwrap();
    assert_eq!(summary.tasks.len(), 1);
    assert_eq!(summary.tasks[0].label, "看目录");
    assert_eq!(summary.tasks[0].spend.cost, 60.0);

    // 负责的 Bot 被删掉以后，任务花了多少还查得到。
    let other = rig.engine.create_bot("替补", ModelTier::Mid, "").unwrap();
    rig.engine.add_room_member(&rig.room, &other.id).unwrap();
    rig.engine.delete_bot(&rig.bot).unwrap();
    assert_eq!(rig.engine.task_spend(&task.id).unwrap().total, 60_000_000);
    let summary = rig.orchestrator.usage_summary().await.unwrap();
    assert_eq!(summary.tasks[0].spend.cost, 60.0);
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn queued_tasks_do_not_start_while_the_daily_limit_is_used_up() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(
        dir.path(),
        vec![MockReply::text("你好")],
        vec![report("做完了")],
        pricey(10.0, 0.0),
        Watchdog {
            backlog_wait: Duration::ZERO,
            ..Watchdog::default()
        },
    );
    rig.say("在吗").await;
    let task = rig.open_task("等额度").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        rig.engine.get_task(&task.id).unwrap().state,
        TaskState::Open
    );
    assert!(rig.work.requests().is_empty());

    // 看门狗看到有任务在排队，说明是在等额度；再看一遍不会重复提醒。
    let found = rig.orchestrator.watchdog_tick().await;
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].kind, NoticeKind::Backlog);
    assert!(
        found[0].text.contains("今天的花费到上限了"),
        "{}",
        found[0].text
    );
    assert!(rig.orchestrator.watchdog_tick().await.is_empty());
    assert_eq!(
        rig.engine.get_task(&task.id).unwrap().state,
        TaskState::Open
    );
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn the_watchdog_stops_a_task_that_has_gone_quiet() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(
        dir.path(),
        vec![],
        vec![
            MockReply::Hang(vec![]),
            report("这回做完了"),
            MockReply::text("交完了。"),
        ],
        Budget::default(),
        Watchdog {
            idle_limit: Duration::from_millis(50),
            ..Watchdog::default()
        },
    );
    let task = rig.open_task("卡住的任务").await;
    rig.work_requests(1).await;
    // 刚开工，还没到「没动静」的时限。
    assert!(rig.orchestrator.watchdog_tick().await.is_empty());
    tokio::time::sleep(Duration::from_millis(80)).await;

    let found = rig.orchestrator.watchdog_tick().await;
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].kind, NoticeKind::StuckTask);
    assert_eq!(found[0].task_id.as_ref(), Some(&task.id));
    assert_eq!(found[0].room_id.as_ref(), Some(&rig.room));
    assert!(found[0].text.contains("卡住的任务"));

    let blocked = rig.task_in(&task.id, TaskState::Blocked).await;
    assert!(blocked.note.contains("没有任何动静"), "{}", blocked.note);
    let runs = rig.engine.task_runs(&task.id).unwrap();
    assert_eq!(runs[0].state, RunState::Cancelled);

    // 人让它继续，这回正常做完；之前的提醒收起来之后不再出现。
    rig.orchestrator
        .resume_task(&task.id, String::new(), None)
        .await
        .unwrap();
    rig.task_in(&task.id, TaskState::Review).await;
    let notice = rig.engine.list_notices().unwrap().remove(0);
    let dismissed = rig.engine.dismiss_notice(&notice.id).unwrap();
    assert!(dismissed.dismissed_at.is_some());
    assert!(rig.engine.list_notices().unwrap().is_empty());
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn the_watchdog_warns_about_a_long_run_and_then_stops_it() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(
        dir.path(),
        vec![],
        vec![MockReply::Hang(vec![])],
        Budget::default(),
        Watchdog {
            run_warn: Duration::from_millis(50),
            run_limit: Duration::from_millis(400),
            ..Watchdog::default()
        },
    );
    let task = rig.open_task("跑很久的任务").await;
    rig.work_requests(1).await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    let found = rig.orchestrator.watchdog_tick().await;
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].kind, NoticeKind::StuckRun);
    assert!(found[0].text.contains("会被自动停下"), "{}", found[0].text);
    // 同一次运行只提醒一次。
    assert!(rig.orchestrator.watchdog_tick().await.is_empty());
    assert_eq!(
        rig.engine.get_task(&task.id).unwrap().state,
        TaskState::Working
    );

    tokio::time::sleep(Duration::from_millis(400)).await;
    let found = rig.orchestrator.watchdog_tick().await;
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].text.contains("被自动停下了"), "{}", found[0].text);
    let blocked = rig.task_in(&task.id, TaskState::Blocked).await;
    assert!(blocked.note.contains("被自动停下了"), "{}", blocked.note);
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn the_watchdog_reports_failures_and_a_database_that_grew_too_big() {
    let dir = tempfile::tempdir().unwrap();
    let fail = || MockReply::Fail(sa_llm::LlmError::Auth("密钥不对".into()));
    let rig = start(
        dir.path(),
        vec![],
        vec![fail(), fail()],
        Budget::default(),
        Watchdog {
            failure_min_runs: 2,
            max_db_bytes: 1,
            db_path: Some(dir.path().join("data").join("state.db")),
            ..Watchdog::default()
        },
    );
    for title in ["一号", "二号"] {
        let task = rig.open_task(title).await;
        rig.task_in(&task.id, TaskState::Blocked).await;
    }
    let found = rig.orchestrator.watchdog_tick().await;
    let kinds: Vec<NoticeKind> = found.iter().map(|notice| notice.kind).collect();
    assert!(kinds.contains(&NoticeKind::Failures), "{found:?}");
    assert!(kinds.contains(&NoticeKind::Storage), "{found:?}");
    let failures = rig.notices(NoticeKind::Failures);
    assert!(
        failures[0].contains("2 次运行里有 2 次失败"),
        "{failures:?}"
    );
    assert!(rig.notices(NoticeKind::Storage)[0].contains("数据库已经有"));
    assert!(rig.orchestrator.watchdog_tick().await.is_empty());
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn the_watchdog_stops_a_task_that_spends_far_more_than_the_others() {
    let dir = tempfile::tempdir().unwrap();
    let list = MockReply::tool_calls("", &[("l1", "list_dir", r#"{"path":"."}"#)]);
    // 第一个任务花 29 块（交结果 15 + 收尾 14）；第二个任务三次调用花 45 块，然后挂住。
    let rig = start(
        dir.path(),
        vec![],
        vec![
            report("做完了"),
            MockReply::text("交完了。"),
            list.clone(),
            list.clone(),
            list,
            MockReply::Hang(vec![]),
        ],
        pricey(0.0, 0.0),
        Watchdog {
            spend_samples: 1,
            spend_factor: 1.5,
            ..Watchdog::default()
        },
    );
    let first = rig.open_task("普通的任务").await;
    rig.task_in(&first.id, TaskState::Review).await;
    // 只有一个做完的任务、没有正在做的：没什么可比的。
    assert!(rig.orchestrator.watchdog_tick().await.is_empty());

    let second = rig.open_task("花钱的任务").await;
    rig.work_requests(6).await;
    let found = rig.orchestrator.watchdog_tick().await;
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].kind, NoticeKind::Spend);
    assert!(
        found[0]
            .text
            .contains("这一段花了 45.00 CNY，做完的任务平均 29.00 CNY"),
        "{}",
        found[0].text
    );
    let blocked = rig.task_in(&second.id, TaskState::Blocked).await;
    assert!(
        blocked.note.contains("花得比平常多得多"),
        "{}",
        blocked.note
    );
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn a_reply_stopped_halfway_is_still_charged() {
    let dir = tempfile::tempdir().unwrap();
    // 模型说了几个字就挂住，一直没报用量；人点了停止。
    let rig = start(
        dir.path(),
        // 文字放在最后：看到它的时候前面的工具调用肯定已经收到了。
        vec![MockReply::Hang(vec![
            sa_llm::StreamEvent::ToolCallStart {
                id: "c1".into(),
                name: "写".into(),
            },
            sa_llm::StreamEvent::ToolCallArgsDelta {
                id: "c1".into(),
                json_fragment: "{\"内容".into(),
            },
            sa_llm::StreamEvent::TextDelta("说到一半".into()),
        ])],
        vec![],
        pricey(0.0, 0.0),
        Watchdog::default(),
    );
    rig.orchestrator
        .post_message(&rig.room, "在吗".into())
        .await
        .unwrap();
    for _ in 0..500 {
        let said = rig
            .engine
            .list_messages(&rig.room, None, 100)
            .unwrap()
            .messages
            .iter()
            .any(|m| m.content == "说到一半");
        if said {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    rig.orchestrator.stop(&rig.room);
    for _ in 0..500 {
        if rig.orchestrator.usage_summary().await.unwrap().today.calls == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // 没有模型报的数，按字数估：输出是「说到一半」四个字，加上写了一半的工具调用
    //（名字 1 + 参数 3）；输入是发出去的整个请求。
    let today = rig.orchestrator.usage_summary().await.unwrap().today;
    assert_eq!(today.calls, 1);
    assert_eq!(today.output_tokens, 8);
    assert!(today.input_tokens > 0);
    assert!(today.cost > 4.0, "{today:?}");
    rig.orchestrator.shutdown().await;
}
