//! 端到端：harness Bot（用测试用的假 harness 命令）在自己的工位目录里干活，步骤和改动交回房间。

use sa_engine::Engine;
use sa_llm::{MockProvider, MockReply};
use sa_orchestrator::{ModelRouter, Orchestrator, ResolvedModel, Settings, SingleModel};
use sa_protocol::{
    ApprovalDecision, Bot, Discussion, Harness, Message, MessageState, ModelTier, RoomId, Sender,
    StepState, UpdateRoomRequest,
};
use sa_store::Store;
use sa_tools::Fence;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const FAKE: &str = env!("CARGO_BIN_EXE_sa-fake-harness");

struct Rig {
    engine: Arc<Engine>,
    orchestrator: Orchestrator,
    room: RoomId,
    work: PathBuf,
}

fn start(dir: &Path, lead: Vec<MockReply>, parallel: usize) -> Rig {
    let data = dir.join("data");
    let work = dir.join("work");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("README.md"), "原来的文件\n").unwrap();
    let engine = Arc::new(Engine::new(Store::open(&data.join("state.db")).unwrap()).unwrap());
    engine.ensure_defaults().unwrap();
    engine.recover_interrupted().unwrap();
    let room = engine.list_rooms().unwrap().pop().unwrap().id;
    let models: Arc<dyn ModelRouter> = Arc::new(SingleModel(ResolvedModel {
        provider: Arc::new(MockProvider::scripted(lead)),
        model: "mock-model".into(),
        thinking: None,
        context_window: None,
    }));
    let settings = Settings {
        sessions_dir: data.join("sessions"),
        fence: Fence::new(&work, std::slice::from_ref(&data)).unwrap(),
        prompt: None,
        max_steps: 6,
        max_parallel_bots: 3,
        max_parallel_harness: parallel,
        allow_private_network: false,
        approval_timeout: Duration::from_secs(600),
        work_dir: data.join("work"),
        github: None,
        budget: Default::default(),
        watchdog: Default::default(),
        routines: Default::default(),
    };
    Rig {
        orchestrator: Orchestrator::new(engine.clone(), models, settings),
        engine,
        room,
        work,
    }
}

impl Rig {
    fn harness(&self, name: &str) -> Bot {
        let bot = self
            .engine
            .create_harness_bot(
                name,
                ModelTier::Mid,
                "",
                Harness::Command,
                &format!("\"{FAKE}\""),
            )
            .unwrap();
        self.engine.add_room_member(&self.room, &bot.id).unwrap();
        bot
    }

    fn messages(&self) -> Vec<Message> {
        self.engine
            .list_messages(&self.room, None, 100)
            .unwrap()
            .messages
    }

    async fn say(&self, text: &str) {
        self.orchestrator
            .post_message(&self.room, text.into())
            .await
            .unwrap();
    }

