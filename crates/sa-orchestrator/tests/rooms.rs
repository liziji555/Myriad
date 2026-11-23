//! 端到端：真实的 Engine（SQLite 文件）+ 会话记录文件 + 脚本化的假模型。

use sa_engine::Engine;
use sa_llm::{LlmError, MockProvider, MockReply, StopReason, StreamEvent};
use sa_orchestrator::{ModelRouter, Orchestrator, ResolvedModel, Settings, SingleModel};
use sa_protocol::{
    ApprovalDecision, ApprovalId, BotId, Message, MessageState, ModelTier, RoomId, Sender,
    StepState,
};
use sa_store::Store;
use sa_tools::Fence;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

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

    /// 第 `index` 次发给模型的请求里，最后一条工具结果。
    fn last_tool_result_of(&self, index: usize) -> String {
        self.provider.requests()[index]
            .messages
            .iter()
            .rev()
            .find(|m| m.role == sa_llm::Role::Tool)
            .map(|m| m.content.clone())
            .unwrap_or_default()
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
    assert_eq!(
        (rules[0].tool.as_str(), rules[0].pattern.as_str()),
        ("shell", "echo ok")
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
    bots: Arc<MockProvider>,
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
    host: BotId,
    coder: BotId,
    reviewer: BotId,
}

/// 一个房间，三个 Bot：助手（入口）、Coder、Reviewer。同一个目录再开一次就是重启。
fn team(dir: &Path, router: Vec<MockReply>, bots: Vec<MockReply>) -> Team {
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
        allow_private_network: false,
        approval_timeout: Duration::from_secs(600),
        work_dir: data.join("work"),
        github: None,
        budget: Default::default(),
        watchdog: Default::default(),
        routines: Default::default(),
    };
    let models = Arc::new(Tiered {
        router: router.clone(),
        bots: bots.clone(),
    });
    Team {
        rig: Rig {
            orchestrator: Orchestrator::new(engine.clone(), models, settings),
            engine,
            provider: bots,
            room: room.id,
        },
        router,
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

/// Bot 找 Bot：话带到就走，不等回答；对方的回答是一条新消息，转手次数记在消息上。
#[tokio::test]
async fn ask_bot_hands_over_without_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let bots = vec![
        ask("c1", "@reviewer", "改好了，帮我看看 diff"),
        MockReply::text("我请 Reviewer 看了"),
        MockReply::text("看过了，没问题"),
    ];
    let t = team(dir.path(), vec![], bots);
    t.rig.say("@Coder 修一下 bug").await;
    let all = t.rig.settled(4).await;

    let (reply, asked, answer) = (&all[1], &all[2], &all[3]);
    assert_eq!(sender_of(reply), Some(&t.coder));
    assert_eq!(reply.content, "我请 Reviewer 看了");
    assert_eq!(reply.steps.len(), 1);
    assert_eq!(reply.steps[0].title, "请 reviewer：改好了，帮我看看 diff");
    assert_eq!(reply.steps[0].state, StepState::Ok);
    assert_eq!(reply.steps[0].approval, None, "找别的 Bot 不用人批准");
    // 带的话是房间里看得到的一条消息。
    assert_eq!(sender_of(asked), Some(&t.coder));
    assert_eq!(asked.content, "@Reviewer 改好了，帮我看看 diff");
    assert_eq!(
        (asked.mentions.clone(), asked.hop),
        (vec![t.reviewer.clone()], 1)
    );
    assert_eq!(asked.state, MessageState::Complete);
    assert_eq!(sender_of(answer), Some(&t.reviewer));
    assert_eq!((answer.content.as_str(), answer.hop), ("看过了，没问题", 1));

    assert!(t
        .rig
        .last_tool_result_of(1)
        .contains("已经把话带给 Reviewer"));
    assert_eq!(
        heard(&t.rig.provider),
        [
            "【用户】@Coder 修一下 bug",
            "【Coder】我请 Reviewer 看了",
            "【Coder（对你说）】@Reviewer 改好了，帮我看看 diff"
        ]
    );
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

/// 人点名找另一个 Bot：正在说话的被打断，被点到的来接。
#[tokio::test]
async fn mentioning_another_bot_interrupts_the_speaker() {
    let dir = tempfile::tempdir().unwrap();
    let bots = vec![
        MockReply::Hang(vec![StreamEvent::TextDelta("我慢慢说".into())]),
        MockReply::text("我来接"),
    ];
    let t = team(dir.path(), vec![], bots);
    t.rig.say("@Coder 讲讲").await;
    t.rig
        .until("Coder 开始说", |m| m.content == "我慢慢说")
        .await;
    t.rig.say("@Reviewer 还是你来").await;

    let all = t.rig.settled(4).await;
    assert_eq!(sender_of(&all[1]), Some(&t.coder));
    assert_eq!(all[1].state, MessageState::Cancelled);
    assert_eq!(sender_of(&all[3]), Some(&t.reviewer));
    assert_eq!(all[3].content, "我来接");
    // Reviewer 看到了前面发生的事，包括 Coder 没说完的那半句。
    assert_eq!(
        heard(&t.rig.provider),
        [
            "【用户】@Coder 讲讲",
            "【Coder】我慢慢说",
            "【用户（对你说）】@Reviewer 还是你来"
        ]
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

/// 人中途补了一句、由正在说话的 Bot 接着答：算新的一轮，之前用掉的上限不再算数。
#[tokio::test]
async fn a_follow_up_read_by_the_speaker_resets_the_limits() {
    let dir = tempfile::tempdir().unwrap();
    let bots = vec![
        // 第一句话：Coder 和 Reviewer 互相找满 2 次。
        ask("c1", "Reviewer", "先看第一版"),
        MockReply::text("交给 Reviewer"),
        ask("r1", "Coder", "看过了，请确认"),
        MockReply::text("请 Coder 确认"),
        // Coder 这一轮停在审批卡上；人补了一句之后它再找 Reviewer，应该被允许。
        MockReply::tool_calls("", &[("c2", "shell", r#"{"command":"echo hi"}"#)]),
        ask("c3", "Reviewer", "用户有新要求，请再看一遍"),
        MockReply::text("已经转给 Reviewer"),
        MockReply::text("按新要求看过了"),
    ];
    let t = team(dir.path(), vec![], bots);
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
    assert_eq!(all.last().unwrap().content, "按新要求看过了");
    t.rig.orchestrator.shutdown().await;
}

/// 「停止」：正在说的停下，排着队的也不再说。重启之后已经处理过的消息不会被再答一遍。
#[tokio::test]
async fn stop_ends_the_whole_exchange_and_a_restart_does_not_replay_it() {
    let dir = tempfile::tempdir().unwrap();
    {
        let bots = vec![
            ask("c1", "Reviewer", "帮我看看"),
            MockReply::Hang(vec![StreamEvent::TextDelta("还在写".into())]),
        ];
        let t = team(dir.path(), vec![], bots);
        t.rig.say("@Coder 开始").await;
        t.rig
            .until("Coder 把话带给了 Reviewer", |m| {
                m.content == "@Reviewer 帮我看看"
            })
            .await;
        // 等 Coder 开始写后面的话再停：这时它的第二次模型调用已经发出去了。
        for _ in 0..500 {
            if t.rig.messages().iter().any(|m| m.content == "还在写") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(t.rig.provider.requests().len(), 2);
        assert!(t.rig.orchestrator.stop(&t.rig.room));
        t.rig.until_idle().await;
        let all = t.rig.messages();
        assert_eq!(all.len(), 3, "{all:#?}");
        assert_eq!(all[1].state, MessageState::Cancelled);
        assert_eq!(
            t.rig.provider.requests().len(),
            2,
            "Reviewer 不应该被叫起来"
        );
        // 没有东西在跑时，停止什么都不做。
        assert!(!t.rig.orchestrator.stop(&t.rig.room));
        t.rig.orchestrator.shutdown().await;
    }

    let t = team(dir.path(), vec![], vec![MockReply::text("新的回答")]);
    t.rig.say("@Reviewer 现在呢").await;
    let all = t.rig.settled(5).await;
    assert_eq!(sender_of(&all[4]), Some(&t.reviewer));
    assert_eq!(t.rig.provider.requests().len(), 1, "只回答新的这一条");
    t.rig.until_idle().await;
    assert_eq!(t.rig.messages().len(), 5);
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
        fan("k3", "E", "F"),
        MockReply::text("D 说完"),
        MockReply::text("E 说完"),
        MockReply::text("F 说完"),
        MockReply::text("不会用到"),
    ];
    let t = team(dir.path(), vec![], bots);
    t.rig.say("@Coder 把活分一下").await;
    t.rig.until_idle().await;

    // Coder、Reviewer、D、E、F 各说一次是 5 次，加上排到的第 6 次；D 想再找两个人时名额只剩一个。
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
    let messages = t.rig.settled(5).await;
    assert_eq!(messages.last().unwrap().state, MessageState::Cancelled);
    assert!(t.rig.orchestrator.watchdog_tick().await.is_empty());
    t.rig.orchestrator.shutdown().await;
}
