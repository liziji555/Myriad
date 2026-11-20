//! 端到端：真实的 Engine（SQLite 文件）+ 会话记录文件 + 脚本化的假模型。

use sa_engine::Engine;
use sa_llm::{LlmError, MockProvider, MockReply, StreamEvent};
use sa_orchestrator::{ModelRouter, Orchestrator, ResolvedModel, Settings, SingleModel};
use sa_protocol::{Message, MessageState, ModelTier, RoomId, Sender, StepState};
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
        }))
    } else {
        Arc::new(NoModel)
    };
    let settings = Settings {
        sessions_dir: data.join("sessions"),
        fence: Fence::new(&work, &[data.clone()]).unwrap(),
        prompt: None,
        max_steps: 6,
        allow_private_network: false,
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
    assert_eq!(request.tools.len(), 5);
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
