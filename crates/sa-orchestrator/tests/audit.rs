//! 审计日志：Bot 动了手的事、人做了主的事都留了底。

use sa_engine::{AuditFilter, Engine};
use sa_llm::{MockProvider, MockReply};
use sa_orchestrator::{ModelRouter, Orchestrator, ResolvedModel, Settings};
use sa_protocol::{
    ApprovalDecision, AuditActor, Message, MessageState, ModelTier, RoomId, StepState,
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
    #[allow(dead_code)]
    mock: Arc<MockProvider>,
    room: RoomId,
}

fn boot(dir: &Path, replies: Vec<MockReply>) -> Rig {
    boot_with(dir, Duration::from_secs(600), replies)
}

fn boot_with(dir: &Path, approval_timeout: Duration, replies: Vec<MockReply>) -> Rig {
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
        approval_timeout,
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
    async fn until(&self, what: &str, ready: impl Fn(&Message) -> bool) -> Message {
        for _ in 0..400 {
            let page = self.engine.list_messages(&self.room, None, 100).unwrap();
            if let Some(message) = page.messages.last().filter(|m| ready(m)) {
                return message.clone();
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("等了 10 秒也没等到：{what}");
    }

    /// 等到有一步在等批准，答复它。
    async fn decide(&self, decision: ApprovalDecision) {
        let waiting = self
            .until("有一步在等批准", |m| {
                m.steps.iter().any(|step| step.state == StepState::Waiting)
            })
            .await;
        let step = waiting
            .steps
            .iter()
            .find(|step| step.state == StepState::Waiting);
        let id = step.unwrap().approval.clone().unwrap().id;
        assert!(self.orchestrator.decide(&id, decision));
    }
}

#[tokio::test]
async fn commands_and_approvals_are_on_record_and_reading_is_not() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("ws")).unwrap();
    std::fs::write(dir.path().join("ws/说明.md"), "项目代号海豚").unwrap();
    let rig = boot(
        dir.path(),
        vec![
            MockReply::tool_calls(
                "",
                &[
                    ("c1", "read", r#"{"path":"说明.md"}"#),
                    ("c2", "shell", r#"{"command":"echo 一"}"#),
                ],
            ),
            MockReply::tool_calls("", &[("c3", "shell", r#"{"command":"echo 二"}"#)]),
            MockReply::text("做完了。"),
        ],
    );
    rig.orchestrator
        .post_message(&rig.room, "跑两个命令".into())
        .await
        .unwrap();
    rig.decide(ApprovalDecision::Once).await;
    rig.until("第二个命令开始等", |m| {
        m.steps.len() == 3 && m.steps[2].state == StepState::Waiting
    })
    .await;
    rig.decide(ApprovalDecision::Deny).await;
    rig.until("回复写完", |m| {
        m.state == MessageState::Complete && m.content == "做完了。"
    })
    .await;

    let page = rig.engine.list_audit(&AuditFilter::default(), 50).unwrap();
    let mut entries = page.entries;
    entries.reverse();
    let actions: Vec<&str> = entries.iter().map(|e| e.action.as_str()).collect();
    // 读文件不记；命令每次都记；人的每个答复都记。
    assert_eq!(
        actions,
        [
            "tool.shell",
            "approval.allowed",
            "tool.shell",
            "approval.denied"
        ]
    );
    let bot = rig.engine.list_bots().unwrap().remove(0);
    assert_eq!(
        (entries[0].actor, entries[0].actor_name.as_str()),
        (AuditActor::Bot, bot.name.as_str())
    );
    assert!(entries[0].summary.contains("echo 一"), "{:?}", entries[0]);
    assert_eq!(entries[1].actor, AuditActor::User);
    assert!(entries[1].summary.contains("允许一次") && entries[1].summary.contains("echo 一"));
    assert!(entries[3].summary.contains("拒绝") && entries[3].summary.contains("echo 二"));
    assert!(entries
        .iter()
        .all(|e| e.room_id.as_ref() == Some(&rig.room) && e.task_id.is_none()));
    // 按房间、按动作都找得到。
    let approvals = AuditFilter {
        room_id: Some(rig.room.clone()),
        action: Some("approval.".into()),
        ..Default::default()
    };
    assert_eq!(
        rig.engine.list_audit(&approvals, 50).unwrap().entries.len(),
        2
    );
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn an_approval_nobody_answers_is_recorded_as_expired() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = boot(dir.path(), vec![]);
    rig.orchestrator.shutdown().await;
    rig = boot_with(
        dir.path(),
        Duration::from_millis(150),
        vec![
            MockReply::tool_calls("", &[("c1", "shell", r#"{"command":"echo 一"}"#)]),
            MockReply::text("没批准，不做了。"),
        ],
    );
    rig.orchestrator
        .post_message(&rig.room, "跑个命令".into())
        .await
        .unwrap();
    rig.until("回复写完", |m| {
        m.state == MessageState::Complete && m.content == "没批准，不做了。"
    })
    .await;
    let expired = AuditFilter {
        action: Some("approval.expired".into()),
        ..Default::default()
    };
    let entries = rig.engine.list_audit(&expired, 50).unwrap().entries;
    let all = rig.engine.list_audit(&AuditFilter::default(), 50).unwrap();
    assert_eq!(entries.len(), 1, "{:?}", all.entries);
    assert_eq!(entries[0].actor, AuditActor::System);
    assert!(entries[0].summary.contains("到时间没人处理"));
    rig.orchestrator.shutdown().await;
}
