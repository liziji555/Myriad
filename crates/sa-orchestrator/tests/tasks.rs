//! 端到端：任务从开出来到验收。真实的 Engine、真实的 git、脚本化的假模型。
//!
//! 对话里的 Bot（中档）和任务的工作会话（强档）各用一个假模型，两边的脚本互不干扰。

use sa_engine::Engine;
use sa_llm::{MockProvider, MockReply, Role, StreamEvent};
use sa_orchestrator::{
    ModelRouter, Orchestrator, OrchestratorError, ResolvedModel, Settings, MAX_PARALLEL_RUNS,
};
use sa_protocol::{
    ApprovalDecision, ArtifactKind, BotId, CreateTaskRequest, Message, MessageKind, MessageState,
    ModelTier, ReportStatus, RoomId, RunState, StepState, Task, TaskId, TaskState,
};
use sa_store::Store;
use sa_tools::Fence;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

struct Models {
    front: Arc<MockProvider>,
    work: Option<Arc<MockProvider>>,
}

impl ModelRouter for Models {
    fn resolve(&self, tier: ModelTier) -> Result<ResolvedModel, String> {
        let provider = match tier {
            ModelTier::Strong => self.work.clone().ok_or("没有配置强档模型的密钥")?,
            _ => self.front.clone(),
        };
        Ok(ResolvedModel {
            provider,
            model: format!("mock-{}", tier.as_str()),
            thinking: None,
            context_window: None,
        })
    }
}

struct Rig {
    engine: Arc<Engine>,
    orchestrator: Orchestrator,
    front: Arc<MockProvider>,
    work: Arc<MockProvider>,
    room: RoomId,
    bot: BotId,
    /// 人的工作目录。
    workspace: PathBuf,
    /// 放任务目录的地方。
    tasks_dir: PathBuf,
}

fn start(dir: &Path, front: Vec<MockReply>, work: Vec<MockReply>) -> Rig {
    start_with(dir, front, work, true)
}

fn start_with(dir: &Path, front: Vec<MockReply>, work: Vec<MockReply>, strong: bool) -> Rig {
    start_in(dir, "ws", front, work, strong)
}

/// `workspace` 是人的工作目录在 `dir` 下面叫什么。
fn start_in(
    dir: &Path,
    workspace: &str,
    front: Vec<MockReply>,
    work: Vec<MockReply>,
    strong: bool,
) -> Rig {
    let data = dir.join("data");
    let workspace = dir.join(workspace);
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    let engine = Arc::new(Engine::new(Store::open(&data.join("state.db")).unwrap()).unwrap());
    engine.ensure_defaults().unwrap();
    engine.recover_interrupted().unwrap();
    engine.recover_runs().unwrap();
    let room = engine.list_rooms().unwrap().pop().unwrap();
    let front = Arc::new(MockProvider::scripted(front));
    let work = Arc::new(MockProvider::scripted(work));
    let settings = Settings {
        sessions_dir: data.join("sessions"),
        fence: Fence::new(&workspace, std::slice::from_ref(&data)).unwrap(),
        prompt: None,
        max_steps: 8,
        allow_private_network: false,
        approval_timeout: Duration::from_secs(600),
        work_dir: data.join("work"),
    };
    let models = Arc::new(Models {
        front: front.clone(),
        work: strong.then(|| work.clone()),
    });
    Rig {
        orchestrator: Orchestrator::new(engine.clone(), models, settings),
        engine,
        front,
        work,
        bot: room.host_bot_id.clone().unwrap(),
        room: room.id,
        workspace,
        tasks_dir: data.join("work"),
    }
}

fn git(dir: &Path, args: &[&str]) {
    let done = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        done.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&done.stderr)
    );
}

fn report(status: &str, summary: &str) -> MockReply {
    let input = serde_json::json!({ "status": status, "summary": summary }).to_string();
    MockReply::tool_calls("", &[("r1", "report", &input)])
}

fn write(path: &str, content: &str) -> MockReply {
    let input = serde_json::json!({ "path": path, "content": content }).to_string();
    MockReply::tool_calls("我来改。", &[("w1", "write", &input)])
}

impl Rig {
    fn messages(&self) -> Vec<Message> {
        self.engine
            .list_messages(&self.room, None, 100)
            .unwrap()
            .messages
    }

