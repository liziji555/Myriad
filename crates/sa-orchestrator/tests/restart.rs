//! 重启恢复：进程退出时还没有 Bot 接的话，重启之后接着回应；回到一半的不重做。
//! 真实的 Engine 和数据库文件，每次「重启」都重新打开。

use sa_engine::Engine;
use sa_llm::{MockProvider, MockReply, StreamEvent};
use sa_orchestrator::{ModelRouter, Orchestrator, ResolvedModel, Settings};
use sa_protocol::{Message, MessageState, ModelTier, RoomId, Sender};
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

/// 启动一次：打开数据库，做启动时的恢复，和 `sa serve` 一样。
fn boot(dir: &Path, replies: Vec<MockReply>) -> Rig {
    let data = dir.join("data");
    let workspace = dir.join("ws");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    let engine = Arc::new(Engine::new(Store::open(&data.join("state.db")).unwrap()).unwrap());
    engine.ensure_defaults().unwrap();
    engine.recover_interrupted().unwrap();
    engine.recover_runs().unwrap();
    let room = engine.list_rooms().unwrap().pop().unwrap();
    let mock = Arc::new(MockProvider::scripted(replies));
    let settings = Settings {
        sessions_dir: data.join("sessions"),
        fence: Fence::new(&workspace, std::slice::from_ref(&data)).unwrap(),
        prompt: None,
        max_steps: 8,
        allow_private_network: false,
        approval_timeout: Duration::from_secs(600),
        work_dir: data.join("work"),
        github: None,
        budget: Default::default(),
        watchdog: Default::default(),
        routines: Default::default(),
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
    fn messages(&self) -> Vec<Message> {
        self.engine
            .list_messages(&self.room, None, 100)
            .unwrap()
            .messages
    }

    async fn until(&self, what: &str, ready: impl Fn(&Message) -> bool) -> Message {
        for _ in 0..400 {
            if let Some(message) = self.messages().pop().filter(|m| ready(m)) {
                return message;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!(
            "等了 10 秒也没等到：{what}。现在是 {:?}",
            self.messages().pop()
        );
    }

    async fn replied(&self, text: &str) {
        self.until(text, |m| {
            m.content == text && m.state == MessageState::Complete
        })
        .await;
    }

    /// 关掉这一次，像进程退出一样。
    async fn stop(self) {
        self.orchestrator.shutdown().await;
    }
}

#[tokio::test]
async fn words_nobody_picked_up_are_answered_after_each_of_three_restarts() {
    let dir = tempfile::tempdir().unwrap();
    for round in 1..=3 {
        // 这句话落了库，还没有 Bot 接，进程就没了。
        let rig = boot(dir.path(), vec![]);
        rig.engine
            .post_user_message(&rig.room, &format!("第 {round} 句"))
            .unwrap();
        rig.stop().await;

        let answer = format!("收到第 {round} 句。");
        let rig = boot(dir.path(), vec![MockReply::text(&answer)]);
        assert_eq!(rig.orchestrator.resume_rooms().unwrap(), 1);
        rig.replied(&answer).await;
        // 前几次重启之前说过的话，Bot 在上下文里都还看得到。
        let request = format!("{:?}", rig.mock.requests()[0]);
        for earlier in 1..=round {
            assert!(request.contains(&format!("第 {earlier} 句")), "{request}");
        }
        rig.stop().await;
    }

    // 再启动一次：没有欠着的话，谁也不出声。
    let rig = boot(dir.path(), vec![]);
    assert_eq!(rig.orchestrator.resume_rooms().unwrap(), 0);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(rig.mock.requests().is_empty());
    // 一句都没丢、没重复，每句后面跟着它的回复。
    let said: Vec<String> = rig.messages().into_iter().map(|m| m.content).collect();
    assert_eq!(
        said,
        [
            "第 1 句",
            "收到第 1 句。",
            "第 2 句",
            "收到第 2 句。",
            "第 3 句",
            "收到第 3 句。"
        ]
    );
    rig.stop().await;
}

#[tokio::test]
async fn a_reply_cut_off_half_way_is_left_interrupted_and_not_redone() {
    let dir = tempfile::tempdir().unwrap();
    let rig = boot(
        dir.path(),
        vec![MockReply::Hang(vec![StreamEvent::TextDelta(
            "写到一半".into(),
        )])],
    );
    rig.orchestrator
        .post_message(&rig.room, "帮我改个文件".into())
        .await
        .unwrap();
    rig.until("回复开始写", |m| {
        m.state == MessageState::Streaming && m.content == "写到一半"
    })
    .await;
    rig.stop().await;

    let rig = boot(dir.path(), vec![MockReply::text("不应该说这句")]);
    // 最后一条是那条中断的回复，不是没人接的话：不重做。
    assert_eq!(rig.orchestrator.resume_rooms().unwrap(), 0);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(rig.mock.requests().is_empty());
    let messages = rig.messages();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].state, MessageState::Interrupted);
    assert_eq!(messages[1].content, "写到一半");

    // 人接着说话，一切照常；上一轮的话和中断的回复都在上下文里。
    rig.orchestrator
        .post_message(&rig.room, "接着来".into())
        .await
        .unwrap();
    rig.replied("不应该说这句").await;
    let request = format!("{:?}", rig.mock.requests()[0]);
    assert!(
        request.contains("帮我改个文件") && request.contains("接着来"),
        "{request}"
    );
    assert!(rig
        .messages()
        .iter()
        .all(|m| m.sender != Sender::User || m.state == MessageState::Complete));
    rig.stop().await;
}