    async fn until(&self, what: &str, test: impl Fn(&[Message]) -> bool) -> Vec<Message> {
        for _ in 0..1000 {
            let messages = self.messages();
            if test(&messages) {
                return messages;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("等不到：{what}。现在是：{:#?}", self.messages());
    }

    /// 等到 `bot` 有一条写完的回复（`nth` 从 0 开始数），返回它。
    async fn reply(&self, bot: &Bot, nth: usize) -> Message {
        let done = |m: &Message| {
            matches!(&m.sender, Sender::Bot { bot_id, .. } if *bot_id == bot.id)
                && m.state != MessageState::Streaming
        };
        let all = self
            .until("回复写完", |all| {
                all.iter().filter(|m| done(m)).count() > nth
            })
            .await;
        all.into_iter().filter(|m| done(m)).nth(nth).unwrap()
    }

    async fn approve(&self, decision: ApprovalDecision) {
        let all = self
            .until("有一步在等批准", |all| {
                all.iter()
                    .any(|m| m.steps.iter().any(|s| s.state == StepState::Waiting))
            })
            .await;
        let step = all
            .iter()
            .flat_map(|m| m.steps.iter())
            .find(|s| s.state == StepState::Waiting)
            .unwrap();
        assert_eq!(step.tool, "harness");
        let id = step.approval.clone().unwrap().id;
        assert!(self.orchestrator.decide(&id, decision));
    }

    fn desk_dir(&self, bot: &Bot) -> PathBuf {
        let desks = self.engine.room_desks(&self.room).unwrap();
        let desk = desks.iter().find(|d| d.bot_id == bot.id).expect("开过工位");
        PathBuf::from(desk.path.clone().unwrap())
    }
}

fn tools(message: &Message) -> Vec<(&str, StepState)> {
    message
        .steps
        .iter()
        .map(|s| (s.tool.as_str(), s.state))
        .collect()
}

/// 点名一个 harness Bot：先问人，批准后在自己的独立目录里干活，步骤、说的话、改了哪些文件都回到房间里。
/// 「以后都允许」之后不再问，第二次接着上次的会话。
#[tokio::test]
async fn a_harness_bot_works_in_its_own_desk_and_reports_its_steps() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(dir.path(), vec![], 2);
    let fake = rig.harness("Fake");
    rig.say("@Fake 写一个文件").await;
    rig.approve(ApprovalDecision::Always).await;
    let first = rig.reply(&fake, 0).await;
    assert_eq!(first.state, MessageState::Complete, "{first:#?}");
    assert!(
        first.content.contains("假 harness 做完了"),
        "{}",
        first.content
    );
    assert!(first.content.contains("改了 1 个文件"), "{}", first.content);
    assert_eq!(
        tools(&first),
        vec![
            ("harness", StepState::Ok),
            ("workdir", StepState::Ok),
            ("read", StepState::Ok),
            ("write", StepState::Ok),
        ]
    );
    // 改动在它自己的目录里，人的工作目录没动。
    let desk = rig.desk_dir(&fake);
    assert_ne!(desk, rig.work);
    let written: Vec<_> = std::fs::read_dir(&desk)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("harness-"))
        .collect();
    assert_eq!(written.len(), 1);
    assert!(!rig.work.join(&written[0]).exists());
    assert!(desk.join("README.md").exists());
    let desks = rig.engine.room_desks(&rig.room).unwrap();
    assert_eq!(desks[0].changes.len(), 1);
    assert_eq!(desks[0].changes[0].label, "新增");

    rig.say("@Fake 再看一眼").await;
    let second = rig.reply(&fake, 1).await;
    assert_eq!(second.state, MessageState::Complete, "{second:#?}");
    assert!(
        second.content.contains("接着上次的会话"),
        "{}",
        second.content
    );
    // 没再问，也没再开目录。
    assert_eq!(
        tools(&second),
        vec![("harness", StepState::Ok), ("read", StepState::Ok)]
    );
    assert_eq!(rig.desk_dir(&fake), desk);

    // 人验收：改动带回工作目录，工位收掉；下次重新开目录，也不再接着旧会话。
    let left = rig
        .orchestrator
        .accept_desk(&rig.room, &fake.id)
        .await
        .unwrap();
    assert!(left.is_empty());
    assert!(rig.work.join(&written[0]).exists());
    assert!(!desk.exists());
    rig.say("@Fake 再写一个").await;
    let third = rig.reply(&fake, 2).await;
    assert!(
        !third.content.contains("接着上次的会话"),
        "{}",
        third.content
    );
    assert_ne!(rig.desk_dir(&fake), desk);

    // 丢掉：工作目录不变。
    let before = std::fs::read_dir(&rig.work).unwrap().count();
    rig.orchestrator
        .discard_desk(&rig.room, &fake.id)
        .await
        .unwrap();
    assert_eq!(std::fs::read_dir(&rig.work).unwrap().count(), before);
    assert!(rig.engine.room_desks(&rig.room).unwrap().is_empty());
    rig.orchestrator.shutdown().await;
}

/// harness 报错：这一条回复标成失败，原因是它说的；失败的命令那一步标红，但不算整轮失败。
#[tokio::test]
async fn harness_failures_show_up_as_failed_replies_and_failed_steps() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(dir.path(), vec![], 2);
    let fake = rig.harness("Fake");
    rig.say("@Fake 跑一下，命令失败").await;
    rig.approve(ApprovalDecision::Always).await;
    let ran = rig.reply(&fake, 0).await;
    assert_eq!(ran.state, MessageState::Complete);
    assert!(
        tools(&ran).contains(&("shell", StepState::Error)),
        "{ran:#?}"
    );

    rig.say("@Fake 这次出错").await;
    let broke = rig.reply(&fake, 1).await;
    assert_eq!(broke.state, MessageState::Failed);
    assert!(
        broke
            .error
            .as_deref()
            .unwrap_or("")
            .contains("假 harness 按要求出错了"),
        "{broke:#?}"
    );

    rig.orchestrator.shutdown().await;
}

