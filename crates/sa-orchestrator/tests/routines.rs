//! 端到端：定时任务到点之后的一轮。真实的 Engine、脚本化的假模型。

use sa_engine::{now_ms, Engine};
use sa_llm::{MockProvider, MockReply};
use sa_orchestrator::{
    Budget, ModelRouter, Orchestrator, Price, ResolvedModel, Routines, Settings,
};
use sa_protocol::{
    Message, MessageKind, MessageState, ModelTier, NoticeKind, RoomId, Routine, RoutineResult,
    Sender,
};
use sa_store::Store;
use sa_tools::Fence;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

struct Models(Arc<MockProvider>);

impl ModelRouter for Models {
    fn resolve(&self, tier: ModelTier) -> Result<ResolvedModel, String> {
        Ok(ResolvedModel {
            provider: self.0.clone(),
            model: format!("mock-{}", tier.as_str()),
            thinking: None,
            context_window: None,
        })
    }
}

struct Rig {
    engine: Arc<Engine>,
    orchestrator: Arc<Orchestrator>,
    mock: Arc<MockProvider>,
    room: RoomId,
}

fn start(dir: &Path, replies: Vec<MockReply>, budget: Budget) -> Rig {
    start_with(
        dir,
        MockProvider::scripted(replies),
        budget,
        Routines::default(),
    )
}

fn start_with(dir: &Path, mock: MockProvider, budget: Budget, routines: Routines) -> Rig {
    let data = dir.join("data");
    let workspace = dir.join("ws");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    let engine = Arc::new(Engine::new(Store::open(&data.join("state.db")).unwrap()).unwrap());
    engine.ensure_defaults().unwrap();
    let room = engine.list_rooms().unwrap().pop().unwrap();
    let mock = Arc::new(mock);
    let settings = Settings {
        sessions_dir: data.join("sessions"),
        fence: Fence::new(&workspace, std::slice::from_ref(&data)).unwrap(),
        prompt: None,
        max_steps: 8,
        max_parallel_bots: 3,
        max_parallel_harness: sa_orchestrator::DEFAULT_MAX_PARALLEL_HARNESS,
        allow_private_network: false,
        approval_timeout: Duration::from_secs(600),
        work_dir: data.join("work"),
        github: None,
        budget,
        watchdog: Default::default(),
        routines,
    };
    Rig {
        orchestrator: Arc::new(Orchestrator::new(
            engine.clone(),
            Arc::new(Models(mock.clone())),
            settings,
        )),
        engine,
        mock,
        room: room.id,
    }
}

impl Rig {
    /// 建一个定时任务，并把它拨到 `ago` 毫秒之前就该到点。
    fn due(&self, ago: i64) -> Routine {
        let routine = self
            .engine
            .create_routine(&self.room, None, "早报", "看看有什么新消息", "0 9 * * *")
            .unwrap();
        self.engine
            .reschedule_routine(&routine.id, now_ms() - ago)
            .unwrap()
    }

    fn messages(&self) -> Vec<Message> {
        self.engine
            .list_messages(&self.room, None, 100)
            .unwrap()
            .messages
    }

    fn bot_said(&self) -> Vec<String> {
        self.messages()
            .into_iter()
            .filter(|m| m.kind == MessageKind::Chat && m.sender != Sender::User)
            .map(|m| m.content)
            .collect()
    }
}

