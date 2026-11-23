//! 故障注入：模型服务以各种方式出毛病之后，房间都要落到一个干净的状态——
//! 没有回复停在「正在写」，没有步骤停在「进行中」，出错的那条写明了原因，下一句话照常有人回。

use sa_engine::Engine;
use sa_llm::{LlmError, MockProvider, MockReply, StreamEvent};
use sa_orchestrator::{ModelRouter, Orchestrator, ResolvedModel, Settings};
use sa_protocol::{Message, MessageState, ModelTier, RoomId, Sender, StepState};
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
        max_parallel_bots: 3,
        max_parallel_harness: sa_orchestrator::DEFAULT_MAX_PARALLEL_HARNESS,
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

    async fn say(&self, text: &str) {
        self.orchestrator
            .post_message(&self.room, text.to_string())
            .await
            .unwrap();
    }

    /// 等到最后一条是 Bot 的、而且已经不在写了。
    async fn settled(&self, what: &str) -> Message {
        for _ in 0..600 {
            let last = self.messages().pop().unwrap();
            if last.sender != Sender::User && last.state != MessageState::Streaming {
                return last;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!(
            "「{what}」之后等了 15 秒，回复还没有着落：{:?}",
            self.messages().pop()
        );
    }
}

/// 写到一半的字。
fn partial() -> Vec<StreamEvent> {
    vec![StreamEvent::TextDelta("写到一半".into())]
}

/// 开了头、参数还没写完的工具调用。
fn torn_tool_call() -> Vec<StreamEvent> {
    let MockReply::Events(mut events) =
        MockReply::tool_calls("我读一下。", &[("c1", "read", r#"{"path":"说明"#)])
    else {
        unreachable!()
    };
    // 去掉收尾：用量和结束标记都没来得及发。
    events.truncate(events.len().saturating_sub(2));
    events
}

struct Fault {
    name: &'static str,
    replies: Vec<MockReply>,
    /// 人要不要点一下停止（模型挂着不动的时候）。
    stop: bool,
    state: MessageState,
    /// 出错的那条里应该写着的原因。
    explains: &'static str,
}

fn faults() -> Vec<Fault> {
    let fault = |name, replies, state, explains| Fault {
        name,
        replies,
        stop: false,
        state,
        explains,
    };
    vec![
        fault(
            "密钥不对",
            vec![MockReply::Fail(LlmError::Auth("HTTP 401".into()))],
            MessageState::Failed,
            "访问凭证",
        ),
        fault(
            "请求不被接受",
            vec![MockReply::Fail(LlmError::BadRequest("HTTP 400".into()))],
            MessageState::Failed,
            "不接受这个请求",
        ),
        fault(
            "返回了看不懂的内容",
            vec![MockReply::Fail(LlmError::Protocol("不是 JSON".into()))],
            MessageState::Failed,
            "看不懂",
        ),
        fault(
            "写到一半服务出错",
            vec![MockReply::FailAfter(
                partial(),
                LlmError::Server("HTTP 502".into()),
            )],
            MessageState::Failed,
            "出错了",
        ),
        fault(
            "写到一半网络断了",
            vec![MockReply::FailAfter(
                partial(),
                LlmError::Network("连接被重置".into()),
            )],
            MessageState::Failed,
            "连不上",
        ),
        fault(
            "工具调用写到一半断了",
            vec![MockReply::FailAfter(
                torn_tool_call(),
                LlmError::Network("连接被重置".into()),
            )],
            MessageState::Failed,
            "连不上",
        ),
        fault(
            "工具调用没写完流就结束了",
            vec![MockReply::Events(torn_tool_call())],
            MessageState::Failed,
            "没有正常结束",
        ),
        fault(
            "调用了不存在的工具",
            vec![
                MockReply::tool_calls("", &[("c1", "no_such_tool", "{}")]),
                MockReply::text("没有这个工具。"),
            ],
            MessageState::Complete,
            "",
        ),
        fault(
            "工具参数不是 JSON",
            vec![
                MockReply::tool_calls("", &[("c1", "read", "{{{")]),
                MockReply::text("参数写错了。"),
            ],
            MessageState::Complete,
            "",
        ),
        fault(
            "先限流一次再好",
            vec![
                MockReply::Fail(LlmError::RateLimited("HTTP 429".into())),
                MockReply::text("这次好了。"),
            ],
            MessageState::Complete,
            "",
        ),
        Fault {
            name: "一直不回应，人点了停止",
            replies: vec![MockReply::Hang(partial())],
            stop: true,
            state: MessageState::Cancelled,
            explains: "",
        },
    ]
}

#[tokio::test]
async fn after_any_model_fault_the_room_is_clean_and_keeps_working() {
    for fault in faults() {
        let dir = tempfile::tempdir().unwrap();
        let mut replies = fault.replies;
        replies.push(MockReply::text("在。"));
        let rig = boot(dir.path(), replies);
        rig.say("做件事").await;
        if fault.stop {
            for _ in 0..200 {
                if rig.orchestrator.stop(&rig.room) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
        let last = rig.settled(fault.name).await;
        assert_eq!(last.state, fault.state, "{}：{last:?}", fault.name);
        if fault.state == MessageState::Failed {
            let error = last.error.clone().unwrap_or_default();
            assert!(error.contains(fault.explains), "{}：{error}", fault.name);
        }

        // 下一句话照常有人回，之前出的事不留尾巴。
        rig.say("还在吗").await;
        let next = rig.settled(fault.name).await;
        assert_eq!(
            (next.content.as_str(), next.state),
            ("在。", MessageState::Complete),
            "{}",
            fault.name
        );
        for message in rig.messages() {
            assert_ne!(message.state, MessageState::Streaming, "{}", fault.name);
            let stuck = |state| matches!(state, StepState::Running | StepState::Waiting);
            assert!(
                message.steps.iter().all(|step| !stuck(step.state)),
                "{}：{message:?}",
                fault.name
            );
        }
        assert_eq!(rig.mock.remaining(), 0, "{}", fault.name);

        // 这时候重启，也没有要恢复的东西。
        rig.orchestrator.shutdown().await;
        let again = boot(dir.path(), vec![]);
        assert_eq!(
            again.orchestrator.resume_rooms().unwrap(),
            0,
            "{}",
            fault.name
        );
        again.orchestrator.shutdown().await;
    }
}
