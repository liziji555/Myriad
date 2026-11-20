//! 端到端：真实的 Engine（SQLite 文件）+ 会话记录文件 + 脚本化的假模型。

use sa_engine::Engine;
use sa_llm::{LlmError, MockProvider, MockReply, StopReason, StreamEvent};
use sa_orchestrator::{ModelRouter, Orchestrator, ResolvedModel, Settings, SingleModel};
use sa_protocol::{
    ApprovalDecision, ApprovalId, Message, MessageState, ModelTier, RoomId, Sender, StepState,
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
            "todo"
        ]
    );
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

/// SuperAgent 自己的数据目录：不问，直接不让写。
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

/// 压缩不了又放不下：回复标成失败，原因写清楚，房间还能继续用。
#[tokio::test]
async fn context_limit_is_reported_on_the_message() {
    let dir = tempfile::tempdir().unwrap();
    let replies = vec![MockReply::Fail(sa_llm::LlmError::BadRequest("太长".into()))];
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