#[tokio::test]
async fn a_due_routine_is_handed_to_its_bot_and_the_reply_lands_in_the_room() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(
        dir.path(),
        vec![MockReply::text("今天有两条新消息。")],
        Budget::default(),
    );
    // 还没到点的不动。
    let later = rig
        .engine
        .create_routine(&rig.room, None, "晚报", "总结一下", "0 21 * * *")
        .unwrap();
    let routine = rig.due(1_000);

    let done = rig.orchestrator.routines_tick().await;
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].id, routine.id);
    assert_eq!(done[0].last_result, Some(RoutineResult::Spoke));
    assert!(done[0].next_at.unwrap() > now_ms());
    assert_eq!(rig.bot_said(), ["今天有两条新消息。"]);
    assert_eq!(rig.engine.get_routine(&later.id).unwrap().last_at, None);

    // Bot 看到的是定时任务的那段话，并且被告知可以不出声。
    let requests = rig.mock.requests();
    let input = &requests[0].messages.last().unwrap().content;
    assert!(input.contains("定时任务「早报」到点了"), "{input}");
    assert!(input.contains("看看有什么新消息"), "{input}");
    assert!(input.contains("(pass)"), "{input}");

    // 做过了：下一遍没有到点的。
    assert!(rig.orchestrator.routines_tick().await.is_empty());
    assert_eq!(rig.mock.requests().len(), 1);
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn a_bot_with_nothing_to_say_leaves_nothing_in_the_room() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(
        dir.path(),
        vec![MockReply::text("(pass)")],
        Budget::default(),
    );
    rig.due(1_000);
    let done = rig.orchestrator.routines_tick().await;
    assert_eq!(done[0].last_result, Some(RoutineResult::Silent));
    assert_eq!(done[0].failures, 0);
    assert!(rig.bot_said().is_empty());
    // 房间里只有定时任务那一条，界面不把它当成人说的话显示。
    let kinds: Vec<MessageKind> = rig.messages().iter().map(|m| m.kind).collect();
    assert_eq!(kinds, [MessageKind::Routine]);
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn three_failures_in_a_row_disable_the_routine_and_tell_the_human() {
    let dir = tempfile::tempdir().unwrap();
    // 脚本里没有回复：每次调用模型都报错。
    let rig = start(dir.path(), vec![], Budget::default());
    let routine = rig.due(1_000);
    for failures in 1..=3 {
        let done = rig.orchestrator.routines_tick().await;
        assert_eq!(done[0].last_result, Some(RoutineResult::Failed));
        assert_eq!(done[0].failures, failures);
        assert!(!done[0].last_note.is_empty());
        if failures < 3 {
            rig.engine
                .reschedule_routine(&routine.id, now_ms() - 1_000)
                .unwrap();
        }
    }
    let stopped = rig.engine.get_routine(&routine.id).unwrap();
    assert!(!stopped.enabled);
    assert_eq!(stopped.next_at, None);
    let notices = rig.engine.list_notices().unwrap();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].kind, NoticeKind::Failures);
    assert!(notices[0].text.contains("早报"), "{}", notices[0].text);
    assert!(rig.orchestrator.routines_tick().await.is_empty());
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn a_routine_missed_for_too_long_or_over_budget_is_skipped_without_calling_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let budget = Budget {
        daily_limit: 10.0,
        default_price: Price {
            input: 1_000_000.0,
            cached_input: 1_000_000.0,
            output: 1_000_000.0,
        },
        ..Budget::default()
    };
    let rig = start(dir.path(), vec![MockReply::text("第一次照常做。")], budget);
    // 两个小时前就该做了（服务当时没开）：不补做。
    let routine = rig.due(2 * 60 * 60 * 1000);
    let done = rig.orchestrator.routines_tick().await;
    assert_eq!(done[0].last_result, Some(RoutineResult::Skipped));
    assert!(
        done[0].last_note.contains("不补做"),
        "{}",
        done[0].last_note
    );
    assert!(done[0].enabled && done[0].next_at.unwrap() > now_ms());
    assert!(rig.messages().is_empty());

    // 做一次就把今天的额度用完了；再到点时跳过。
    rig.engine
        .reschedule_routine(&routine.id, now_ms() - 1_000)
        .unwrap();
    let done = rig.orchestrator.routines_tick().await;
    assert_eq!(done[0].last_result, Some(RoutineResult::Spoke));
    rig.engine
        .reschedule_routine(&routine.id, now_ms() - 1_000)
        .unwrap();
    let done = rig.orchestrator.routines_tick().await;
    assert_eq!(done[0].last_result, Some(RoutineResult::Skipped));
    assert!(done[0].last_note.contains("上限"), "{}", done[0].last_note);
    assert_eq!(done[0].failures, 0);
    assert_eq!(rig.mock.requests().len(), 1);
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn run_now_does_not_wait_for_the_schedule_and_a_missing_bot_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(
        dir.path(),
        vec![MockReply::text("现在就做了。")],
        Budget::default(),
    );
    let routine = rig
        .engine
        .create_routine(&rig.room, None, "早报", "看看有什么新消息", "0 9 * * *")
        .unwrap();
    rig.orchestrator.run_routine_now(&routine.id).await.unwrap();
    let mut done = routine.clone();
    for _ in 0..500 {
        done = rig.engine.get_routine(&routine.id).unwrap();
        if done.last_at.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(done.last_result, Some(RoutineResult::Spoke));
    assert_eq!(rig.bot_said(), ["现在就做了。"]);

    // 负责的 Bot 被停用了：到点时跳过，不换别人做。
    let patch = sa_engine::BotPatch {
        enabled: Some(false),
        ..Default::default()
    };
    rig.engine.update_bot(&routine.bot_id, patch).unwrap();
    rig.engine
        .reschedule_routine(&routine.id, now_ms() - 1_000)
        .unwrap();
    let done = rig.orchestrator.routines_tick().await;
    assert_eq!(done[0].last_result, Some(RoutineResult::Skipped));
    assert!(done[0].last_note.contains("停用"), "{}", done[0].last_note);
    assert_eq!(rig.mock.requests().len(), 1);
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn routines_due_together_in_one_room_run_one_per_tick() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(
        dir.path(),
        vec![
            MockReply::text("第一个做完了。"),
            MockReply::text("第二个做完了。"),
        ],
        Budget::default(),
    );
    let first = rig.due(2_000);
    let second = rig
        .engine
        .create_routine(&rig.room, None, "晚报", "总结一下", "0 21 * * *")
        .unwrap();
    rig.engine
        .reschedule_routine(&second.id, now_ms() - 1_000)
        .unwrap();

    // 排在后面的不提前占租约：等前面的做完，下一遍再抢。
    let done = rig.orchestrator.routines_tick().await;
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].id, first.id);
    assert_eq!(rig.engine.get_routine(&second.id).unwrap().last_at, None);

    let done = rig.orchestrator.routines_tick().await;
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].id, second.id);
    assert_eq!(done[0].last_result, Some(RoutineResult::Spoke));
    assert_eq!(rig.bot_said(), ["第一个做完了。", "第二个做完了。"]);
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn a_routine_that_runs_out_of_time_is_stopped_not_left_running() {
    let dir = tempfile::tempdir().unwrap();
    // 一个字一个字慢慢写，远远超过给它的时间。
    let slow = MockProvider::scripted(vec![MockReply::text(&"慢".repeat(2_000))])
        .with_chunk_delay(Duration::from_millis(100));
    let routines = Routines {
        settle: Duration::from_millis(300),
        ..Routines::default()
    };
    let rig = start_with(dir.path(), slow, Budget::default(), routines);
    rig.due(1_000);

    let done = rig.orchestrator.routines_tick().await;
    assert_eq!(done[0].last_result, Some(RoutineResult::Failed));
    assert!(
        done[0].last_note.contains("已经停下"),
        "{}",
        done[0].last_note
    );

    // 记成「没做成」的时候它已经停了：不会在租约还掉之后接着写。
    let replies: Vec<Message> = rig
        .messages()
        .into_iter()
        .filter(|m| m.kind == MessageKind::Chat && m.sender != Sender::User)
        .collect();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].state, MessageState::Cancelled);
    let written: usize = replies.iter().map(|reply| reply.content.len()).sum();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let later: usize = rig.bot_said().iter().map(String::len).sum();
    assert_eq!(written, later);
    rig.orchestrator.shutdown().await;
}
