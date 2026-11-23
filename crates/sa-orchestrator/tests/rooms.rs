//! 端到端：真实的 Engine（SQLite 文件）+ 会话记录文件 + 脚本化的假模型。

use sa_engine::{Engine, Incoming, IncomingKind};
use sa_llm::{
    ChatRequest, EventStream, LlmError, MockProvider, MockReply, ModelCaps, Provider, StopReason,
    StreamEvent,
};
use sa_orchestrator::{ModelRouter, Orchestrator, ResolvedModel, Settings, SingleModel};
use sa_protocol::{
    ApprovalDecision, ApprovalId, BotId, Message, MessageState, ModelTier, RoomId, Sender, Source,
    StepState,
};
use sa_store::Store;
use sa_tools::Fence;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

struct Rig {
    engine: Arc<Engine>,
    orchestrator: Orchestrator,
    provider: Arc<MockProvider>,
    room: RoomId,
}

struct NoModel;

impl ModelRouter for NoModel {
    fn resolve(&self, _: ModelTier) -> Result<ResolvedModel, String> {
        Err("没有设置环境变量 DEEPSEEK_API_KEY".into())
    }
}

fn start(dir: &Path, replies: Vec<MockReply>, working_model: bool) -> Rig {
    start_with(dir, replies, working_model, Duration::from_secs(600))
}

fn start_with(
    dir: &Path,
    replies: Vec<MockReply>,
    working_model: bool,
    approval_timeout: Duration,
) -> Rig {
    build(dir, replies, working_model, approval_timeout, None)
}

fn build(
    dir: &Path,
    replies: Vec<MockReply>,
    working_model: bool,
    approval_timeout: Duration,
    context_window: Option<usize>,
) -> Rig {
    let data = dir.join("data");
    let work = dir.join("work");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let engine = Arc::new(Engine::new(Store::open(&data.join("state.db")).unwrap()).unwrap());
    engine.ensure_defaults().unwrap();
    engine.recover_interrupted().unwrap();
    let room = engine.list_rooms().unwrap().pop().unwrap().id;
    let provider = Arc::new(MockProvider::scripted(replies));
    let models: Arc<dyn ModelRouter> = if working_model {
        Arc::new(SingleModel(ResolvedModel {
            provider: provider.clone(),
            model: "mock-model".into(),
            thinking: None,
            context_window,
        }))
    } else {
        Arc::new(NoModel)
    };
    let settings = Settings {
        sessions_dir: data.join("sessions"),
        fence: Fence::new(&work, std::slice::from_ref(&data)).unwrap(),
        prompt: None,
        max_steps: 6,
        max_parallel_bots: 3,
        allow_private_network: false,
        approval_timeout,
        work_dir: data.join("work"),
        github: None,
        budget: Default::default(),
        watchdog: Default::default(),
        routines: Default::default(),
    };
    Rig {
        orchestrator: Orchestrator::new(engine.clone(), models, settings),
        engine,
        provider,
        room,
    }
}

impl Rig {
    fn messages(&self) -> Vec<Message> {
        self.engine
            .list_messages(&self.room, None, 100)
            .unwrap()
            .messages
    }

    async fn say(&self, text: &str) -> Message {
        self.orchestrator
            .post_message(&self.room, text.into())
            .await
            .unwrap()
    }