/// 不批准就不启动。
#[tokio::test]
async fn a_denied_harness_never_starts() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(dir.path(), vec![], 2);
    let fake = rig.harness("Fake");
    rig.say("@Fake 写一个文件").await;
    rig.approve(ApprovalDecision::Deny).await;
    let denied = rig.reply(&fake, 0).await;
    assert_eq!(denied.state, MessageState::Failed);
    assert_eq!(tools(&denied), vec![("harness", StepState::Denied)]);
    assert!(rig.engine.room_desks(&rig.room).unwrap().is_empty());
    rig.orchestrator.shutdown().await;
}

/// 点停止：harness 和它开出来的子进程一起结束，回复标「已停止」。
#[tokio::test]
async fn stopping_a_harness_ends_its_whole_process_tree() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(dir.path(), vec![], 2);
    let fake = rig.harness("Fake");
    rig.say("@Fake 慢慢来，开一个子进程").await;
    rig.approve(ApprovalDecision::Once).await;
    let pid_file = rig
        .until("开出子进程", |_| {
            rig.engine
                .room_desks(&rig.room)
                .unwrap()
                .first()
                .and_then(|d| d.path.clone())
                .is_some_and(|p| Path::new(&p).join("child.pid").exists())
        })
        .await;
    drop(pid_file);
    let pid: u32 = std::fs::read_to_string(rig.desk_dir(&fake).join("child.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(rig.orchestrator.stop_bot(&rig.room, &fake.id));
    let stopped = rig.reply(&fake, 0).await;
    assert_eq!(stopped.state, MessageState::Cancelled, "{stopped:#?}");
    #[cfg(unix)]
    {
        let mut gone = false;
        for _ in 0..100 {
            let alive = std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .unwrap()
                .success();
            if !alive {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(gone, "子进程 {pid} 还活着");
    }
    let _ = pid;
    rig.orchestrator.shutdown().await;
}

fn asks(calls: &[(&str, &str, &str)]) -> MockReply {
    let calls: Vec<(String, String)> = calls
        .iter()
        .map(|(id, bot, message)| {
            let arguments = serde_json::json!({ "bot": bot, "message": message }).to_string();
            (id.to_string(), arguments)
        })
        .collect();
    let calls: Vec<(&str, &str, &str)> = calls
        .iter()
        .map(|(id, arguments)| (id.as_str(), "ask_bot", arguments.as_str()))
        .collect();
    MockReply::tool_calls("", &calls)
}

/// 多 harness 讨论：主 Agent 同时派给两个 harness，同时只能跑一个时另一个排队；
/// 两个都交回后主 Agent 只被叫起来一次。
#[tokio::test]
async fn the_lead_hands_work_to_two_harnesses_that_queue_for_a_slot() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(
        dir.path(),
        vec![
            asks(&[
                ("a", "Alpha", "写方案 A，慢一点"),
                ("b", "Beta", "写方案 B"),
            ]),
            MockReply::text("已经派给 Alpha 和 Beta"),
            MockReply::text("汇总：两个方案都写好了"),
        ],
        1,
    );
    let alpha = rig.harness("Alpha");
    let beta = rig.harness("Beta");
    for bot in [&alpha, &beta] {
        rig.engine
            .allow_always(&rig.room, "harness", bot.id.as_str())
            .unwrap();
    }
    let edit = UpdateRoomRequest {
        discussion: Some(Discussion::Lead),
        ..Default::default()
    };
    rig.engine.edit_room(&rig.room, &edit).unwrap();
    rig.say("两个人各写一个方案").await;
    let a = rig.reply(&alpha, 0).await;
    let b = rig.reply(&beta, 0).await;
    assert_eq!(a.state, MessageState::Complete, "{a:#?}");
    assert_eq!(b.state, MessageState::Complete, "{b:#?}");
    // 两个里总有一个排过队。
    let queued = [&a, &b]
        .iter()
        .filter(|m| m.steps.iter().any(|s| s.title.starts_with("排队")))
        .count();
    assert_eq!(queued, 1, "{a:#?}\n{b:#?}");
    let all = rig
        .until("主 Agent 汇总", |all| {
            all.iter()
                .any(|m| m.content.contains("汇总") && m.state == MessageState::Complete)
        })
        .await;
    let summary = all.iter().find(|m| m.content.contains("汇总")).unwrap();
    let mut back = summary.reply_to.clone();
    back.sort_by_key(|id| id.to_string());
    let mut expected = vec![a.id.clone(), b.id.clone()];
    expected.sort_by_key(|id| id.to_string());
    assert_eq!(back, expected);
    // 两个各在自己的目录里。
    assert_ne!(rig.desk_dir(&alpha), rig.desk_dir(&beta));
    rig.orchestrator.shutdown().await;
}