    async fn open_task(&self, title: &str) -> Task {
        self.orchestrator
            .create_task(
                &self.room,
                CreateTaskRequest {
                    title: title.into(),
                    scope: "把 a.txt 的内容改成「二」".into(),
                    acceptance: Some("a.txt 里是「二」".into()),
                    owner_bot_id: Some(self.bot.clone()),
                },
            )
            .await
            .unwrap()
    }

    /// 等任务走到某个状态，并且它的运行都收了尾。
    async fn task_in(&self, id: &TaskId, state: TaskState) -> Task {
        for _ in 0..1000 {
            let task = self.engine.get_task(id).unwrap();
            let running = self
                .engine
                .task_runs(id)
                .unwrap()
                .iter()
                .any(|run| run.state == RunState::Running);
            if task.state == state && (!running || state == TaskState::Working) {
                return task;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "任务没有变成 {state:?}，现在是：{:#?}\n{:#?}",
            self.engine.get_task(id).unwrap(),
            self.messages()
        );
    }

    /// 等工作会话的假模型收到第 `count` 次请求。
    async fn work_requests(&self, count: usize) {
        for _ in 0..1000 {
            if self.work.requests().len() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("工作会话只发了 {} 次请求", self.work.requests().len());
    }

    /// 工作会话第 `index` 次请求里，人这一侧说的话（任务说明、新指示）。
    fn work_inputs(&self, index: usize) -> Vec<String> {
        self.work.requests()[index]
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .map(|m| m.content.clone())
            .collect()
    }

    fn task_dir(&self, id: &TaskId) -> PathBuf {
        self.tasks_dir.join(id.as_str())
    }
}

#[tokio::test]
async fn a_bot_opens_a_task_and_the_work_session_delivers_it_for_acceptance() {
    let tmp = tempfile::tempdir().unwrap();
    let create = serde_json::json!({
        "title": "改 a.txt",
        "scope": "把 a.txt 的内容改成「二」",
        "acceptance": "a.txt 里是「二」"
    })
    .to_string();
    let rig = start(
        tmp.path(),
        vec![
            MockReply::tool_calls("", &[("c1", "create_task", &create)]),
            MockReply::text("任务开了，做完告诉你。"),
            MockReply::text("已经改好并验收了。"),
        ],
        vec![
            write("a.txt", "二\n"),
            report("done", "a.txt 已经改成「二」"),
            MockReply::text("交完了。"),
        ],
    );
    std::fs::write(rig.workspace.join("a.txt"), "一\n").unwrap();

    rig.orchestrator
        .post_message(&rig.room, "帮我把 a.txt 改成二".into())
        .await
        .unwrap();
    let task = loop {
        if let Some(task) = rig.engine.list_tasks(&rig.room).unwrap().pop() {
            break task;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let task = rig.task_in(&task.id, TaskState::Review).await;
    assert_eq!(task.title, "改 a.txt");
    assert_eq!(task.owner_bot_id, Some(rig.bot.clone()));
    // 验收之前，人的工作目录没有被动过；改动在任务自己的目录里。
    assert_eq!(
        std::fs::read_to_string(rig.workspace.join("a.txt")).unwrap(),
        "一\n"
    );
    assert_eq!(
        std::fs::read_to_string(rig.task_dir(&task.id).join("a.txt")).unwrap(),
        "二\n"
    );
    let files: Vec<_> = task
        .artifacts
        .iter()
        .map(|a| (a.kind, a.reference.as_str(), a.summary.as_str()))
        .collect();
    assert_eq!(files, [(ArtifactKind::File, "a.txt", "修改")]);

    // 时间线：人的话、Bot 的回复（里面有开任务这一步）、任务卡、运行过程、交付卡。
    let messages = rig.messages();
    let kinds: Vec<_> = messages.iter().map(|m| m.kind).collect();
    assert_eq!(
        kinds,
        [
            MessageKind::Chat,
            MessageKind::Chat,
            MessageKind::Task,
            MessageKind::Run,
            MessageKind::Delivery
        ],
        "{messages:#?}"
    );
    assert_eq!(messages[1].steps[0].tool, "create_task");
    assert_eq!(messages[1].steps[0].state, StepState::Ok);
    let run_message = &messages[3];
    assert_eq!(run_message.state, MessageState::Complete);
    let tools: Vec<_> = run_message.steps.iter().map(|s| s.tool.as_str()).collect();
    assert_eq!(tools, ["write", "report"]);
    assert_eq!(messages[4].content, "a.txt 已经改成「二」");

    // 工作会话拿到的是任务说明和房间里之前的对话，在任务目录里干活。
    let brief = &rig.work_inputs(0)[0];
    assert!(brief.contains("【任务】改 a.txt"), "{brief}");
    assert!(brief.contains("验收标准"), "{brief}");
    assert!(brief.contains("用户：帮我把 a.txt 改成二"), "{brief}");
    let system = &rig.work.requests()[0].messages[0].content;
    assert!(system.contains(task.id.as_str()), "{system}");
    assert!(rig.work.requests()[0]
        .tools
        .iter()
        .any(|t| t.name == "report"));
    assert!(!rig.work.requests()[0]
        .tools
        .iter()
        .any(|t| t.name == "create_task"));

    let runs = rig.engine.task_runs(&task.id).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].state, RunState::Succeeded);
    assert_eq!(runs[0].report.as_ref().unwrap().status, ReportStatus::Done);

    // 验收通过：改动带回工作目录，任务目录删掉。运行收尾之后才出现在任务目录里的文件也一起带回来。
    std::fs::write(rig.task_dir(&task.id).join("late.txt"), "后来的\n").unwrap();
    let task = rig.orchestrator.accept_task(&task.id).await.unwrap();
    assert_eq!(task.state, TaskState::Done);
    assert!(rig.workspace.join("late.txt").exists());
    assert_eq!(
        std::fs::read_to_string(rig.workspace.join("a.txt")).unwrap(),
        "二\n"
    );
    assert!(!rig.task_dir(&task.id).exists());
    assert!(rig.orchestrator.accept_task(&task.id).await.is_err());

    // 之后人再说话，对话里的 Bot 看得到交付结果。
    rig.orchestrator
        .post_message(&rig.room, "怎么样了？".into())
        .await
        .unwrap();
    for _ in 0..500 {
        if rig.front.requests().len() >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let seen: Vec<_> = rig.front.requests()[2]
        .messages
        .iter()
        .filter(|m| m.role == Role::User)
        .map(|m| m.content.clone())
        .collect();
    assert!(
        seen.iter()
            .any(|m| m.contains("【任务交付】") && m.contains("已经改成")),
        "{seen:#?}"
    );
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn a_question_waits_for_the_answer_and_a_rejection_sends_it_back() {
    let tmp = tempfile::tempdir().unwrap();
    let rig = start(
        tmp.path(),
        vec![],
        vec![
            report("needs_input", "要改成简体的「二」还是大写的「贰」？"),
            MockReply::text("等回答。"),
            write("a.txt", "贰\n"),
            report("done", "改成了「贰」"),
            MockReply::text("交完了。"),
            write("a.txt", "二\n"),
            report("done", "改成了「二」"),
            MockReply::text("交完了。"),
        ],
    );
    std::fs::write(rig.workspace.join("a.txt"), "一\n").unwrap();
    let task = rig.open_task("改 a.txt").await;

    let waiting = rig.task_in(&task.id, TaskState::Waiting).await;
    assert!(waiting.note.contains("还是"), "{waiting:#?}");
    // 等回答的任务不能直接验收。
    assert!(rig.orchestrator.accept_task(&task.id).await.is_err());

    rig.orchestrator
        .resume_task(&task.id, "大写的".into(), None)
        .await
        .unwrap();
    rig.task_in(&task.id, TaskState::Review).await;
    // 第二次运行接着同一个会话：前面的任务说明还在，后面多了人的回答。
    let inputs = rig.work_inputs(2);
    assert!(inputs[0].contains("【任务】改 a.txt"), "{inputs:#?}");
    assert_eq!(inputs.last().unwrap(), "【用户的新指示】\n大写的");

    // 等验收的任务不能「继续」，只能通过或打回；打回要写意见。
    assert!(rig
        .orchestrator
        .resume_task(&task.id, "再改改".into(), None)
        .await
        .is_err());
    assert!(rig
        .orchestrator
        .reject_task(&task.id, " ".into())
        .await
        .is_err());
    rig.orchestrator
        .reject_task(&task.id, "还是用简体吧".into())
        .await
        .unwrap();
    let task = rig.task_in(&task.id, TaskState::Review).await;
    for _ in 0..200 {
        if rig.engine.task_runs(&task.id).unwrap().len() == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let runs = rig.engine.task_runs(&task.id).unwrap();
    let attempts: Vec<_> = runs.iter().map(|run| run.attempt).collect();
    assert_eq!(attempts, [1, 2, 3]);
    assert_eq!(
        rig.work_inputs(5).last().unwrap(),
        "【用户的新指示】\n还是用简体吧"
    );

    rig.orchestrator.accept_task(&task.id).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(rig.workspace.join("a.txt")).unwrap(),
        "二\n"
    );
    // 三次运行各有一张交付卡。
    let deliveries = rig
        .messages()
        .iter()
        .filter(|m| m.kind == MessageKind::Delivery)
        .count();
    assert_eq!(deliveries, 3);
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn in_a_git_repo_the_task_works_on_its_own_branch_and_commands_need_approval() {
    let tmp = tempfile::tempdir().unwrap();
    let shell = serde_json::json!({ "command": "git status --short" }).to_string();
    let rig = start(
        tmp.path(),
        vec![
            MockReply::tool_calls("我也看一下。", &[("s2", "shell", &shell)]),
            MockReply::text("看完了。"),
        ],
        vec![
            write("b.txt", "新文件\n"),
            MockReply::tool_calls("看一下状态。", &[("s1", "shell", &shell)]),
            report("done", "加了 b.txt"),
            MockReply::text("交完了。"),
        ],
    );
    std::fs::write(rig.workspace.join("a.txt"), "一\n").unwrap();
    git(&rig.workspace, &["init", "-q", "-b", "main"]);
    git(&rig.workspace, &["add", "-A"]);
    git(&rig.workspace, &["commit", "-q", "-m", "init"]);
    let task = rig.open_task("加 b.txt").await;

    // 任务目录里写文件不用问；执行命令要先批准，审批卡挂在运行过程那条消息上。
    let approval = loop {
        let waiting = rig.messages().into_iter().find_map(|m| {
            (m.kind == MessageKind::Run)
                .then(|| m.steps.into_iter().find(|s| s.state == StepState::Waiting))
                .flatten()
        });
        if let Some(step) = waiting {
            break step.approval.unwrap().id;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(
        rig.engine.get_task(&task.id).unwrap().state,
        TaskState::Working
    );
    assert!(rig.orchestrator.decide(&approval, ApprovalDecision::Always));

    let task = rig.task_in(&task.id, TaskState::Review).await;
    // 任务里点的「总是允许」只管这个任务：同一条命令在对话里（在人的工作目录里跑）照样要问。
    rig.orchestrator
        .post_message(&rig.room, "你也跑一下".into())
        .await
        .unwrap();
    let asked_again = loop {
        let waiting = rig.messages().into_iter().find_map(|m| {
            (m.kind == MessageKind::Chat)
                .then(|| m.steps.into_iter().find(|s| s.state == StepState::Waiting))
                .flatten()
        });
        if let Some(step) = waiting {
            break step.approval.unwrap().id;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert!(rig
        .orchestrator
        .decide(&asked_again, ApprovalDecision::Deny));
    let branch = task.branch.clone().expect("git 仓库里的任务有自己的分支");
    let kinds: Vec<_> = task
        .artifacts
        .iter()
        .map(|a| (a.kind, a.reference.clone()))
        .collect();
    assert_eq!(
        kinds,
        [
            (ArtifactKind::Branch, branch.clone()),
            (ArtifactKind::File, "b.txt".to_string())
        ]
    );
    assert!(!rig.workspace.join("b.txt").exists());

    rig.orchestrator.accept_task(&task.id).await.unwrap();
    assert!(rig.workspace.join("b.txt").exists());
    assert!(!rig.task_dir(&task.id).exists());
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn accepting_fails_cleanly_when_the_workspace_changed_underneath() {
    let tmp = tempfile::tempdir().unwrap();
    let rig = start(
        tmp.path(),
        vec![],
        vec![
            write("a.txt", "任务改的\n"),
            report("done", "改了"),
            MockReply::text("交完了。"),
        ],
    );
    std::fs::write(rig.workspace.join("a.txt"), "一\n").unwrap();
    let task = rig.open_task("改 a.txt").await;
    rig.task_in(&task.id, TaskState::Review).await;
    std::fs::write(rig.workspace.join("a.txt"), "人改的\n").unwrap();

    let error = rig.orchestrator.accept_task(&task.id).await.unwrap_err();
    assert!(matches!(error, OrchestratorError::Conflict(_)), "{error}");
    // 任务还在等验收，两边的文件都没丢。
    assert_eq!(
        rig.engine.get_task(&task.id).unwrap().state,
        TaskState::Review
    );
    assert_eq!(
        std::fs::read_to_string(rig.workspace.join("a.txt")).unwrap(),
        "人改的\n"
    );
    assert!(rig.task_dir(&task.id).join("a.txt").exists());
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn stopping_keeps_the_work_and_cancelling_throws_it_away() {
    let tmp = tempfile::tempdir().unwrap();
    let rig = start(
        tmp.path(),
        vec![],
        vec![
            write("a.txt", "写了一半\n"),
            MockReply::Hang(vec![StreamEvent::TextDelta("还在想".into())]),
            MockReply::Hang(vec![StreamEvent::TextDelta("接着做".into())]),
        ],
    );
    std::fs::write(rig.workspace.join("a.txt"), "一\n").unwrap();
    let task = rig.open_task("改 a.txt").await;
    rig.work_requests(2).await;
    assert!(rig.orchestrator.stop_task(&task.id));

    let blocked = rig.task_in(&task.id, TaskState::Blocked).await;
    assert!(blocked.note.contains("停下"), "{blocked:#?}");
    let runs = rig.engine.task_runs(&task.id).unwrap();
    assert_eq!(runs[0].state, RunState::Cancelled);
    let run_message = rig
        .messages()
        .into_iter()
        .find(|m| m.kind == MessageKind::Run)
        .unwrap();
    assert_eq!(run_message.state, MessageState::Cancelled);
    // 现场留着：写了一半的文件还在任务目录里，也记进了交付物。
    assert_eq!(
        std::fs::read_to_string(rig.task_dir(&task.id).join("a.txt")).unwrap(),
        "写了一半\n"
    );
    assert_eq!(blocked.artifacts.len(), 1);
    // 运行收了尾之后再点停止，什么都不会停。
    for _ in 0..500 {
        if !rig.orchestrator.stop_task(&task.id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!rig.orchestrator.stop_task(&task.id));

    // 继续：接着同一个会话，告诉它上次没做完。
    rig.orchestrator
        .resume_task(&task.id, String::new(), None)
        .await
        .unwrap();
    rig.work_requests(3).await;
    assert!(rig.work_inputs(2).last().unwrap().contains("没有做完"));

    // 取消：运行停下，任务目录删掉，人的文件没动过。
    let cancelled = rig
        .orchestrator
        .cancel_task(&task.id, "不做了".into())
        .await
        .unwrap();
    assert_eq!(cancelled.state, TaskState::Cancelled);
    for _ in 0..500 {
        if !rig.task_dir(&task.id).exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!rig.task_dir(&task.id).exists());
    let task = rig.task_in(&task.id, TaskState::Cancelled).await;
    assert_eq!(task.note, "不做了");
    assert_eq!(
        std::fs::read_to_string(rig.workspace.join("a.txt")).unwrap(),
        "一\n"
    );
    assert!(rig
        .orchestrator
        .resume_task(&task.id, String::new(), None)
        .await
        .is_err());
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn a_restart_picks_the_task_up_where_it_stopped() {
    let tmp = tempfile::tempdir().unwrap();
    let rig = start(
        tmp.path(),
        vec![],
        vec![
            write("a.txt", "二\n"),
            MockReply::Hang(vec![StreamEvent::TextDelta("还在想".into())]),
        ],
    );
    std::fs::write(rig.workspace.join("a.txt"), "一\n").unwrap();
    let task = rig.open_task("改 a.txt").await;
    rig.work_requests(2).await;
    rig.orchestrator.shutdown().await;

    let stopped = rig.engine.get_task(&task.id).unwrap();
    assert_eq!(stopped.state, TaskState::Open);
    let runs = rig.engine.task_runs(&task.id).unwrap();
    assert_eq!(runs[0].state, RunState::Interrupted);
    drop(rig);

    let rig = start(
        tmp.path(),
        vec![],
        vec![report("done", "接着做完了"), MockReply::text("交完了。")],
    );
    rig.orchestrator.resume_tasks();
    let task = rig.task_in(&task.id, TaskState::Review).await;
    let inputs = rig.work_inputs(0);
    assert!(inputs[0].contains("【任务】改 a.txt"), "{inputs:#?}");
    assert!(inputs.last().unwrap().contains("没有做完"), "{inputs:#?}");
    assert_eq!(rig.engine.task_runs(&task.id).unwrap().len(), 2);
    // 上一次运行写的文件还在。
    assert_eq!(task.artifacts.len(), 1);
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn a_task_that_cannot_start_is_blocked_with_the_reason() {
    let tmp = tempfile::tempdir().unwrap();
    let rig = start_with(tmp.path(), vec![], vec![], false);
    let task = rig.open_task("改 a.txt").await;
    let blocked = rig.task_in(&task.id, TaskState::Blocked).await;
    assert!(blocked.note.contains("强档模型"), "{blocked:#?}");
    let runs = rig.engine.task_runs(&task.id).unwrap();
    assert_eq!(runs[0].state, RunState::Failed);

    // 负责人被请出了房间：开不了工，写明原因。
    let other = rig.engine.create_bot("Coder", ModelTier::Mid, "").unwrap();
    rig.engine.add_room_member(&rig.room, &other.id).unwrap();
    let second = rig
        .orchestrator
        .create_task(
            &rig.room,
            CreateTaskRequest {
                title: "另一个".into(),
                scope: "随便".into(),
                acceptance: None,
                owner_bot_id: Some(other.id.clone()),
            },
        )
        .await;
    // 抢在开工之前把它请出去不好控制时序，所以这里只看结果是两者之一。
    rig.engine.remove_room_member(&rig.room, &other.id).unwrap();
    let second = rig.task_in(&second.unwrap().id, TaskState::Blocked).await;
    assert!(!second.note.is_empty());
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn only_a_few_tasks_run_at_once_and_the_rest_wait_their_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let hang = || MockReply::Hang(vec![StreamEvent::TextDelta("想".into())]);
    let mut script: Vec<MockReply> = (0..MAX_PARALLEL_RUNS).map(|_| hang()).collect();
    script.push(hang());
    let rig = start(tmp.path(), vec![], script);
    let mut tasks = vec![];
    for index in 0..=MAX_PARALLEL_RUNS {
        tasks.push(rig.open_task(&format!("任务 {index}")).await);
    }
    rig.work_requests(MAX_PARALLEL_RUNS).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let states: Vec<_> = tasks
        .iter()
        .map(|t| rig.engine.get_task(&t.id).unwrap().state)
        .collect();
    assert_eq!(
        states.iter().filter(|s| **s == TaskState::Working).count(),
        MAX_PARALLEL_RUNS,
        "{states:?}"
    );
    assert_eq!(states.iter().filter(|s| **s == TaskState::Open).count(), 1);

    // 空出一个位置，排队的那个就开工。
    let waiting = tasks
        .iter()
        .find(|t| rig.engine.get_task(&t.id).unwrap().state == TaskState::Open)
        .unwrap();
    let running = tasks.iter().find(|t| t.id != waiting.id).unwrap();
    rig.orchestrator
        .cancel_task(&running.id, String::new())
        .await
        .unwrap();
    rig.task_in(&waiting.id, TaskState::Working).await;
    rig.orchestrator.shutdown().await;
}

/// 服务换了一个工作目录重新启动：之前的任务不在新的目录里接着做，也不往新的目录里验收。
#[tokio::test]
async fn a_task_never_lands_in_a_workspace_it_was_not_opened_in() {
    let tmp = tempfile::tempdir().unwrap();
    let rig = start(
        tmp.path(),
        vec![],
        vec![
            write("a.txt", "二\n"),
            report("done", "改了"),
            MockReply::text("交完了。"),
        ],
    );
    std::fs::write(rig.workspace.join("a.txt"), "一\n").unwrap();
    let task = rig.open_task("改 a.txt").await;
    rig.task_in(&task.id, TaskState::Review).await;
    rig.orchestrator.shutdown().await;
    let original = rig.workspace.clone();
    drop(rig);

    let rig = start_in(tmp.path(), "elsewhere", vec![], vec![], true);
    std::fs::write(rig.workspace.join("a.txt"), "一\n").unwrap();
    let error = rig.orchestrator.accept_task(&task.id).await.unwrap_err();
    assert!(matches!(error, OrchestratorError::Conflict(_)), "{error}");
    assert_eq!(
        rig.engine.get_task(&task.id).unwrap().state,
        TaskState::Review
    );
    for workspace in [&original, &rig.workspace] {
        assert_eq!(
            std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
            "一\n"
        );
    }
    // 打回重做也开不了工：卡住并说明原因，任务目录原样留着。
    rig.orchestrator
        .reject_task(&task.id, "再改改".into())
        .await
        .unwrap();
    let blocked = rig.task_in(&task.id, TaskState::Blocked).await;
    assert!(blocked.note.contains("现在的工作目录"), "{blocked:#?}");
    assert!(rig.task_dir(&task.id).join("a.txt").exists());
    rig.orchestrator.shutdown().await;
}