    /// 等到房间里有 `count` 条消息，并且都不在「正在写」的状态。
    async fn settled(&self, count: usize) -> Vec<Message> {
        for _ in 0..500 {
            let messages = self.messages();
            let writing = messages.iter().any(|m| m.state == MessageState::Streaming);
            if messages.len() >= count && !writing {
                return messages;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "等不到 {count} 条写完的消息，现在是：{:#?}",
            self.messages()
        );
    }

    /// 等到最后一条消息里有一步在等批准，返回那张审批卡的 id。
    async fn waiting_approval(&self) -> ApprovalId {
        let message = self
            .until("有一步在等批准", |m| {
                m.steps.iter().any(|s| s.state == StepState::Waiting)
            })
            .await;
        let step = message
            .steps
            .iter()
            .find(|s| s.state == StepState::Waiting)
            .unwrap();
        step.approval.clone().expect("等批准的步骤带着审批卡").id
    }

    /// 房间最近一次报告的动态。
    fn activity(&self) -> sa_protocol::RoomActivity {
        let events = self.engine.events_after(0, 10_000).unwrap();
        let event = events
            .iter()
            .rev()
            .find(|e| e.kind == sa_protocol::EventKind::RoomActivity)
            .expect("房间报告过动态");
        serde_json::from_value(event.payload.clone()).unwrap()
    }

    /// 等到所有消息里一共有 `count` 步在等批准，返回这些审批卡（按消息先后）。
    async fn waiting_approvals(&self, count: usize) -> Vec<(BotId, ApprovalId)> {
        for _ in 0..500 {
            let waiting: Vec<(BotId, ApprovalId)> = self
                .messages()
                .iter()
                .flat_map(|m| {
                    m.steps
                        .iter()
                        .filter(|s| s.state == StepState::Waiting)
                        .filter_map(|s| Some((sender_of(m)?.clone(), s.approval.clone()?.id)))
                })
                .collect();
            if waiting.len() >= count {
                return waiting;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("等不到 {count} 张审批卡：{:#?}", self.messages());
    }

    /// 等到消息里有 `count` 条正在写的回复，内容都有了。
    async fn until_writing(&self, count: usize) -> Vec<Message> {
        for _ in 0..500 {
            let writing: Vec<Message> = self
                .messages()
                .into_iter()
                .filter(|m| m.state == MessageState::Streaming && !m.content.is_empty())
                .collect();
            if writing.len() >= count {
                return writing;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("等不到 {count} 条正在写的回复：{:#?}", self.messages());
    }

    /// 等到房间报告「没有人在说话，也没有人排队」，并且人发的消息都处理过了。
    async fn until_idle(&self) {
        for _ in 0..500 {
            let last_user = self
                .messages()
                .iter()
                .filter(|m| m.sender == Sender::User)
                .map(|m| m.seq)
                .max()
                .unwrap_or(0);
            let events = self.engine.events_after(0, 10_000).unwrap();
            let idle = events
                .iter()
                .rev()
                .find(|e| e.kind == sa_protocol::EventKind::RoomActivity)
                .map(|e| {
                    serde_json::from_value::<sa_protocol::RoomActivity>(e.payload.clone()).unwrap()
                })
                .is_some_and(|a| {
                    a.speaking.is_none()
                        && a.queued.is_empty()
                        && !a.routing
                        && a.handled_seq >= last_user
                });
            if idle {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("房间一直没闲下来：{:#?}", self.messages());
    }

    /// 最后一次发给模型的请求里，最后一条工具结果。
    fn last_tool_result(&self) -> String {
        let requests = self.provider.requests();
        let request = requests.last().unwrap();
        request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == sa_llm::Role::Tool)
            .map(|m| m.content.clone())
            .unwrap_or_default()
    }

    /// 等到最后一条消息满足条件。
    async fn until(&self, what: &str, test: impl Fn(&Message) -> bool) -> Message {
        for _ in 0..500 {
            if let Some(last) = self.messages().pop().filter(|m| test(m)) {
                return last;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("等不到：{what}。现在是：{:#?}", self.messages());
    }
}

#[tokio::test]
async fn hello_gets_a_streamed_reply_from_the_host_bot() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(dir.path(), vec![MockReply::text("你好，我在。")], true);
    let mut live = rig.engine.subscribe_live();

    let sent = rig.say("你好").await;
    assert_eq!(sent.sender, Sender::User);
    let messages = rig.settled(2).await;
    let reply = &messages[1];
    assert!(matches!(reply.sender, Sender::Bot { .. }));
    assert_eq!(reply.content, "你好，我在。");
    assert_eq!(reply.state, MessageState::Complete);
    assert_eq!(reply.error, None);

    // 文字是一段一段推出来的，版本号连续。
    let mut text = String::new();
    let mut last_rev = 0;
    while let Ok(delta) = live.try_recv() {
        assert_eq!(delta.message_id, reply.id);
        assert_eq!(delta.rev, last_rev + 1);
        last_rev = delta.rev;
        text.push_str(&delta.text);
    }
    assert_eq!(text, "你好，我在。");
    assert!(last_rev > 1);

    let request = &rig.provider.requests()[0];
    assert!(request.messages[0].content.contains("助手"));
    assert!(!request.messages[0].content.contains("{workspace}"));
    assert_eq!(request.messages[1].content, "你好");
    assert!(request.session_id.as_deref().unwrap().starts_with("ses_"));
    let tools: Vec<_> = request.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        tools,
        [
            "read",
            "list_dir",
            "glob",
            "grep",
            "web_fetch",
            "write",
            "edit",
            "shell",
            "todo",
            "create_task"
        ]
    );
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn steps_that_finish_together_all_keep_their_results() {
    let dir = tempfile::tempdir().unwrap();
    let names: Vec<String> = (0..12).map(|n| format!("f{n}.txt")).collect();
    let args: Vec<String> = names
        .iter()
        .map(|name| format!(r#"{{"path":"{name}"}}"#))
        .collect();
    let ids: Vec<String> = (0..names.len()).map(|n| format!("c{n}")).collect();
    let calls: Vec<(&str, &str, &str)> = ids
        .iter()
        .zip(&args)
        .map(|(id, args)| (id.as_str(), "read", args.as_str()))
        .collect();
    let rig = start(
        dir.path(),
        vec![
            MockReply::tool_calls("一起读。", &calls),
            MockReply::text("读完了。"),
        ],
        true,
    );
    for name in &names {
        std::fs::write(dir.path().join("work").join(name), format!("内容 {name}\n")).unwrap();
    }
    rig.say("都读一下").await;
    let reply = rig.settled(2).await.pop().unwrap();
    assert_eq!(reply.state, MessageState::Complete);
    assert_eq!(reply.steps.len(), names.len());
    // 只读的工具是同时跑的：谁先结束都不能让别人的结果从存下来的回复里消失。
    for (step, name) in reply.steps.iter().zip(&names) {
        assert_eq!(step.state, StepState::Ok, "{step:?}");
        let output = step.output.as_deref().unwrap_or_default();
        assert!(output.contains(&format!("内容 {name}")), "{step:?}");
    }
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn reads_a_workspace_file_and_shows_the_step() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(
        dir.path(),
        vec![
            MockReply::tool_calls(
                "我先读一下。",
                &[
                    ("c1", "read", r#"{"path":"说明.md"}"#),
                    ("c2", "read", r#"{"path":"../data/token"}"#),
                ],
            ),
            MockReply::text("里面写的是：项目代号海豚。"),
        ],
        true,
    );
    std::fs::write(dir.path().join("work/说明.md"), "项目代号：海豚\n").unwrap();
    std::fs::write(dir.path().join("data/token"), "机密令牌").unwrap();

    rig.say("读一下 说明.md 并总结").await;
    let reply = rig.settled(2).await.pop().unwrap();
    assert_eq!(reply.state, MessageState::Complete);
    assert_eq!(reply.content, "我先读一下。\n\n里面写的是：项目代号海豚。");
    let steps: Vec<_> = reply
        .steps
        .iter()
        .map(|s| (s.tool.as_str(), s.title.as_str(), s.state))
        .collect();
    assert_eq!(
        steps,
        [
            ("read", "读取 说明.md", StepState::Ok),
            ("read", "读取 ../data/token", StepState::Error),
        ]
    );
    // 每一步交回来的结果留在消息里，给人展开看：读到了什么、为什么没做成。
    let read = reply.steps[0].output.as_deref().unwrap();
    assert!(read.contains("项目代号：海豚"), "{read}");
    let refused = reply.steps[1].output.as_deref().unwrap();
    assert!(
        !refused.is_empty() && !refused.contains("机密令牌"),
        "{refused}"
    );

    let second = &rig.provider.requests()[1];
    let results: Vec<_> = second
        .messages
        .iter()
        .filter(|m| m.tool_call_id.is_some())
        .map(|m| m.content.as_str())
        .collect();
    assert!(results[0].contains("项目代号：海豚"));
    assert!(results[1].contains("工作目录之外") && !results[1].contains("机密令牌"));
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn stop_cancels_the_reply_and_the_room_keeps_working() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(
        dir.path(),
        vec![
            MockReply::Hang(vec![StreamEvent::TextDelta("写到一半".into())]),
            MockReply::text("新的回复"),
        ],
        true,
    );
    assert!(
        !rig.orchestrator.stop(&rig.room),
        "没有东西在跑时停止是空操作"
    );

    rig.say("讲个长故事").await;
    rig.until("回复开始出字", |m| m.content == "写到一半").await;
    assert!(rig.orchestrator.stop(&rig.room));
    let stopped = rig.settled(2).await.pop().unwrap();
    assert_eq!(stopped.state, MessageState::Cancelled);
    assert_eq!(stopped.content, "写到一半");
    // 停下的消息先存好，房间再报空闲：等它报完再看。
    rig.until_idle().await;
    assert!(!rig.orchestrator.stop(&rig.room));

    rig.say("换个话题").await;
    let reply = rig.settled(4).await.pop().unwrap();
    assert_eq!(reply.state, MessageState::Complete);
    assert_eq!(reply.content, "新的回复");
    // 模型能看到被停下的那半句。
    let contents: Vec<_> = rig.provider.requests()[1]
        .messages
        .iter()
        .skip(1)
        .map(|m| m.content.clone())
        .collect();
    assert_eq!(contents, ["讲个长故事", "写到一半", "换个话题"]);
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn messages_sent_while_busy_are_answered_in_a_following_turn() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(
        dir.path(),
        vec![
            MockReply::Hang(vec![StreamEvent::TextDelta("第一轮".into())]),
            MockReply::text("第二轮，看到了两条"),
        ],
        true,
    );
    rig.say("第一条").await;
    rig.until("第一轮开始", |m| m.content == "第一轮").await;
    rig.say("第二条").await;
    rig.say("第三条").await;
    rig.orchestrator.stop(&rig.room);

    let messages = rig.settled(5).await;
    let last = messages.last().unwrap();
    assert_eq!(last.content, "第二轮，看到了两条");
    assert_eq!(rig.provider.requests().len(), 2, "两条插话合在一轮里回答");
    let tail: Vec<_> = rig.provider.requests()[1]
        .messages
        .iter()
        .rev()
        .take(2)
        .map(|m| m.content.clone())
        .collect();
    assert_eq!(tail, ["第三条", "第二条"]);
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn model_failures_surface_on_the_message_and_do_not_loop() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(
        dir.path(),
        vec![
            MockReply::Fail(LlmError::Auth("HTTP 401：Authentication Fails".into())),
            MockReply::text("这次好了"),
        ],
        true,
    );
    rig.say("你好").await;
    let failed = rig.settled(2).await.pop().unwrap();
    assert_eq!(failed.state, MessageState::Failed);
    assert!(failed
        .error
        .as_deref()
        .unwrap()
        .contains("Authentication Fails"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(rig.provider.requests().len(), 1, "失败之后不能自己重跑");
    assert_eq!(rig.messages().len(), 2);

    rig.say("再试一次").await;
    let reply = rig.settled(4).await.pop().unwrap();
    assert_eq!(reply.content, "这次好了");
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn reply_cut_off_at_the_output_limit_is_flagged() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(
        dir.path(),
        vec![MockReply::Events(vec![
            StreamEvent::TextDelta("写到这里就没".into()),
            StreamEvent::Done {
                stop: StopReason::MaxTokens,
            },
        ])],
        true,
    );
    rig.say("写一篇长文").await;
    let reply = rig.settled(2).await.pop().unwrap();
    assert_eq!(reply.state, MessageState::Failed);
    assert_eq!(reply.content, "写到这里就没");
    assert!(reply.error.as_deref().unwrap().contains("截断"));
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn missing_model_is_reported_and_inputs_are_kept_for_later() {
    let dir = tempfile::tempdir().unwrap();
    {
        let rig = start(dir.path(), vec![], false);
        rig.say("密钥还没配").await;
        let failed = rig.settled(2).await.pop().unwrap();
        assert_eq!(failed.state, MessageState::Failed);
        assert!(failed
            .error
            .as_deref()
            .unwrap()
            .contains("DEEPSEEK_API_KEY"));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(rig.messages().len(), 2, "不能反复生成失败消息");
        rig.orchestrator.shutdown().await;
    }
    // 配好之后重启：之前那句话模型也能看到。
    let rig = start(dir.path(), vec![MockReply::text("现在可以了")], true);
    rig.say("现在呢").await;
    let reply = rig.settled(4).await.pop().unwrap();
    assert_eq!(reply.content, "现在可以了");
    let contents: Vec<_> = rig.provider.requests()[0]
        .messages
        .iter()
        .skip(1)
        .map(|m| m.content.clone())
        .collect();
    assert_eq!(contents, ["密钥还没配", "现在呢"]);
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn history_survives_a_restart_and_the_conversation_continues() {
    let dir = tempfile::tempdir().unwrap();
    {
        let rig = start(
            dir.path(),
            vec![
                MockReply::text("记住了，暗号是海豚。"),
                MockReply::Hang(vec![StreamEvent::TextDelta("正说着".into())]),
            ],
            true,
        );
        rig.say("暗号是海豚").await;
        rig.settled(2).await;
        rig.say("再多说点").await;
        rig.until("第二轮开始", |m| m.content == "正说着").await;
        // 服务关闭：正在写的回复标成「中断」，已经写出来的字保留。
        rig.orchestrator.shutdown().await;
        let last = rig.messages().pop().unwrap();
        assert_eq!(last.state, MessageState::Interrupted);
        assert_eq!(last.content, "正说着");
        assert!(rig
            .orchestrator
            .post_message(&rig.room, "关了".into())
            .await
            .is_err());
    }

    let rig = start(dir.path(), vec![MockReply::text("暗号是海豚。")], true);
    let before = rig.messages();
    assert_eq!(before.len(), 4);
    assert_eq!(before[1].content, "记住了，暗号是海豚。");

    rig.say("暗号是什么？").await;
    let reply = rig.settled(6).await.pop().unwrap();
    assert_eq!(reply.content, "暗号是海豚。");
    let contents: Vec<_> = rig.provider.requests()[0]
        .messages
        .iter()
        .skip(1)
        .map(|m| m.content.clone())
        .collect();
    assert_eq!(
        contents,
        [
            "暗号是海豚",
            "记住了，暗号是海豚。",
            "再多说点",
            "正说着",
            "暗号是什么？"
        ]
    );
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn rejects_empty_messages_and_unknown_rooms() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(dir.path(), vec![], true);
    let post = |room: RoomId, text: &str| {
        let text = text.to_string();
        let orchestrator = &rig.orchestrator;
        async move { orchestrator.post_message(&room, text).await }
    };
    assert!(post(rig.room.clone(), "   ").await.is_err());
    assert!(post(RoomId::new(), "你好").await.is_err());
    assert!(rig.messages().is_empty());
    assert!(rig.provider.requests().is_empty());
    rig.orchestrator.shutdown().await;
}

fn shell(id: &str, command: &str) -> MockReply {
    let arguments = serde_json::json!({ "command": command }).to_string();
    MockReply::tool_calls("", &[(id, "shell", &arguments)])
}

fn write(id: &str, path: &str, content: &str) -> MockReply {
    let arguments = serde_json::json!({ "path": path, "content": content }).to_string();
    MockReply::tool_calls("", &[(id, "write", &arguments)])
}

/// 工作目录里面写文件不用问；写完的文件真的在。
#[tokio::test]
async fn writing_inside_the_workdir_needs_no_approval() {
    let dir = tempfile::tempdir().unwrap();
    let replies = vec![
        write("c1", "笔记.md", "第一行\n"),
        MockReply::text("写好了"),
    ];
    let rig = start(dir.path(), replies, true);

    rig.say("记一下").await;
    let messages = rig.settled(2).await;
    let step = &messages[1].steps[0];
    assert_eq!(
        (step.title.as_str(), step.state),
        ("写入 笔记.md", StepState::Ok)
    );
    assert_eq!(step.approval, None);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("work/笔记.md")).unwrap(),
        "第一行\n"
    );
    assert_eq!(rig.last_tool_result(), "已新建 笔记.md（1 行）。");
}

/// 命令要先批准。「允许一次」只管这一次；「总是允许」之后同样的命令不再问。
#[tokio::test]
async fn shell_waits_for_approval_and_always_is_remembered() {
    let dir = tempfile::tempdir().unwrap();
    let replies = vec![
        shell("c1", "echo ok"),
        shell("c2", "echo ok"),
        shell("c3", "echo ok"),
        shell("c4", "echo other"),
        MockReply::text("跑完了"),
    ];
    let rig = start(dir.path(), replies, true);
    rig.say("跑一下").await;

    // 第一次：等批准时命令还没跑。
    let first = rig.waiting_approval().await;
    let waiting = rig.messages().pop().unwrap();
    assert_eq!(waiting.state, MessageState::Streaming);
    let approval = waiting.steps[0].approval.clone().unwrap();
    assert_eq!(approval.reason, "要执行命令：echo ok");
    assert!(approval.expires_at > waiting.created_at);
    assert_eq!(rig.provider.requests().len(), 1);
    assert!(rig.orchestrator.decide(&first, ApprovalDecision::Once));
    // 同一张卡不能答两次。
    assert!(!rig.orchestrator.decide(&first, ApprovalDecision::Once));

    // 第二次同样的命令：还是要问。这次选「总是允许」。
    let second = rig
        .until("第二步在等批准", |m| {
            m.steps.len() == 2 && m.steps[1].state == StepState::Waiting
        })
        .await
        .steps[1]
        .approval
        .clone()
        .unwrap()
        .id;
    assert_ne!(second, first);
    assert!(rig.orchestrator.decide(&second, ApprovalDecision::Always));

    // 第三次不问了；第四次是另一条命令，要问。
    let fourth = rig
        .until("第四步在等批准", |m| {
            m.steps.len() == 4 && m.steps[3].state == StepState::Waiting
        })
        .await;
    assert_eq!(fourth.steps[2].state, StepState::Ok);
    assert_eq!(fourth.steps[2].approval, None);
    let id = fourth.steps[3].approval.clone().unwrap().id;
    assert!(rig.orchestrator.decide(&id, ApprovalDecision::Once));

    let messages = rig.settled(2).await;
    assert_eq!(messages[1].state, MessageState::Complete);
    assert_eq!(messages[1].content, "跑完了");
    assert!(messages[1].steps.iter().all(|s| s.state == StepState::Ok));
    let result = rig.last_tool_result();
    assert!(
        result.starts_with("退出码 0") && result.contains("other"),
        "{result}"
    );
    let rules = rig.engine.list_rules(&rig.room).unwrap();
    assert_eq!(rules.len(), 1);
    // 记的是在哪个工作目录里跑的这条命令。
    assert_eq!(rules[0].tool, "shell");
    assert!(
        rules[0].pattern.ends_with("\necho ok"),
        "{}",
        rules[0].pattern
    );
}

/// 拒绝：不执行，步骤显示「没有批准」，模型被告知并接着说。
#[tokio::test]
async fn denied_approval_does_not_run_and_the_bot_carries_on() {
    let dir = tempfile::tempdir().unwrap();
    let replies = vec![
        write("c1", "../外面.txt", "不该出现"),
        MockReply::text("好的，不写了。"),
    ];
    let rig = start(dir.path(), replies, true);
    rig.say("写到外面去").await;

    let id = rig.waiting_approval().await;
    let reason = rig.messages().pop().unwrap().steps[0]
        .approval
        .clone()
        .unwrap()
        .reason;
    assert!(reason.contains("工作目录之外"), "{reason}");
    assert!(rig.orchestrator.decide(&id, ApprovalDecision::Deny));

    let messages = rig.settled(2).await;
    assert_eq!(messages[1].state, MessageState::Complete);
    assert_eq!(messages[1].content, "好的，不写了。");
    assert_eq!(messages[1].steps[0].state, StepState::Denied);
    assert!(!dir.path().join("外面.txt").exists());
    assert!(rig.last_tool_result().contains("没有批准"));
    assert!(rig.engine.list_rules(&rig.room).unwrap().is_empty());
}

/// 批准写到工作目录之外，并选「总是允许」：记住的是那个文件。
#[tokio::test]
async fn approved_outside_write_happens_and_always_covers_that_file_only() {
    let dir = tempfile::tempdir().unwrap();
    let replies = vec![
        write("c1", "../外面.txt", "一"),
        write("c2", "../外面.txt", "二"),
        write("c3", "../另一个.txt", "三"),
        MockReply::text("好"),
    ];
    let rig = start(dir.path(), replies, true);
    rig.say("写").await;

    let id = rig.waiting_approval().await;
    assert!(rig.orchestrator.decide(&id, ApprovalDecision::Always));
    let third = rig
        .until("第三步在等批准", |m| {
            m.steps.len() == 3 && m.steps[2].state == StepState::Waiting
        })
        .await;
    assert_eq!(third.steps[1].state, StepState::Ok);
    assert_eq!(third.steps[1].approval, None);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("外面.txt")).unwrap(),
        "二"
    );
    let id = third.steps[2].approval.clone().unwrap().id;
    assert!(rig.orchestrator.decide(&id, ApprovalDecision::Deny));
    rig.settled(2).await;
    assert!(!dir.path().join("另一个.txt").exists());
}

/// 苦乐自己的数据目录：不问，直接不让写。
#[tokio::test]
async fn data_dir_writes_are_refused_without_asking() {
    let dir = tempfile::tempdir().unwrap();
    let replies = vec![write("c1", "../data/token", "x"), MockReply::text("写不了")];
    let rig = start(dir.path(), replies, true);
    rig.say("改令牌").await;

    let messages = rig.settled(2).await;
    let step = &messages[1].steps[0];
    assert_eq!(step.state, StepState::Error);
    assert_eq!(step.approval, None);
    assert!(rig.last_tool_result().contains("数据目录"));
    assert!(!dir.path().join("data/token").exists());
}

/// 一直没人处理：到时间按拒绝算，之后再答也没用。
#[tokio::test]
async fn unanswered_approval_expires_as_denied() {
    let dir = tempfile::tempdir().unwrap();
    let replies = vec![shell("c1", "echo ok"), MockReply::text("没批准，就不跑了")];
    let rig = start_with(dir.path(), replies, true, Duration::from_millis(150));
    rig.say("跑一下").await;

    let id = rig.waiting_approval().await;
    let messages = rig.settled(2).await;
    assert_eq!(messages[1].state, MessageState::Complete);
    assert_eq!(messages[1].steps[0].state, StepState::Denied);
    assert!(rig.last_tool_result().contains("没有批准"));
    assert!(!rig.orchestrator.decide(&id, ApprovalDecision::Once));
}

/// 命令退出码不是 0：这一步是「失败」，退出码和输出照样交给模型。
#[tokio::test]
async fn a_command_that_exits_non_zero_is_a_failed_step() {
    let dir = tempfile::tempdir().unwrap();
    let replies = vec![shell("c1", "exit 3"), MockReply::text("命令没成功")];
    let rig = start(dir.path(), replies, true);
    rig.say("跑一下").await;

    let id = rig.waiting_approval().await;
    assert!(rig.orchestrator.decide(&id, ApprovalDecision::Once));
    let messages = rig.settled(2).await;
    assert_eq!(messages[1].state, MessageState::Complete);
    assert_eq!(messages[1].steps[0].state, StepState::Error);
    assert_eq!(rig.last_tool_result(), "退出码 3（没有输出）");
}

/// 等批准时点了停止：这一轮结束，卡片作废。
#[tokio::test]
async fn stop_while_waiting_for_approval_cancels_the_card() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(dir.path(), vec![shell("c1", "echo ok")], true);
    rig.say("跑一下").await;

    let id = rig.waiting_approval().await;
    assert!(rig.orchestrator.stop(&rig.room));
    let messages = rig.settled(2).await;
    assert_eq!(messages[1].state, MessageState::Cancelled);
    assert_eq!(messages[1].steps[0].state, StepState::Cancelled);
    assert!(!rig.orchestrator.decide(&id, ApprovalDecision::Once));
    assert_eq!(rig.provider.requests().len(), 1);
}

/// 对话快占满窗口：自动压缩成摘要，时间线上有一步过程，之后（包括重启之后）接着聊。
#[tokio::test]
async fn long_conversation_is_compacted_and_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let timeout = Duration::from_secs(600);
    // 窗口 60,000：压缩线 27,000。
    let window = Some(60_000);
    let long = |tag: &str, chars: usize| format!("{tag}{}", "长".repeat(chars));
    {
        let replies = vec![
            MockReply::text("一"),
            MockReply::text("二"),
            MockReply::text("用户发了两段很长的话。"),
            MockReply::text("三"),
        ];
        let rig = build(dir.path(), replies, true, timeout, window);
        rig.say(&long("甲", 10_000)).await;
        rig.settled(2).await;
        rig.say(&long("乙", 10_000)).await;
        let all = rig.settled(4).await;
        // 还没到线：没有压缩。
        assert!(all.iter().all(|m| m.steps.is_empty()), "{all:#?}");
        assert_eq!(rig.provider.requests().len(), 2);

        rig.say(&long("丙", 6_000)).await;
        let reply = rig.settled(6).await.pop().unwrap();
        assert_eq!(reply.state, MessageState::Complete, "{reply:?}");
        assert_eq!(reply.content, "三");
        assert_eq!(reply.steps.len(), 1);
        assert_eq!(reply.steps[0].tool, "compact");
        assert_eq!(reply.steps[0].title, "对话太长，压缩前面的内容");
        assert_eq!(reply.steps[0].state, StepState::Ok);

        let requests = rig.provider.requests();
        assert_eq!(requests.len(), 4);
        assert!(requests[2].tools.is_empty());
        assert!(requests[2].messages[1].content.contains("用户：甲长长"));
        let after = &requests[3].messages;
        assert!(after[1].content.ends_with("用户发了两段很长的话。"));
        assert!(after.last().unwrap().content.starts_with("丙长"));
        assert!(after.iter().all(|m| !m.content.starts_with("甲")));
        rig.orchestrator.shutdown().await;
    }

    let rig = build(
        dir.path(),
        vec![MockReply::text("四")],
        true,
        timeout,
        window,
    );
    rig.say("接着聊").await;
    let reply = rig.settled(8).await.pop().unwrap();
    assert_eq!(reply.content, "四");
    assert!(reply.steps.is_empty(), "重启后不用再压缩一次：{reply:?}");
    let messages = &rig.provider.requests()[0].messages;
    assert!(messages[1].content.ends_with("用户发了两段很长的话。"));
    assert_eq!(messages.last().unwrap().content, "接着聊");
    assert!(messages.iter().all(|m| !m.content.starts_with("甲")));
    // 房间里的消息一条没少：压缩的只是发给模型的内容。
    assert_eq!(rig.messages().len(), 8);
    rig.orchestrator.shutdown().await;
}

/// 一条消息就超过了窗口，压缩又失败：回复标成失败，原因写清楚。之后房间还能继续用。
#[tokio::test]
async fn context_limit_is_reported_and_the_room_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let replies = vec![
        MockReply::Fail(sa_llm::LlmError::BadRequest("太长".into())),
        MockReply::text("用户贴了一大段文字。"),
        MockReply::text("现在可以了"),
    ];
    let rig = build(
        dir.path(),
        replies,
        true,
        Duration::from_secs(600),
        Some(60_000),
    );
    // 硬上限 40,000，这一条就超了。
    rig.say(&"长".repeat(45_000)).await;
    let reply = rig.settled(2).await.pop().unwrap();
    assert_eq!(reply.state, MessageState::Failed);
    assert!(
        reply.error.as_deref().unwrap().contains("对话太长"),
        "{reply:?}"
    );
    assert_eq!(reply.steps[0].state, StepState::Error);
    assert_eq!(rig.provider.requests().len(), 1);

    // 那条放不下的消息不会永远卡在会话里：下一轮把它压缩掉，短消息照常得到回复。
    rig.say("换个短的问题").await;
    let reply = rig.settled(4).await.pop().unwrap();
    assert_eq!(reply.state, MessageState::Complete, "{reply:?}");
    assert_eq!(reply.content, "现在可以了");
    let requests = rig.provider.requests();
    let last = &requests.last().unwrap().messages;
    assert!(last[1].content.ends_with("用户贴了一大段文字。"));
    assert_eq!(last.last().unwrap().content, "换个短的问题");
    assert_eq!(last.len(), 3);
    rig.orchestrator.shutdown().await;
}

/// 点「停止」时命令正在跑，而且它自己又启动了后台进程：整棵进程树都要结束。
#[cfg(unix)]
#[tokio::test]
async fn stop_kills_the_running_command_and_its_children() {
    let dir = tempfile::tempdir().unwrap();
    let replies = vec![shell("c1", "sleep 300 & echo $! > child.pid; wait")];
    let rig = start(dir.path(), replies, true);
    rig.say("跑一个很久的命令").await;
    let approval = rig.waiting_approval().await;
    assert!(rig.orchestrator.decide(&approval, ApprovalDecision::Once));

    let pid_file = dir.path().join("work/child.pid");
    let mut pid = String::new();
    for _ in 0..300 {
        pid = std::fs::read_to_string(&pid_file).unwrap_or_default();
        if pid.ends_with('\n') {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let child = std::path::PathBuf::from(format!("/proc/{}", pid.trim()));
    assert!(child.exists(), "子进程没起来：{pid:?}");

    assert!(rig.orchestrator.stop(&rig.room));
    let reply = rig.settled(2).await.pop().unwrap();
    assert_eq!(reply.state, MessageState::Cancelled);
    // 是人停下的，不是命令自己出了错：这一步和回复说的要是同一件事。
    assert_eq!(reply.steps[0].state, StepState::Cancelled);
    for _ in 0..300 {
        if !child.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!child.exists(), "停止之后子进程 {pid} 还活着");
    rig.orchestrator.shutdown().await;
}

/// 等批准的时候，路径上的链接被改指向了别处：批准的是原来那个文件，新的目标不能写。
#[cfg(unix)]
#[tokio::test]
async fn approval_does_not_cover_a_retargeted_link() {
    let dir = tempfile::tempdir().unwrap();
    let (first, second) = (dir.path().join("甲"), dir.path().join("乙"));
    std::fs::create_dir_all(&first).unwrap();
    std::fs::create_dir_all(&second).unwrap();
    let replies = vec![write("c1", "出口/a.txt", "内容"), MockReply::text("好")];
    let rig = start(dir.path(), replies, true);
    let link = dir.path().join("work/出口");
    std::os::unix::fs::symlink(&first, &link).unwrap();

    rig.say("写").await;
    let id = rig.waiting_approval().await;
    let card = rig.messages().pop().unwrap().steps[0].approval.clone();
    assert!(card.unwrap().reason.ends_with("甲/a.txt"));
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&second, &link).unwrap();
    assert!(rig.orchestrator.decide(&id, ApprovalDecision::Once));

    let reply = rig.settled(2).await.pop().unwrap();
    assert_eq!(reply.steps[0].state, StepState::Error);
    assert!(rig.last_tool_result().contains("目标变了"));
    assert!(!first.join("a.txt").exists() && !second.join("a.txt").exists());
    rig.orchestrator.shutdown().await;
}

// ───────────────────────── 多个 Bot ─────────────────────────

/// 路由用便宜档和强档，Bot 都用中档：两边的脚本分开写，互不干扰。
struct Tiered {
    router: Arc<MockProvider>,
    bots: Arc<dyn Provider>,
}

/// 几个 Bot 同时说话时，各自用各自的脚本（按系统提示词里的名字分）；没有单独脚本的用共用的那份。
struct ByBot {
    shared: Arc<MockProvider>,
    own: Mutex<Vec<(String, Arc<MockProvider>)>>,
}

impl ByBot {
    fn pick(&self, request: &ChatRequest) -> Arc<MockProvider> {
        let system = request
            .messages
            .iter()
            .find(|m| m.role == sa_llm::Role::System)
            .map(|m| m.content.clone())
            .unwrap_or_default();
        let own = self.own.lock().unwrap();
        own.iter()
            .find(|(name, _)| system.contains(&format!("名字叫「{name}」")))
            .map_or_else(|| self.shared.clone(), |(_, provider)| provider.clone())
    }
}

#[async_trait::async_trait]
impl Provider for ByBot {
    async fn stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<EventStream, LlmError> {
        self.pick(&request).stream(request, cancel).await
    }

    fn capabilities(&self, model: &str) -> ModelCaps {
        self.shared.capabilities(model)
    }
}

impl ModelRouter for Tiered {
    fn resolve(&self, tier: ModelTier) -> Result<ResolvedModel, String> {
        let provider = match tier {
            ModelTier::Mid => self.bots.clone(),
            _ => self.router.clone(),
        };
        Ok(ResolvedModel {
            provider,
            model: format!("mock-{}", tier.as_str()),
            thinking: None,
            context_window: None,
        })
    }
}

struct Team {
    rig: Rig,
    router: Arc<MockProvider>,
    by_bot: Arc<ByBot>,
    host: BotId,
    coder: BotId,
    reviewer: BotId,
}

/// 一个房间，三个 Bot：助手（入口）、Coder、Reviewer。同一个目录再开一次就是重启。
fn team(dir: &Path, router: Vec<MockReply>, bots: Vec<MockReply>) -> Team {
    team_with(dir, router, bots, 3)
}

impl Team {
    /// 给名叫 `name` 的 Bot 一份自己的脚本，返回它的假模型（看它收到了什么）。
    fn script(&self, name: &str, replies: Vec<MockReply>) -> Arc<MockProvider> {
        let provider = Arc::new(MockProvider::scripted(replies));
        let mut own = self.by_bot.own.lock().unwrap();
        own.push((name.to_string(), provider.clone()));
        provider
    }
}

/// `parallel`：一个房间里最多几个 Bot 同时说话。
fn team_with(dir: &Path, router: Vec<MockReply>, bots: Vec<MockReply>, parallel: usize) -> Team {
    let data = dir.join("data");
    let work = dir.join("work");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let engine = Arc::new(Engine::new(Store::open(&data.join("state.db")).unwrap()).unwrap());
    engine.ensure_defaults().unwrap();
    engine.recover_interrupted().unwrap();
    let room = engine.list_rooms().unwrap().pop().unwrap();
    let member = |name: &str, persona: &str| {
        let existing = engine
            .list_bots()
            .unwrap()
            .into_iter()
            .find(|b| b.name == name);
        let bot =
            existing.unwrap_or_else(|| engine.create_bot(name, ModelTier::Mid, persona).unwrap());
        engine.add_room_member(&room.id, &bot.id).unwrap();
        bot.id
    };
    let coder = member("Coder", "写代码、改 bug");
    let reviewer = member("Reviewer", "审查代码");
    let (router, bots) = (
        Arc::new(MockProvider::scripted(router)),
        Arc::new(MockProvider::scripted(bots)),
    );
    let settings = Settings {
        sessions_dir: data.join("sessions"),
        fence: Fence::new(&work, std::slice::from_ref(&data)).unwrap(),
        prompt: None,
        max_steps: 6,
        max_parallel_bots: parallel,
        allow_private_network: false,
        approval_timeout: Duration::from_secs(600),
        work_dir: data.join("work"),
        github: None,
        budget: Default::default(),
        watchdog: Default::default(),
        routines: Default::default(),
    };
    let by_bot = Arc::new(ByBot {
        shared: bots.clone(),
        own: Mutex::default(),
    });
    let models = Arc::new(Tiered {
        router: router.clone(),
        bots: by_bot.clone(),
    });
    Team {
        rig: Rig {
            orchestrator: Orchestrator::new(engine.clone(), models, settings),
            engine,
            provider: bots,
            room: room.id,
        },
        router,
        by_bot,
        host: room.host_bot_id.unwrap(),
        coder,
        reviewer,
    }
}

fn ask(id: &str, bot: &str, message: &str) -> MockReply {
    let arguments = serde_json::json!({ "bot": bot, "message": message }).to_string();
    MockReply::tool_calls("", &[(id, "ask_bot", &arguments)])
}

fn verdict(bot: &BotId, confidence: f64) -> MockReply {
    MockReply::text(&format!(
        r#"{{"bot_id":"{bot}","confidence":{confidence},"reason":"测试"}}"#
    ))
}

fn sender_of(message: &Message) -> Option<&BotId> {
    match &message.sender {
        Sender::Bot { bot_id } => Some(bot_id),
        Sender::User => None,
    }
}

/// 最后一次发给 Bot 模型的请求里，人和别的 Bot 说的话。
fn heard(provider: &MockProvider) -> Vec<String> {
    let requests = provider.requests();
    requests
        .last()
        .unwrap()
        .messages
        .iter()
        .filter(|m| m.role == sa_llm::Role::User)
        .map(|m| m.content.clone())
        .collect()
}

/// 点了名：直接给被点到的 Bot，不问路由。它知道房间里还有谁，也拿到了找别的 Bot 的工具。
#[tokio::test]
async fn a_mention_goes_straight_to_that_bot() {
    let dir = tempfile::tempdir().unwrap();
    let t = team(dir.path(), vec![], vec![MockReply::text("我来看")]);
    let sent = t.rig.say("@coder 看一下登录页").await;
    assert_eq!(sent.mentions, std::slice::from_ref(&t.coder));

    let reply = t.rig.settled(2).await.pop().unwrap();
    assert_eq!(sender_of(&reply), Some(&t.coder));
    assert_eq!((reply.content.as_str(), reply.hop), ("我来看", 0));
    assert!(t.router.requests().is_empty(), "点了名就不该问路由");

    let request = &t.rig.provider.requests()[0];
    let system = &request.messages[0].content;
    assert!(
        system.contains("你的分工和人设：\n写代码、改 bug"),
        "{system}"
    );
    assert!(
        system.contains("- Reviewer：审查代码") && system.contains("- 助手："),
        "{system}"
    );
    assert!(!system.contains("- Coder："), "成员列表里不该有它自己");
    assert!(!system.contains("{team}") && !system.contains("{persona}"));
    assert!(request.tools.iter().any(|tool| tool.name == "ask_bot"));
    assert_eq!(
        heard(&t.rig.provider),
        ["【用户（对你说）】@coder 看一下登录页"]
    );
    t.rig.orchestrator.shutdown().await;
}

/// 没点名：路由按分工挑；挑不出来（乱答、把握不够）就换强模型再问一次，还不行交给入口 Bot。
#[tokio::test]
async fn unaddressed_messages_are_routed_and_fall_back_to_the_host() {
    let dir = tempfile::tempdir().unwrap();
    let probe = team(dir.path(), vec![], vec![]);
    let (coder, reviewer) = (probe.coder.clone(), probe.reviewer.clone());
    probe.rig.orchestrator.shutdown().await;
    drop(probe);

    let router = vec![
        // 第一条：便宜模型一次选对。
        verdict(&reviewer, 0.9),
        // 第二条：便宜模型把握不够，强模型选了 Coder。
        verdict(&reviewer, 0.3),
        verdict(&coder, 0.8),
        // 第三条：两次都答得不合规矩。
        MockReply::text("让 Reviewer 来吧"),
        MockReply::text(r#"{"bot_id":"bot_nobody","confidence":1}"#),
    ];
    let bots = vec![
        MockReply::text("审查意见"),
        MockReply::text("代码改好了"),
        MockReply::text("我是入口"),
    ];
    let t = team(dir.path(), router, bots);

    t.rig.say("帮我审一下这个改动").await;
    let reply = t.rig.settled(2).await.pop().unwrap();
    assert_eq!(sender_of(&reply), Some(&t.reviewer));
    let asked = &t.router.requests()[0];
    // 有的模型服务不带会话标识就拒绝请求：路由的请求也要带，按房间固定。
    assert_eq!(
        asked.session_id.as_deref(),
        Some(format!("route_{}", t.rig.room).as_str())
    );
    assert!(asked.tools.is_empty());
    assert_eq!(asked.model, "mock-cheap");
    let material = &asked.messages[1].content;
    assert!(
        material.contains("名字: Reviewer，分工: 审查代码"),
        "{material}"
    );
    assert!(material.contains("入口 Bot"), "{material}");
    assert!(
        material.ends_with("【用户刚发的消息】\n帮我审一下这个改动\n"),
        "{material}"
    );
    // 挑人的这段时间对外是「有事情在进行」，界面上能点停止。
    let routing = t
        .rig
        .engine
        .events_after(0, 10_000)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == sa_protocol::EventKind::RoomActivity)
        .map(|e| serde_json::from_value::<sa_protocol::RoomActivity>(e.payload).unwrap())
        .filter(|a| a.routing)
        .count();
    assert_eq!(routing, 1);
    // 没点名的话，在多 Bot 房间里也标明是用户说的。
    assert_eq!(heard(&t.rig.provider), ["【用户】帮我审一下这个改动"]);

    t.rig.say("那个 bug 修一下").await;
    let reply = t.rig.settled(4).await.pop().unwrap();
    assert_eq!(sender_of(&reply), Some(&t.coder));
    assert_eq!(t.router.requests().len(), 3);
    assert_eq!(t.router.requests()[2].model, "mock-strong");
    // Coder 第一次被叫到：前面它没参与的对话也一起看到了。
    assert_eq!(
        heard(&t.rig.provider),
        [
            "【用户】帮我审一下这个改动",
            "【Reviewer】审查意见",
            "【用户】那个 bug 修一下"
        ]
    );

    t.rig.say("随便聊聊").await;
    let reply = t.rig.settled(6).await.pop().unwrap();
    assert_eq!(sender_of(&reply), Some(&t.host));
    assert_eq!(reply.content, "我是入口");
    assert_eq!(t.router.requests().len(), 5);
    t.rig.orchestrator.shutdown().await;
}

/// `ask_bot`：对方马上在自己那边开始，找人的那个接着做自己的事，两边同时在说。
#[tokio::test]
async fn ask_bot_starts_the_other_bot_without_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let t = team(dir.path(), vec![], vec![]);
    t.script(
        "Coder",
        vec![
            ask("c1", "@reviewer", "改好了，帮我看看 diff"),
            MockReply::Hang(vec![StreamEvent::TextDelta("我接着写测试".into())]),
        ],
    );
    let reviewer = t.script("Reviewer", vec![MockReply::text("看过了，没问题")]);
    t.rig.say("@Coder 修一下 bug").await;

    // Coder 还在写，Reviewer 已经答完了。
    let answer = t
        .rig
        .until("Reviewer 答完", |m| {
            m.content == "看过了，没问题" && m.state == MessageState::Complete
        })
        .await;
    let all = t.rig.messages();
    let (reply, asked) = (&all[1], &all[2]);
    assert_eq!(sender_of(reply), Some(&t.coder));
    assert_eq!(reply.state, MessageState::Streaming);
    assert_eq!(reply.steps.len(), 1);
    assert_eq!(reply.steps[0].title, "请 reviewer：改好了，帮我看看 diff");
    assert_eq!(reply.steps[0].state, StepState::Ok);
    assert_eq!(reply.steps[0].approval, None, "找别的 Bot 不用人批准");
    assert_eq!(sender_of(asked), Some(&t.coder));
    assert_eq!(asked.content, "@Reviewer 改好了，帮我看看 diff");
    assert_eq!(
        (asked.mentions.clone(), asked.hop),
        (vec![t.reviewer.clone()], 1)
    );
    assert_eq!(sender_of(&answer), Some(&t.reviewer));
    assert_eq!(answer.hop, 1);
    // Reviewer 看不到 Coder 还没写完的话。
    assert_eq!(
        heard(&reviewer),
        [
            "【用户】@Coder 修一下 bug",
            "【Coder（对你说）】@Reviewer 改好了，帮我看看 diff"
        ]
    );
    // Reviewer 那条道收尾后，房间里只剩 Coder 在说。
    t.rig
        .until("只剩 Coder", |_| {
            t.rig.activity().speakers == vec![t.coder.clone()]
        })
        .await;

    assert!(t.rig.orchestrator.stop_bot(&t.rig.room, &t.coder));
    t.rig.until_idle().await;
    assert_eq!(t.rig.messages()[1].state, MessageState::Cancelled);
    assert!(t.router.requests().is_empty());
    t.rig.orchestrator.shutdown().await;
}

/// 两个 Bot 来回客套：第三次互相找被掐断，被拒绝的话不会出现在房间里。找不存在的 Bot、找自己也不行。
#[tokio::test]
async fn polite_ping_pong_is_cut_off_at_the_third_ask() {
    let dir = tempfile::tempdir().unwrap();
    let bots = vec![
        ask("c1", "Reviewer", "辛苦看一下第一版"),
        MockReply::text("交给 Reviewer 了"),
        ask("r1", "Coder", "看过了，你再确认一下细节"),
        MockReply::text("请 Coder 确认"),
        MockReply::tool_calls(
            "",
            &[
                (
                    "c2",
                    "ask_bot",
                    r#"{"bot":"Reviewer","message":"好的，那再麻烦你一次"}"#,
                ),
                ("c3", "ask_bot", r#"{"bot":"Nobody","message":"在吗"}"#),
                ("c4", "ask_bot", r#"{"bot":"Coder","message":"自言自语"}"#),
            ],
        ),
        MockReply::text("我们来回太多次了，先停在这里"),
    ];
    let t = team(dir.path(), vec![], bots);
    t.rig.say("@Coder 开始吧").await;
    let all = t.rig.settled(6).await;

    let texts: Vec<&str> = all.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(
        texts,
        [
            "@Coder 开始吧",
            "交给 Reviewer 了",
            "@Reviewer 辛苦看一下第一版",
            "请 Coder 确认",
            "@Coder 看过了，你再确认一下细节",
            "我们来回太多次了，先停在这里",
        ]
    );
    assert_eq!(all[5].hop, 2);
    let states: Vec<_> = all[5].steps.iter().map(|s| s.state).collect();
    assert_eq!(states, [StepState::Error; 3]);
    let results: Vec<String> = t.rig.provider.requests()[5]
        .messages
        .iter()
        .filter(|m| m.role == sa_llm::Role::Tool)
        .map(|m| m.content.clone())
        .collect();
    // 前面还有它第一次找 Reviewer 的那条回执，只看这一步的三条。
    let results = &results[results.len() - 3..];
    assert!(results[0].contains("互相找了 2 次"), "{results:?}");
    assert!(
        results[1].contains("没有叫「Nobody」的 Bot") && results[1].contains("Reviewer"),
        "{results:?}"
    );
    assert_eq!(results[2], "不能找自己。");
    // 没有第七次模型调用：被掐断之后没有人再被叫起来。
    assert_eq!(t.rig.provider.requests().len(), 6);
    t.rig.orchestrator.shutdown().await;
}

/// 被叫到但没什么要说的：回 (pass)，房间里不留下它的消息，也不闪一下。
#[tokio::test]
async fn a_bot_that_passes_leaves_no_message() {
    let dir = tempfile::tempdir().unwrap();
    let bots = vec![
        ask("c1", "Reviewer", "谢谢你刚才的帮忙"),
        MockReply::text("已经道过谢了"),
        MockReply::text("(pass)"),
        // 下一轮：看起来像 (pass) 开头、其实不是的话照常显示。
        MockReply::text("(pa)renthesis"),
    ];
    let t = team(dir.path(), vec![], bots);
    let mut live = t.rig.engine.subscribe_live();
    t.rig.say("@Coder 跟 Reviewer 道个谢").await;
    t.rig.until_idle().await;
    let all = t.rig.messages();
    assert_eq!(all.len(), 3, "{all:#?}");
    assert!(all.iter().all(|m| sender_of(m) != Some(&t.reviewer)));
    assert_eq!(t.rig.provider.requests().len(), 3, "Reviewer 确实被叫到过");
    let mut shown = String::new();
    while let Ok(delta) = live.try_recv() {
        shown.push_str(&delta.text);
    }
    assert_eq!(shown, "已经道过谢了");

    t.rig.say("@Reviewer 说点什么").await;
    let reply = t.rig.settled(5).await.pop().unwrap();
    assert_eq!(reply.content, "(pa)renthesis");
    t.rig.orchestrator.shutdown().await;
}

/// Bot 正在说时人点名另一个 Bot：不打断正在说的，被点到的同时开始。点名的话只给被点到的。
#[tokio::test]
async fn mentioning_another_bot_starts_it_alongside_the_speaker() {
    let dir = tempfile::tempdir().unwrap();
    let t = team(dir.path(), vec![], vec![]);
    let coder = t.script(
        "Coder",
        vec![
            MockReply::tool_calls("", &[("c1", "shell", r#"{"command":"echo hi"}"#)]),
            MockReply::text("命令跑完了"),
        ],
    );
    let reviewer = t.script("Reviewer", vec![MockReply::text("我来接")]);
    t.rig.say("@Coder 讲讲").await;
    let approval = t.rig.waiting_approval().await;
    t.rig.say("@Reviewer 你也看看").await;

    let answer = t
        .rig
        .until("Reviewer 答完", |m| {
            m.content == "我来接" && m.state == MessageState::Complete
        })
        .await;
    assert_eq!(sender_of(&answer), Some(&t.reviewer));
    assert_eq!(
        t.rig.messages()[1].state,
        MessageState::Streaming,
        "Coder 没被打断"
    );
    assert_eq!(
        heard(&reviewer),
        [
            "【用户】@Coder 讲讲",
            "【用户（对你说）】@Reviewer 你也看看"
        ]
    );

    assert!(t.rig.orchestrator.decide(&approval, ApprovalDecision::Once));
    t.rig.until_idle().await;
    let all = t.rig.messages();
    assert_eq!(all[1].content, "命令跑完了");
    assert_eq!(all[1].state, MessageState::Complete);
    assert!(
        !heard(&coder).iter().any(|h| h.contains("你也看看")),
        "点名 Reviewer 的话不进 Coder 的上下文：{:?}",
        heard(&coder)
    );
    t.rig.orchestrator.shutdown().await;
}

/// 「点名找别人」的消息落库之后、打断信号发出之前，正在说话的 Bot 可能刚好读到它。
/// 这条消息不能算成它处理过的：等它这一轮结束，被点到的 Bot 仍然要来接。
#[tokio::test]
async fn a_message_for_another_bot_is_not_swallowed_by_the_speaker() {
    let dir = tempfile::tempdir().unwrap();
    let bots = vec![
        // Coder 要执行命令，停在审批卡上。
        MockReply::tool_calls("", &[("c1", "shell", r#"{"command":"echo hi"}"#)]),
        MockReply::text("命令跑完了"),
        MockReply::text("我来接"),
    ];
    let t = team(dir.path(), vec![], bots);
    t.rig.say("@Coder 跑一下").await;
    let approval = t.rig.waiting_approval().await;
    // 只落库、不发打断信号：就是那个空档里的样子。
    t.rig
        .engine
        .post_user_message(&t.rig.room, "@Reviewer 还是你来")
        .unwrap();
    // 命令做完，Coder 在下一步之前读到了这条消息。
    assert!(t.rig.orchestrator.decide(&approval, ApprovalDecision::Once));
    t.rig.until_idle().await;

    let all = t.rig.messages();
    let last = all.last().unwrap();
    assert_eq!(sender_of(last), Some(&t.reviewer), "{all:#?}");
    assert_eq!(last.content, "我来接");
    t.rig.orchestrator.shutdown().await;
}

/// 正在说话的 Bot 先读到一条同时点了它和别人的消息：被点到的别人照样要开始。
#[tokio::test]
async fn a_message_for_the_speaker_and_another_bot_reaches_both() {
    let dir = tempfile::tempdir().unwrap();
    let t = team(dir.path(), vec![], vec![]);
    t.script(
        "Coder",
        vec![
            MockReply::tool_calls("", &[("c1", "shell", r#"{"command":"echo hi"}"#)]),
            MockReply::text("命令跑完了"),
        ],
    );
    t.script("Reviewer", vec![MockReply::text("我也来")]);
    t.rig.say("@Coder 跑一下").await;
    let approval = t.rig.waiting_approval().await;
    // 只落库、不叫醒房间：让 Coder 先读到它。
    t.rig
        .engine
        .post_user_message(&t.rig.room, "@Coder @Reviewer 一起看看")
        .unwrap();
    assert!(t.rig.orchestrator.decide(&approval, ApprovalDecision::Once));
    t.rig
        .until("Reviewer 答完", |m| {
            m.content == "我也来" && m.state == MessageState::Complete
        })
        .await;
    t.rig.until_idle().await;
    t.rig.orchestrator.shutdown().await;
}

/// 点了一个 Bot 的停止：它说话期间收到、还没看到的话也不再答。
#[tokio::test]
async fn a_stopped_bot_does_not_come_back_for_an_unread_follow_up() {
    let dir = tempfile::tempdir().unwrap();
    let t = team(dir.path(), vec![], vec![]);
    t.script(
        "Coder",
        vec![
            MockReply::Hang(vec![StreamEvent::TextDelta("Coder 在写".into())]),
            MockReply::text("不该再说"),
        ],
    );
    t.rig.say("@Coder 讲讲").await;
    t.rig.until_writing(1).await;
    t.rig.say("@Coder 再补一句").await;
    // 等房间把这句话记成 Coder 这一轮该看的（停止之后才到房间的话会另外排队）。
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(t.rig.orchestrator.stop_bot(&t.rig.room, &t.coder));
    t.rig.until_idle().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let all = t.rig.messages();
    assert!(!all.iter().any(|m| m.content == "不该再说"), "{all:#?}");
    assert!(t.rig.activity().speakers.is_empty(), "房间应该空着");
    t.rig.orchestrator.shutdown().await;
}

/// 人中途补了一句、由正在说话的 Bot 接着答：算新的一轮，之前用掉的上限不再算数。
#[tokio::test]
async fn a_follow_up_read_by_the_speaker_resets_the_limits() {
    let dir = tempfile::tempdir().unwrap();
    let t = team(dir.path(), vec![], vec![]);
    t.script(
        "Coder",
        vec![
            // 第一句话：Coder 和 Reviewer 互相找满 2 次。
            ask("c1", "Reviewer", "先看第一版"),
            MockReply::text("交给 Reviewer"),
            // Coder 第二轮停在审批卡上；人补了一句之后它再找 Reviewer，应该被允许。
            MockReply::tool_calls("", &[("c2", "shell", r#"{"command":"echo hi"}"#)]),
            ask("c3", "Reviewer", "用户有新要求，请再看一遍"),
            MockReply::text("已经转给 Reviewer"),
        ],
    );
    t.script(
        "Reviewer",
        vec![
            ask("r1", "Coder", "看过了，请确认"),
            MockReply::text("请 Coder 确认"),
            MockReply::text("按新要求看过了"),
        ],
    );
    t.rig.say("@Coder 开始吧").await;
    let approval = t.rig.waiting_approval().await;
    t.rig.say("补充一下：还要兼容旧版本").await;
    assert!(t.rig.orchestrator.decide(&approval, ApprovalDecision::Once));
    t.rig.until_idle().await;

    let all = t.rig.messages();
    let asked = all
        .iter()
        .find(|m| m.content == "@Reviewer 用户有新要求，请再看一遍")
        .unwrap_or_else(|| panic!("第三次找 Reviewer 应该发得出去：{all:#?}"));
    // 人说了新的话，转手次数也从头算。
    assert_eq!(asked.hop, 1);
    assert!(
        all.iter().any(|m| m.content == "按新要求看过了"),
        "{all:#?}"
    );
    t.rig.orchestrator.shutdown().await;
}

/// 「停止」：所有正在说的都停下，排着队的也不再说。重启之后已经处理过的消息不会被再答一遍。
#[tokio::test]
async fn stop_ends_the_whole_exchange_and_a_restart_does_not_replay_it() {
    let dir = tempfile::tempdir().unwrap();
    {
        let t = team(dir.path(), vec![], vec![]);
        let coder = t.script(
            "Coder",
            vec![
                ask("c1", "Reviewer", "帮我看看"),
                MockReply::Hang(vec![StreamEvent::TextDelta("还在写".into())]),
            ],
        );
        let reviewer = t.script(
            "Reviewer",
            vec![MockReply::Hang(vec![StreamEvent::TextDelta(
                "我也在写".into(),
            )])],
        );
        t.rig.say("@Coder 开始").await;
        for _ in 0..500 {
            let all = t.rig.messages();
            if all.iter().any(|m| m.content == "还在写")
                && all.iter().any(|m| m.content == "我也在写")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(t.rig.orchestrator.stop(&t.rig.room));
        t.rig.until_idle().await;
        let all = t.rig.messages();
        assert_eq!(all.len(), 4, "{all:#?}");
        assert_eq!(all[1].state, MessageState::Cancelled);
        assert_eq!(all[3].state, MessageState::Cancelled);
        assert_eq!((coder.requests().len(), reviewer.requests().len()), (2, 1));
        // 没有东西在跑时，停止什么都不做。
        assert!(!t.rig.orchestrator.stop(&t.rig.room));
        t.rig.orchestrator.shutdown().await;
    }

    let t = team(dir.path(), vec![], vec![MockReply::text("新的回答")]);
    t.rig.say("@Reviewer 现在呢").await;
    let all = t.rig.settled(6).await;
    assert_eq!(sender_of(&all[5]), Some(&t.reviewer));
    assert_eq!(t.rig.provider.requests().len(), 1, "只回答新的这一条");
    t.rig.until_idle().await;
    assert_eq!(t.rig.messages().len(), 6);
    t.rig.orchestrator.shutdown().await;
}

/// 关掉时还排着队的 GitHub 事件：正在说的 Bot 读过去了（这件事不是给它的），重启后照样交给指定的 Bot。
#[tokio::test]
async fn a_queued_event_another_bot_read_past_is_answered_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let t = team_with(dir.path(), vec![], vec![], 1);
        t.rig.engine.link_repo(&t.rig.room, "me/app").unwrap();
        let coder = t.script(
            "Coder",
            vec![
                shell("c1", "echo coder"),
                MockReply::Hang(vec![StreamEvent::TextDelta("还在写".into())]),
            ],
        );
        t.rig.say("@Coder 跑一条命令").await;
        let cards = t.rig.waiting_approvals(1).await;
        let event = Incoming {
            source: Source::Github,
            key: "d-1".into(),
            name: "issues.opened".into(),
            external: "me/app".into(),
            label: "me/app#12".into(),
            url: None,
            kind: IncomingKind::Event {
                summary: "alice 开了 issue #12".into(),
                body: "点登录就 500".into(),
                pr: None,
                branch: None,
                task_only: false,
                notify: true,
            },
        };
        t.rig.orchestrator.receive(event).await.unwrap();
        // 一次只能一个 Bot 说：入口 Bot 排着队。Coder 接着往下做，读过了这件事。
        assert!(t
            .rig
            .orchestrator
            .decide(&cards[0].1, ApprovalDecision::Once));
        for _ in 0..500 {
            if coder.requests().len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(coder.requests().len(), 2);
        t.rig.orchestrator.shutdown().await;
    }

    let t = team_with(dir.path(), vec![], vec![], 1);
    let host = t.script("助手", vec![MockReply::text("看到 issue 了")]);
    assert_eq!(t.rig.orchestrator.resume_rooms().unwrap(), 1);
    t.rig
        .until("入口 Bot 答了", |m| {
            m.content == "看到 issue 了" && m.state == MessageState::Complete
        })
        .await;
    assert!(heard(&host)
        .iter()
        .any(|h| h.contains("alice 开了 issue #12")));
    t.rig.orchestrator.shutdown().await;
}

/// 同上，但关之前人点了停止：排着队的那件事不用答了，重启之后也不答。
#[tokio::test]
async fn a_queued_event_stopped_before_shutdown_is_not_answered_after_restart() {
    stopped_event_is_not_answered_after_restart(true).await;
}

/// 同上，点了停止马上就关：停止还没来得及记下也一样。
#[tokio::test]
async fn a_queued_event_stopped_right_at_shutdown_is_not_answered_after_restart() {
    stopped_event_is_not_answered_after_restart(false).await;
}

/// `settle`：停止之后等房间收好尾再关。
async fn stopped_event_is_not_answered_after_restart(settle: bool) {
    let dir = tempfile::tempdir().unwrap();
    {
        let t = team_with(dir.path(), vec![], vec![], 1);
        t.rig.engine.link_repo(&t.rig.room, "me/app").unwrap();
        let coder = t.script(
            "Coder",
            vec![
                shell("c1", "echo coder"),
                MockReply::Hang(vec![StreamEvent::TextDelta("还在写".into())]),
            ],
        );
        t.rig.say("@Coder 跑一条命令").await;
        let cards = t.rig.waiting_approvals(1).await;
        let event = Incoming {
            source: Source::Github,
            key: "d-1".into(),
            name: "issues.opened".into(),
            external: "me/app".into(),
            label: "me/app#12".into(),
            url: None,
            kind: IncomingKind::Event {
                summary: "alice 开了 issue #12".into(),
                body: "点登录就 500".into(),
                pr: None,
                branch: None,
                task_only: false,
                notify: true,
            },
        };
        t.rig.orchestrator.receive(event).await.unwrap();
        // 一次只能一个 Bot 说：入口 Bot 排着队。Coder 接着往下做，读过了这件事。
        assert!(t
            .rig
            .orchestrator
            .decide(&cards[0].1, ApprovalDecision::Once));
        for _ in 0..500 {
            if coder.requests().len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(coder.requests().len(), 2);
        assert!(t.rig.orchestrator.stop(&t.rig.room));
        for _ in 0..500 {
            if !settle {
                break;
            }
            let messages = t.rig.messages();
            if messages
                .iter()
                .any(|m| m.content == "还在写" && m.state == MessageState::Cancelled)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if settle {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        t.rig.orchestrator.shutdown().await;
    }

    let t = team_with(dir.path(), vec![], vec![], 1);
    let host = t.script("助手", vec![MockReply::text("看到 issue 了")]);
    t.rig.orchestrator.resume_rooms().unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(host.requests().is_empty());
    t.rig.orchestrator.shutdown().await;
}

/// 一句话点了两个 Bot，都排着队；人只停了其中一个就关了。重启后另一个照样答。
#[tokio::test]
async fn a_mentioned_bot_still_queued_answers_after_restart_when_another_was_stopped() {
    let dir = tempfile::tempdir().unwrap();
    {
        let t = team_with(dir.path(), vec![], vec![], 1);
        t.script("Coder", vec![shell("c1", "echo coder")]);
        t.rig.say("@Coder 跑一条命令").await;
        t.rig.waiting_approvals(1).await;
        t.rig.say("@Reviewer @助手 看看").await;
        for _ in 0..500 {
            if t.rig.orchestrator.stop_bot(&t.rig.room, &t.reviewer) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        t.rig.orchestrator.shutdown().await;
    }

    let t = team_with(dir.path(), vec![], vec![], 1);
    let host = t.script("助手", vec![MockReply::text("看过了")]);
    let reviewer = t.script("Reviewer", vec![MockReply::text("我也看过了")]);
    assert_eq!(t.rig.orchestrator.resume_rooms().unwrap(), 1);
    t.rig
        .until("入口 Bot 答了", |m| {
            m.content == "看过了" && m.state == MessageState::Complete
        })
        .await;
    t.rig.until_idle().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(host.requests().len(), 1);
    assert!(reviewer.requests().is_empty(), "停下的那个不再答");
    t.rig.orchestrator.shutdown().await;
}

/// 同上，但之后人又说了一句：之前为上一句排着的不算了，重启后也不答上一句。
#[tokio::test]
async fn a_mention_superseded_before_shutdown_is_not_answered_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let t = team_with(dir.path(), vec![], vec![], 1);
        t.script("Coder", vec![shell("c1", "echo coder")]);
        t.rig.say("@Coder 跑一条命令").await;
        t.rig.waiting_approvals(1).await;
        t.rig.say("@Reviewer @助手 看看").await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        t.rig.say("@Reviewer 换个事").await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(t.rig.orchestrator.stop_bot(&t.rig.room, &t.reviewer));
        tokio::time::sleep(Duration::from_millis(100)).await;
        t.rig.orchestrator.shutdown().await;
    }

    let t = team_with(dir.path(), vec![], vec![], 1);
    let host = t.script("助手", vec![MockReply::text("看过了")]);
    let reviewer = t.script("Reviewer", vec![MockReply::text("我也看过了")]);
    t.rig.orchestrator.resume_rooms().unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(host.requests().is_empty(), "上一句已经不算了");
    assert!(reviewer.requests().is_empty(), "停下的那个不再答");
    t.rig.orchestrator.shutdown().await;
}

/// 关掉时还没人看的定时任务消息：重启后别的话叫醒了房间，也不答它（调度器在租约过期后会重做）。
#[tokio::test]
async fn an_unfinished_routine_is_left_to_the_scheduler_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let t = team(dir.path(), vec![], vec![]);
        let routine = t
            .rig
            .engine
            .create_routine(&t.rig.room, None, "早报", "看看有什么新消息", "0 9 * * *")
            .unwrap();
        let until = sa_engine::now_ms() + 60_000;
        assert!(t
            .rig
            .engine
            .claim_routine(&routine.id, "old", until)
            .unwrap());
        t.rig
            .engine
            .post_routine_message(&routine.id, "old")
            .unwrap();
        t.rig.orchestrator.shutdown().await;
    }

    tokio::time::sleep(Duration::from_millis(5)).await;
    let t = team(dir.path(), vec![], vec![]);
    let host = t.script("助手", vec![MockReply::text("在")]);
    let coder = t.script("Coder", vec![MockReply::text("你好")]);
    t.rig.say("@Coder 你好").await;
    t.rig
        .until("Coder 答了", |m| {
            m.content == "你好" && m.state == MessageState::Complete
        })
        .await;
    t.rig.until_idle().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(coder.requests().len(), 1);
    assert!(host.requests().is_empty());
    // 叫到负责它的 Bot 时，它也看不到那条旧的。
    t.rig.say("@助手 在吗").await;
    t.rig
        .until("入口 Bot 答了", |m| {
            m.content == "在" && m.state == MessageState::Complete
        })
        .await;
    assert_eq!(host.requests().len(), 1);
    let heard = heard(&host);
    assert!(heard.iter().any(|h| h.contains("在吗")));
    assert!(!heard.iter().any(|h| h.contains("看看有什么新消息")));
    t.rig.orchestrator.shutdown().await;
}

/// 一句话点了两个 Bot：两个同时开始说，房间动态里两个都在说。
#[tokio::test]
async fn two_mentioned_bots_answer_the_same_message_together() {
    let dir = tempfile::tempdir().unwrap();
    let t = team(dir.path(), vec![], vec![]);
    let hang = |text: &str| MockReply::Hang(vec![StreamEvent::TextDelta(text.into())]);
    t.script("Coder", vec![hang("Coder 在写")]);
    t.script("Reviewer", vec![hang("Reviewer 在写")]);
    t.rig.say("@Coder @Reviewer 一起看看").await;

    let writing = t.rig.until_writing(2).await;
    let mut senders: Vec<_> = writing.iter().filter_map(sender_of).cloned().collect();
    senders.sort_by_key(|id| id.to_string());
    let mut both = vec![t.coder.clone(), t.reviewer.clone()];
    both.sort_by_key(|id| id.to_string());
    assert_eq!(senders, both);
    let mut speakers = t.rig.activity().speakers;
    speakers.sort_by_key(|id| id.to_string());
    assert_eq!(speakers, both);

    assert!(t.rig.orchestrator.stop(&t.rig.room));
    t.rig.until_idle().await;
    let all = t.rig.messages();
    assert!(
        all[1..].iter().all(|m| m.state == MessageState::Cancelled),
        "{all:#?}"
    );
    assert!(t.router.requests().is_empty());
    t.rig.orchestrator.shutdown().await;
}

/// 同时说话的有上限：多的排队，前面的说完（或被单独停下）就轮到它。单独停一个不影响别的。
#[tokio::test]
async fn bots_over_the_limit_wait_and_stopping_one_lets_the_next_start() {
    let dir = tempfile::tempdir().unwrap();
    let t = team_with(dir.path(), vec![], vec![], 1);
    t.script(
        "Coder",
        vec![MockReply::Hang(vec![StreamEvent::TextDelta(
            "Coder 在写".into(),
        )])],
    );
    t.script("Reviewer", vec![MockReply::text("轮到我了")]);
    t.rig.say("@Coder @Reviewer 都说说").await;
    t.rig.until_writing(1).await;
    for _ in 0..500 {
        if t.rig.activity().queued == vec![t.reviewer.clone()] {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let activity = t.rig.activity();
    assert_eq!(activity.speakers, vec![t.coder.clone()]);
    assert_eq!(activity.queued, vec![t.reviewer.clone()]);

    assert!(t.rig.orchestrator.stop_bot(&t.rig.room, &t.coder));
    t.rig.until_idle().await;
    let all = t.rig.messages();
    assert_eq!(sender_of(&all[1]), Some(&t.coder));
    assert_eq!(all[1].state, MessageState::Cancelled);
    assert_eq!(sender_of(&all[2]), Some(&t.reviewer));
    assert_eq!(
        (all[2].content.as_str(), all[2].state),
        ("轮到我了", MessageState::Complete)
    );
    // 停一个不在说话的 Bot：什么都不做。
    assert!(!t.rig.orchestrator.stop_bot(&t.rig.room, &t.coder));
    t.rig.orchestrator.shutdown().await;
}

/// 两个 Bot 各自等批准，互不耽误；没点名的插话两个都看到，点名的只给被点到的。
#[tokio::test]
async fn approvals_are_separate_and_unaddressed_follow_ups_reach_every_speaker() {
    let dir = tempfile::tempdir().unwrap();
    let t = team(dir.path(), vec![], vec![]);
    let coder = t.script(
        "Coder",
        vec![
            MockReply::tool_calls("", &[("c1", "shell", r#"{"command":"echo coder"}"#)]),
            MockReply::text("Coder 做完了"),
        ],
    );
    let reviewer = t.script(
        "Reviewer",
        vec![
            MockReply::tool_calls("", &[("r1", "shell", r#"{"command":"echo reviewer"}"#)]),
            MockReply::text("Reviewer 做完了"),
        ],
    );
    t.rig.say("@Coder @Reviewer 各跑一条命令").await;
    let cards = t.rig.waiting_approvals(2).await;
    t.rig.say("补充：输出里带上日期").await;
    t.rig.say("@Coder 只跟你说").await;

    // 先批 Reviewer 的：Coder 还在等，Reviewer 照常做完。
    let card = |bot: &BotId| cards.iter().find(|(b, _)| b == bot).unwrap().1.clone();
    assert!(t
        .rig
        .orchestrator
        .decide(&card(&t.reviewer), ApprovalDecision::Once));
    t.rig
        .until("Reviewer 做完", |_| {
            t.rig
                .messages()
                .iter()
                .any(|m| m.content == "Reviewer 做完了" && m.state == MessageState::Complete)
        })
        .await;
    assert_eq!(
        t.rig.waiting_approvals(1).await.len(),
        1,
        "Coder 的卡还在等"
    );
    assert!(t
        .rig
        .orchestrator
        .decide(&card(&t.coder), ApprovalDecision::Once));
    t.rig.until_idle().await;

    let coder_heard = heard(&coder);
    assert!(
        coder_heard
            .iter()
            .any(|h| h == "【用户】补充：输出里带上日期"),
        "{coder_heard:?}"
    );
    assert!(
        coder_heard
            .iter()
            .any(|h| h == "【用户（对你说）】@Coder 只跟你说"),
        "{coder_heard:?}"
    );
    let reviewer_heard = heard(&reviewer);
    assert!(
        reviewer_heard
            .iter()
            .any(|h| h == "【用户】补充：输出里带上日期"),
        "{reviewer_heard:?}"
    );
    assert!(
        !reviewer_heard.iter().any(|h| h.contains("只跟你说")),
        "{reviewer_heard:?}"
    );
    // 插话都在这一轮里看到了，不会再另外答一遍。
    assert_eq!((coder.requests().len(), reviewer.requests().len()), (2, 2));
    let all = t.rig.messages();
    let replies = all.iter().filter(|m| sender_of(m).is_some()).count();
    assert_eq!(replies, 2, "{all:#?}");
    t.rig.orchestrator.shutdown().await;
}

/// 人说一句话最多引出 6 次 Bot 发言：第 6 个 Bot 想再找人时被拒绝。
#[tokio::test]
async fn one_human_message_gets_at_most_six_bot_turns() {
    let dir = tempfile::tempdir().unwrap();
    let probe = team(dir.path(), vec![], vec![]);
    // 再拉三个 Bot 进来，凑够一条不重复的链。
    for name in ["D", "E", "F"] {
        let bot = probe
            .rig
            .engine
            .create_bot(name, ModelTier::Mid, "")
            .unwrap();
        probe
            .rig
            .engine
            .add_room_member(&probe.rig.room, &bot.id)
            .unwrap();
    }
    probe.rig.orchestrator.shutdown().await;
    drop(probe);

    // 每个 Bot 一次找两个人，绕开「转手次数」的上限，只看总次数。
    let fan = |id: &str, a: &str, b: &str| {
        MockReply::tool_calls(
            "",
            &[
                (
                    &format!("{id}a"),
                    "ask_bot",
                    &format!(r#"{{"bot":"{a}","message":"请处理 {id} 的甲部分"}}"#),
                ),
                (
                    &format!("{id}b"),
                    "ask_bot",
                    &format!(r#"{{"bot":"{b}","message":"请处理 {id} 的乙部分"}}"#),
                ),
            ],
        )
    };
    let bots = vec![
        fan("k1", "Reviewer", "D"),
        MockReply::text("分下去了"),
        fan("k2", "E", "F"),
        MockReply::text("我也分下去了"),
        fan("k3", "助手", "E"),
        MockReply::text("D 说完"),
        MockReply::text("E 说完"),
        MockReply::text("F 说完"),
        MockReply::text("助手说完"),
        MockReply::text("不会用到"),
    ];
    // 一次只让一个 Bot 说，次序固定，只看总次数。
    let t = team_with(dir.path(), vec![], bots, 1);
    t.rig.say("@Coder 把活分一下").await;
    t.rig.until_idle().await;

    // Coder、Reviewer、D 说过、E、F 排着是 5 次；D 找助手是第 6 次，再找 E 时没有名额了。
    let spoken: Vec<_> = t
        .rig
        .messages()
        .into_iter()
        .filter(|m| sender_of(m).is_some() && m.mentions.is_empty())
        .collect();
    assert_eq!(spoken.len(), sa_orchestrator::MAX_BOT_TURNS, "{spoken:#?}");
    let refused = t
        .rig
        .provider
        .requests()
        .iter()
        .flat_map(|r| r.messages.clone())
        .filter(|m| m.role == sa_llm::Role::Tool && m.content.contains("发言 6 次"))
        .count();
    assert!(refused >= 1, "应该有一次因为到了上限被拒绝");
    t.rig.orchestrator.shutdown().await;
}

fn heard_tools(provider: &MockProvider) -> Vec<String> {
    let requests = provider.requests();
    requests
        .last()
        .unwrap()
        .messages
        .iter()
        .filter(|m| m.role == sa_llm::Role::Tool)
        .map(|m| m.content.clone())
        .collect()
}

/// 三个 Bot 轮着说同一句话：次数上限还没到，看门狗先发现在转圈，停下这个房间里的对话。
#[tokio::test]
async fn the_watchdog_stops_bots_that_keep_saying_the_same_thing() {
    let dir = tempfile::tempdir().unwrap();
    let t = team(
        dir.path(),
        vec![],
        vec![
            ask("a1", "Reviewer", "你来看看"),
            MockReply::text("收到，我转给别人。"),
            ask("a2", "助手", "登录页的事交给你"),
            MockReply::text("收到，我转给别人。"),
            ask("a3", "Coder", "还是得你来改"),
            MockReply::text("收到， 我转给别人。"),
            MockReply::Hang(vec![]),
        ],
    );
    t.rig.say("@coder 看一下登录页").await;
    for _ in 0..500 {
        if t.rig.provider.requests().len() >= 7 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        t.rig.provider.requests().len(),
        7,
        "{:#?}",
        heard_tools(&t.rig.provider)
    );

    let found = t.rig.orchestrator.watchdog_tick().await;
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].kind, sa_protocol::NoticeKind::Loop);
    assert_eq!(found[0].room_id.as_ref(), Some(&t.rig.room));

    // 正在说话的那个被停下，房间安静下来；再看一遍没有新的提醒。
    // 几个 Bot 同时在说，被停下的那条不一定排在最后。
    let messages = t.rig.settled(5).await;
    let cancelled = messages
        .iter()
        .filter(|m| m.state == MessageState::Cancelled)
        .count();
    assert_eq!(cancelled, 1, "{messages:#?}");
    assert!(t.rig.orchestrator.watchdog_tick().await.is_empty());
    t.rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn queued_words_wait_until_the_room_is_free_and_go_out_one_by_one() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(
        dir.path(),
        vec![
            MockReply::Hang(vec![StreamEvent::TextDelta("写到一半".into())]),
            MockReply::text("回二"),
            MockReply::text("回三"),
        ],
        true,
    );
    rig.say("一").await;
    rig.until("回复开始出字", |m| m.content == "写到一半").await;
    let queue = rig
        .orchestrator
        .queue_message(&rig.room, "二".into())
        .await
        .unwrap();
    assert_eq!(queue.items.len(), 1);
    rig.orchestrator
        .queue_message(&rig.room, "三".into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(rig.messages().len(), 2, "Bot 还在说，排着的不发");
    assert_eq!(rig.provider.requests().len(), 1);

    // 停止只停正在说的，排着的照样一句一句发。
    assert!(rig.orchestrator.stop(&rig.room));
    let messages = rig.settled(6).await;
    let said: Vec<_> = messages.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(said, ["一", "写到一半", "二", "回二", "三", "回三"]);
    let second = &rig.provider.requests()[1];
    assert_eq!(
        second.messages.last().unwrap().content,
        "二",
        "回二时还没看到三"
    );
    assert!(rig.engine.list_queue(&rig.room).unwrap().items.is_empty());
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn words_still_queued_at_shutdown_go_out_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(dir.path(), vec![MockReply::text("接着回")], true);
    rig.engine.queue_input(&rig.room, "关之前排的").unwrap();
    assert_eq!(rig.orchestrator.resume_rooms().unwrap(), 1);
    let messages = rig.settled(2).await;
    let said: Vec<_> = messages.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(said, ["关之前排的", "接着回"]);
    rig.orchestrator.shutdown().await;
}

impl Team {
    fn discuss(&self, discussion: sa_protocol::Discussion, rounds: u32) {
        let edit = sa_protocol::UpdateRoomRequest {
            discussion: Some(discussion),
            rounds: Some(rounds),
            ..Default::default()
        };
        self.rig.engine.edit_room(&self.rig.room, &edit).unwrap();
    }
}

/// 并行回复：没点名的话每个 Bot 各答一份，不问路由。点了名的照旧只给被点到的。
#[tokio::test]
async fn in_parallel_mode_every_bot_answers_an_unaddressed_message() {
    let dir = tempfile::tempdir().unwrap();
    let t = team(dir.path(), vec![], vec![]);
    t.discuss(sa_protocol::Discussion::Parallel, 2);
    t.script("助手", vec![MockReply::text("助手的看法")]);
    t.script(
        "Coder",
        vec![MockReply::text("Coder 的看法"), MockReply::text("只有我")],
    );
    t.script("Reviewer", vec![MockReply::text("Reviewer 的看法")]);
    t.rig.say("大家怎么看").await;
    let all = t.rig.settled(4).await;
    let mut said: Vec<&str> = all[1..].iter().map(|m| m.content.as_str()).collect();
    said.sort();
    assert_eq!(said, ["Coder 的看法", "Reviewer 的看法", "助手的看法"]);
    assert!(t.router.requests().is_empty());

    t.rig.say("@Coder 你再说说").await;
    let all = t.rig.settled(6).await;
    assert_eq!(all.len(), 6, "{all:#?}");
    assert_eq!(sender_of(&all[5]), Some(&t.coder));
    t.rig.orchestrator.shutdown().await;
}

/// 轮流讨论：按进房间的先后一个接一个说，转两圈；后说的看得到先说的。
#[tokio::test]
async fn in_rounds_mode_bots_speak_one_after_another_for_each_round() {
    let dir = tempfile::tempdir().unwrap();
    let t = team(dir.path(), vec![], vec![]);
    t.discuss(sa_protocol::Discussion::Rounds, 2);
    t.script(
        "助手",
        vec![MockReply::text("助手一"), MockReply::text("助手二")],
    );
    t.script(
        "Coder",
        vec![MockReply::text("Coder 一"), MockReply::text("Coder 二")],
    );
    let reviewer = t.script(
        "Reviewer",
        vec![
            MockReply::text("Reviewer 一"),
            MockReply::text("Reviewer 二"),
        ],
    );
    t.rig.say("讨论一下方案").await;
    let all = t.rig.settled(7).await;
    let said: Vec<&str> = all[1..].iter().map(|m| m.content.as_str()).collect();
    assert_eq!(
        said,
        [
            "助手一",
            "Coder 一",
            "Reviewer 一",
            "助手二",
            "Coder 二",
            "Reviewer 二"
        ]
    );
    let first = &reviewer.requests()[0];
    let heard: Vec<&str> = first.messages.iter().map(|m| m.content.as_str()).collect();
    assert!(heard.iter().any(|m| m.contains("Coder 一")), "{heard:#?}");
    assert!(t.router.requests().is_empty());
    t.rig.orchestrator.shutdown().await;
}

/// 独立评议：一次只能一个 Bot 说时，后说的也看不到前面的答案；人再说话时才看得到。
#[tokio::test]
async fn in_blind_mode_bots_do_not_see_each_others_answers_until_the_next_message() {
    let dir = tempfile::tempdir().unwrap();
    let t = team_with(dir.path(), vec![], vec![], 1);
    t.discuss(sa_protocol::Discussion::Blind, 2);
    t.script("助手", vec![MockReply::text("助手打 7 分")]);
    t.script("Coder", vec![MockReply::text("Coder 打 8 分")]);
    let reviewer = t.script(
        "Reviewer",
        vec![
            MockReply::text("Reviewer 打 6 分"),
            MockReply::text("看到了"),
        ],
    );
    t.rig.say("给这个方案打分").await;
    t.rig.settled(4).await;
    let first = &reviewer.requests()[0];
    let heard: Vec<&str> = first.messages.iter().map(|m| m.content.as_str()).collect();
    assert!(
        heard.iter().any(|m| m.contains("给这个方案打分")),
        "{heard:#?}"
    );
    assert!(
        !heard
            .iter()
            .any(|m| m.contains("打 7 分") || m.contains("打 8 分")),
        "{heard:#?}"
    );

    t.rig.say("@Reviewer 看看别人怎么打的").await;
    t.rig.settled(6).await;
    let second = &reviewer.requests()[1];
    let heard: Vec<&str> = second.messages.iter().map(|m| m.content.as_str()).collect();
    assert!(heard.iter().any(|m| m.contains("打 7 分")), "{heard:#?}");
    assert!(heard.iter().any(|m| m.contains("打 8 分")), "{heard:#?}");
    t.rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn in_blind_mode_a_handoff_is_seen_but_the_answers_stay_hidden() {
    let dir = tempfile::tempdir().unwrap();
    let t = team_with(dir.path(), vec![], vec![], 1);
    t.discuss(sa_protocol::Discussion::Blind, 2);
    let host = t.script(
        "助手",
        vec![
            MockReply::text("助手打 7 分"),
            MockReply::text("看到了交接"),
        ],
    );
    let coder = t.script(
        "Coder",
        vec![
            ask("c1", "助手", "请再看一眼性能"),
            MockReply::text("Coder 打 8 分"),
        ],
    );
    t.script("Reviewer", vec![MockReply::text("Reviewer 打 6 分")]);
    t.rig.say("给这个方案打分").await;
    t.rig.settled(5).await;
    t.rig.until_idle().await;

    // 轮到 Coder 时助手已经答完了：它看不到。
    let coder_heard: Vec<String> = coder.requests()[0]
        .messages
        .iter()
        .map(|m| m.content.clone())
        .collect();
    assert!(
        coder_heard.iter().all(|m| !m.contains("打 7 分")),
        "{coder_heard:#?}"
    );
    // 助手已经评过了，被 Coder 找回来：看得到这句交接，看不到别人的分数。
    let requests = host.requests();
    assert_eq!(requests.len(), 2);
    let heard: Vec<&str> = requests[1]
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect();
    assert!(
        heard.iter().any(|m| m.contains("请再看一眼性能")),
        "{heard:#?}"
    );
    assert!(
        !heard
            .iter()
            .any(|m| m.contains("打 8 分") || m.contains("打 6 分")),
        "{heard:#?}"
    );
    t.rig.orchestrator.shutdown().await;
}
