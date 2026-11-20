//! 端到端：技能和外部 MCP 服务的工具。真实的 Engine、脚本化的假模型、进程内的假 MCP 服务。

use sa_engine::Engine;
use sa_llm::{MockProvider, MockReply};
use sa_orchestrator::{Budget, ModelRouter, Orchestrator, ResolvedModel, Routines, Settings};
use sa_protocol::{ApprovalDecision, McpState, Message, ModelTier, RoomId, StepState};
use sa_store::Store;
use sa_tools::mcp::{run_demo_server, Dialer, McpServer, Pipe};
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

    /// 等到最后一条消息满足 `ready`。
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

    async fn replied(&self, text: &str) -> Message {
        self.until(text, |m| {
            m.content == text && m.state == sa_protocol::MessageState::Complete
        })
        .await
    }

    /// 第 `index` 次发给模型的请求，整个打出来。
    fn request(&self, index: usize) -> String {
        format!("{:?}", self.mock.requests()[index])
    }
}

/// 进程内的假 MCP 服务。
fn demo_server(auto_approve: &[&str]) -> Arc<McpServer> {
    let dialer: Dialer = Box::new(|| {
        Box::pin(async {
            let (ours, theirs) = tokio::io::duplex(64 * 1024);
            let (server_in, server_out) = tokio::io::split(theirs);
            tokio::spawn(run_demo_server(server_in, server_out));
            let (reader, writer) = tokio::io::split(ours);
            Ok(Pipe::new(reader, writer))
        })
    });
    let auto = auto_approve.iter().map(|name| name.to_string()).collect();
    Arc::new(McpServer::with_dialer("demo", auto, dialer))
}

#[tokio::test]
async fn a_bot_sees_its_skills_and_reads_one_before_using_it() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(skills.join("release")).unwrap();
    std::fs::write(
        skills.join("release/SKILL.md"),
        "---\nname: 发版\ndescription: 发新版本之前要做的检查\n---\n先跑测试，再改版本号。\n",
    )
    .unwrap();
    std::fs::write(
        skills.join("secret.md"),
        "---\ndescription: 只给别的 Bot 用\nbots: 别人\n---\n正文\n",
    )
    .unwrap();
    let rig = start(
        dir.path(),
        vec![
            MockReply::tool_calls("", &[("c1", "skill", r#"{"name":"发版"}"#)]),
            MockReply::text("按技能来：先跑测试。"),
            MockReply::text("好的。"),
        ],
        Budget::default(),
    );
    rig.orchestrator.use_skills(skills.clone());

    rig.say("准备发版").await;
    rig.replied("按技能来：先跑测试。").await;
    // 提示词里只有名字和一句话说明，不是给它用的不出现；工具表里有 skill。
    let first = rig.request(0);
    assert!(first.contains("- 发版：发新版本之前要做的检查"), "{first}");
    assert!(!first.contains("先跑测试，再改版本号"), "{first}");
    assert!(!first.contains("只给别的 Bot 用"), "{first}");
    let tools: Vec<String> = rig.mock.requests()[0]
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect();
    assert!(tools.contains(&"skill".to_string()), "{tools:?}");
    // 读了全文之后才拿到正文。读技能不用问人。
    assert!(rig.request(1).contains("先跑测试，再改版本号。"));

    let listed = rig.orchestrator.extensions();
    let names: Vec<&str> = listed
        .skills
        .iter()
        .map(|skill| skill.name.as_str())
        .collect();
    assert_eq!(names, ["secret", "发版"]);
    assert_eq!(listed.skills[0].bots, ["别人"]);

    // 技能删掉之后，下一轮就没有了：不用重启。
    std::fs::remove_dir_all(&skills).unwrap();
    rig.say("还有技能吗").await;
    rig.replied("好的。").await;
    let later = &rig.mock.requests()[2];
    assert!(!format!("{:?}", later.messages[0]).contains("发版："));
    assert!(later.tools.iter().all(|tool| tool.name != "skill"));
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn mcp_tools_join_the_toolbox_and_ask_before_running() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(
        dir.path(),
        vec![
            MockReply::tool_calls(
                "",
                &[
                    ("c1", "mcp__demo__add", r#"{"a":2,"b":3}"#),
                    ("c2", "mcp__demo__echo", r#"{"text":"你好"}"#),
                ],
            ),
            MockReply::text("算出来是 5。"),
        ],
        Budget::default(),
    );
    // 还没连上的时候，工具表里没有它的工具。
    assert!(rig.orchestrator.extensions().mcp.is_empty());
    let added = rig
        .orchestrator
        .connect_mcp(demo_server(&["add"]))
        .await
        .unwrap();
    assert_eq!(added, ["echo", "add", "fail", "wait", "quit"]);
    let again = rig.orchestrator.connect_mcp(demo_server(&[])).await;
    assert!(again.unwrap_err().0.contains("已经有一个叫「demo」"));
    let listed = rig.orchestrator.extensions().mcp;
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (listed[0].name.as_str(), listed[0].state),
        ("demo", McpState::Ready)
    );
    assert_eq!(listed[0].tools, added);

    rig.say("2 加 3 等于几").await;
    // 配置里写了不用问的（add）直接执行；别的（echo）出审批卡。
    let waiting = rig
        .until("有一步在等批准", |m| {
            m.steps.iter().any(|step| step.state == StepState::Waiting)
        })
        .await;
    let step = waiting
        .steps
        .iter()
        .find(|step| step.state == StepState::Waiting)
        .unwrap();
    assert_eq!(step.title, "调用 demo 的 echo");
    let approval = step.approval.clone().unwrap();
    assert!(
        approval.reason.contains("外部 MCP 服务「demo」"),
        "{}",
        approval.reason
    );
    assert!(rig
        .orchestrator
        .decide(&approval.id, ApprovalDecision::Once));

    rig.replied("算出来是 5。").await;
    let second = rig.request(1);
    assert!(
        second.contains("\"5\"") || second.contains("content: \"5\""),
        "{second}"
    );
    assert!(second.contains("你好"), "{second}");
    let tools: Vec<String> = rig.mock.requests()[0]
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect();
    assert!(tools.contains(&"mcp__demo__add".to_string()), "{tools:?}");
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn an_mcp_server_that_cannot_start_is_reported_and_harms_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(dir.path(), vec![MockReply::text("在。")], Budget::default());
    let dialer: Dialer = Box::new(|| {
        Box::pin(async { Err(sa_tools::mcp::McpError("找不到命令「npx」".into())) })
    });
    let broken = Arc::new(McpServer::with_dialer("broken", vec![], dialer));
    assert!(rig.orchestrator.connect_mcp(broken).await.is_err());
    let listed = rig.orchestrator.extensions().mcp;
    assert_eq!(listed[0].state, McpState::Failed);
    assert_eq!(listed[0].error.as_deref(), Some("找不到命令「npx」"));
    assert!(listed[0].tools.is_empty());

    rig.say("在吗").await;
    rig.replied("在。").await;
    assert!(rig.mock.requests()[0]
        .tools
        .iter()
        .all(|tool| !tool.name.starts_with("mcp__")));
    rig.orchestrator.shutdown().await;
}
