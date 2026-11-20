//! `sa chat`：在终端里和房间里的 Bot 对话，用 `@名字` 点名。和 `sa serve` 用同一份数据，所以两边看到的历史一样。

use crate::app::{init_logging, App, RunArgs};
use crate::paths::DataDir;
use anyhow::{bail, Context, Result};
use sa_protocol::{
    ApprovalDecision, Event, EventKind, Message, MessageId, MessageKind, MessageRef, MessageState,
    Room, RoomActivity, Sender, StepState, TaskState, ToolStep,
};
use std::collections::HashMap;
use std::future::Future;
use std::io::Write;
use std::process::ExitCode;
use tokio::sync::broadcast::error::RecvError;

#[derive(clap::Args)]
pub struct ChatArgs {
    /// 房间的名字或 ID，默认是第一个房间
    #[arg(long, value_name = "房间")]
    room: Option<String>,
    /// 只发这一条消息，等回复写完就退出
    #[arg(short, long, value_name = "内容")]
    message: Option<String>,
    #[command(flatten)]
    run: RunArgs,
}

pub fn run(data_dir: &DataDir, args: ChatArgs) -> Result<ExitCode> {
    // 终端里是对话，日志默认只留警告，免得插进回复中间。
    init_logging(tracing::Level::WARN);
    let app = App::open(data_dir, &args.run)?;
    let room = pick_room(&app, args.room.as_deref())?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            // 上次退出时没做完的任务接着排队。
            app.orchestrator.resume_tasks();
            // 先把外部 MCP 服务连上再开始聊：第一句话就可能用到它们的工具。
            app.connect_mcp().await;
            let mut out = std::io::stdout();
            let result = match args.message {
                Some(text) => {
                    let state =
                        converse(&app, &room, &text, &mut out, &mut stdin_lines(), ctrl_c())
                            .await?;
                    Ok(if state == MessageState::Complete {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::FAILURE
                    })
                }
                None => interactive(&app, &room, &mut out)
                    .await
                    .map(|()| ExitCode::SUCCESS),
            };
            app.orchestrator.shutdown().await;
            result
        })
}

pub(crate) async fn ctrl_c() {
    let _ = tokio::signal::ctrl_c().await;
}

pub(crate) fn pick_room(app: &App, wanted: Option<&str>) -> Result<Room> {
    let mut rooms = app.engine.list_rooms()?;
    // 按创建时间排，第一个是默认房间。
    rooms.sort_by_key(|room| (room.created_at, room.id.to_string()));
    let Some(wanted) = wanted else {
        return rooms.into_iter().next().context("还没有房间");
    };
    let names: Vec<_> = rooms.iter().map(|room| room.name.clone()).collect();
    let mut matches: Vec<Room> = rooms
        .into_iter()
        .filter(|room| room.name == wanted || room.id.to_string() == wanted)
        .collect();
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => bail!("没有叫「{wanted}」的房间。现有的房间：{}", names.join("、")),
        _ => bail!("有 {} 个房间都叫「{wanted}」，请改用房间 ID", matches.len()),
    }
}

async fn interactive(app: &App, room: &Room, out: &mut impl Write) -> Result<()> {
    let earlier = app.engine.list_messages(&room.id, None, 1)?;
    writeln!(out, "房间：{}　工作目录：{}", room.name, app.workdir)?;
    let bots = app.engine.list_bots()?;
    let members: Vec<&str> = room
        .member_bot_ids
        .iter()
        .filter_map(|id| bots.iter().find(|bot| &bot.id == id && bot.enabled))
        .map(|bot| bot.name.as_str())
        .collect();
    if members.len() > 1 {
        writeln!(
            out,
            "房间里的 Bot：{}。用 @名字 点名；不点名时会按分工挑一个来回答。",
            members.join("、")
        )?;
    }
    if app.mock {
        writeln!(
            out,
            "当前用的是不联网的假模型（--mock），回复是固定规则生成的。"
        )?;
    } else {
        writeln!(out, "{}", app.config.budget_notice())?;
    }
    if !earlier.messages.is_empty() {
        writeln!(out, "这个房间之前聊过，Bot 记得前面的内容。")?;
    }
    writeln!(
        out,
        "输入内容后回车发送；回复过程中按 Ctrl+C 停止；输入 /exit 或按 Ctrl+D 退出。"
    )?;

    let mut next_line = stdin_lines();

    loop {
        write!(out, "\n你> ")?;
        out.flush()?;
        let line = tokio::select! {
            line = next_line.recv() => line,
            _ = ctrl_c() => None,
        };
        let Some(line) = line else {
            writeln!(out)?;
            return Ok(());
        };
        let text = line.trim();
        match text {
            "" => continue,
            "/exit" | "/quit" => return Ok(()),
            _ => {}
        }
        if let Err(error) = converse(app, room, text, out, &mut next_line, ctrl_c()).await {
            writeln!(out, "没发出去：{error:#}")?;
        }
    }
}

/// 一行一行读终端输入。读终端会阻塞，所以放在单独的线程里。
pub(crate) fn stdin_lines() -> Lines {
    let (lines, next_line) = tokio::sync::mpsc::channel::<String>(1);
    std::thread::spawn(move || {
        for line in std::io::stdin().lines() {
            let Ok(line) = line else { break };
            if lines.blocking_send(line).is_err() {
                break;
            }
        }
    });
    next_line
}

pub(crate) type Lines = tokio::sync::mpsc::Receiver<String>;

/// 人对审批卡的回答：y 允许一次，a 总是允许，其他（包括没有输入）都算拒绝。
pub(crate) fn parse_decision(answer: Option<&str>) -> ApprovalDecision {
    match answer.map(|a| a.trim().to_lowercase()).as_deref() {
        Some("y" | "yes") => ApprovalDecision::Once,
        Some("a" | "always") => ApprovalDecision::Always,
        _ => ApprovalDecision::Deny,
    }
}

/// 发一条消息，把回复边写边打印出来，直到写完。`interrupt` 完成时停下回复。
/// Bot 要做需要批准的事时，从 `answers` 读一行作为答复。
/// 一条消息可能引出好几个 Bot 接连说话；等房间里没有人在说、也没有人排队了才返回。
/// 返回回复最后的状态：有哪条没正常结束就是它的状态，否则是「完成」。
///
/// 对话里开了任务的话，接着等任务做到需要人处理的地方（等验收、等回答、卡住），把结果打印出来。
pub async fn converse(
    app: &App,
    room: &Room,
    text: &str,
    out: &mut impl Write,
    answers: &mut Lines,
    interrupt: impl Future<Output = ()>,
) -> Result<MessageState> {
    tokio::pin!(interrupt);
    let known: std::collections::HashSet<_> = app
        .engine
        .list_tasks(&room.id)?
        .into_iter()
        .map(|task| task.id)
        .collect();
    let (mut outcome, interrupted) =
        exchange(app, room, text, out, answers, interrupt.as_mut()).await?;
    if interrupted {
        return Ok(outcome);
    }
    for task in app.engine.list_tasks(&room.id)? {
        // 这次对话里新开的任务，和还在做的任务，都跟到有结果为止。
        let active = matches!(task.state, TaskState::Open | TaskState::Working);
        if known.contains(&task.id) && !active {
            continue;
        }
        writeln!(
            out,
            "\n任务「{}」（按 Ctrl+C 停下它）：",
            printable(&task.title)
        )?;
        let task = crate::task::wait(app, task, out, answers, interrupt.as_mut()).await?;
        crate::task::show(app, &task, out)?;
        match task.state {
            TaskState::Blocked => outcome = MessageState::Failed,
            // 被 Ctrl+C 停下之后任务还没开工：后面的也不等了。
            TaskState::Open | TaskState::Working => break,
            _ => {}
        }
    }
    Ok(outcome)
}

/// 发一条消息并打印回复，直到房间闲下来。返回回复最后的状态，以及中途是不是被 `interrupt` 停下了。
async fn exchange<F: Future<Output = ()>>(
    app: &App,
    room: &Room,
    text: &str,
    out: &mut impl Write,
    answers: &mut Lines,
    mut interrupt: std::pin::Pin<&mut F>,
) -> Result<(MessageState, bool)> {
    // 先订阅再发：不会漏掉回复的开头。
    let mut live = app.engine.subscribe_live();
    let mut wake = app.engine.subscribe();
    let mut position = app.engine.event_head();
    let sent = app
        .orchestrator
        .post_message(&room.id, text.to_string())
        .await?;

    let names: HashMap<_, _> = app
        .engine
        .list_bots()?
        .into_iter()
        .map(|bot| (bot.id, bot.name))
        .collect();
    // 正在写的那条回复，和它已经打印到哪里。
    let mut reply: Option<MessageId> = None;
    let mut printer = Printer::default();
    let mut outcome = MessageState::Complete;
    let mut replies = 0;
    let mut interrupted = false;
    let mut asked = std::collections::HashSet::new();
    loop {
        tokio::select! {
            // 片段优先：同一时刻既有文字又有步骤更新时，文字是先产生的。
            biased;
            delta = live.recv() => match delta {
                Ok(delta) if reply.as_ref() == Some(&delta.message_id) => {
                    printer.text(out, &delta.text)?;
                }
                // 回复的「创建」事件还没读到：文字先不打，定稿时会按完整内容补上。
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => bail!("引擎已经关闭"),
            },
            changed = wake.changed() => {
                changed.context("引擎已经关闭")?;
                let head = *wake.borrow_and_update();
                while position < head {
                    let engine = app.engine.clone();
                    let batch = tokio::task::spawn_blocking(move || engine.events_after(position, 256))
                        .await??;
                    let Some(last) = batch.last() else { break };
                    position = last.seq;
                    for event in batch {
                        if idle(&event, room, sent.seq) {
                            // 没有人在说，也没有人排队：这条消息引出的对话结束了。
                            if replies == 0 {
                                writeln!(out, "\n（没有 Bot 回应）")?;
                            }
                            return Ok((outcome, interrupted));
                        }
                        if withdrawn(&event, reply.as_ref()) {
                            // 它决定不说话，这条回复撤掉了。扣着没显示的字本来就没打印出来。
                            reply = None;
                            printer = Printer::default();
                            continue;
                        }
                        let Some(message) = bot_message(&event, room, sent.seq) else {
                            continue;
                        };
                        if reply.is_none() {
                            // 一出现就已经写完的，是 Bot 找别的 Bot 时带的话：步骤行里已经有了，不再打一遍。
                            if message.state != MessageState::Streaming && event.kind == EventKind::MessageCreated {
                                continue;
                            }
                            reply = Some(message.id.clone());
                            let name = match &message.sender {
                                Sender::Bot { bot_id } => names.get(bot_id).cloned(),
                                Sender::User => None,
                            };
                            printer = Printer::named(name.unwrap_or_else(|| "Bot".into()));
                        }
                        if reply.as_ref() != Some(&message.id) {
                            continue;
                        }
                        printer.steps(out, &message.steps)?;
                        for step in &message.steps {
                            let (StepState::Waiting, Some(approval)) = (step.state, &step.approval)
                            else {
                                continue;
                            };
                            if interrupted || !asked.insert(approval.id.clone()) {
                                continue;
                            }
                            // 提示出现之前敲的回车不算数：那不是对这张卡的回答，不能拿它来批准。
                            while answers.try_recv().is_ok() {}
                            printer.line(out, &format!("  需要你批准：{}", approval.reason), true)?;
                            write!(out, "  [y] 允许一次  [a] 总是允许  [n] 拒绝 > ")?;
                            out.flush()?;
                            printer.at_line_start = false;
                            let left = (approval.expires_at - sa_engine::now_ms()).max(0) as u64;
                            let decision = tokio::select! {
                                answer = answers.recv() => {
                                    // 人按了回车，光标已经在新的一行。
                                    printer.at_line_start = answer.is_some();
                                    parse_decision(answer.as_deref())
                                }
                                _ = &mut interrupt => {
                                    interrupted = true;
                                    app.orchestrator.stop(&room.id);
                                    continue;
                                }
                                // 卡片过期了：不再等输入，接着显示后面的回复。
                                _ = tokio::time::sleep(std::time::Duration::from_millis(left)) => {
                                    printer.line(out, "  太久没有处理，按拒绝算。", true)?;
                                    continue;
                                }
                            };
                            if !app.orchestrator.decide(&approval.id, decision) {
                                printer.line(out, "  这张审批卡已经过期或者处理过了。", true)?;
                            }
                        }
                        if message.state != MessageState::Streaming {
                            printer.finish(out, &message)?;
                            replies += 1;
                            if message.state != MessageState::Complete {
                                outcome = message.state;
                            }
                            reply = None;
                        }
                    }
                }
            }
            _ = &mut interrupt, if !interrupted => {
                interrupted = true;
                app.orchestrator.stop(&room.id);
            }
        }
    }
}

/// 事件里如果是这个房间、在 `after_seq` 之后的 Bot 消息，就取出来。
fn bot_message(event: &Event, room: &Room, after_seq: i64) -> Option<Message> {
    if !matches!(
        event.kind,
        EventKind::MessageCreated | EventKind::MessageUpdated
    ) {
        return None;
    }
    let message: Message = serde_json::from_value(event.payload.clone()).ok()?;
    // 任务卡、任务的运行过程、交付卡不是对话里的回复，另外打印。
    let reply = matches!(message.sender, Sender::Bot { .. }) && message.kind == MessageKind::Chat;
    (message.room_id == room.id && reply && message.seq > after_seq).then_some(message)
}

/// 事件是不是「这个房间闲下来了，而且 `sent_seq` 这条人类消息已经处理过」。
fn idle(event: &Event, room: &Room, sent_seq: i64) -> bool {
    if event.kind != EventKind::RoomActivity {
        return false;
    }
    serde_json::from_value::<RoomActivity>(event.payload.clone()).is_ok_and(|activity| {
        activity.room_id == room.id
            && activity.speaking.is_none()
            && activity.queued.is_empty()
            && !activity.routing
            && activity.handled_seq >= sent_seq
    })
}

/// 事件是不是「正在写的那条回复被撤掉了」。
fn withdrawn(event: &Event, reply: Option<&MessageId>) -> bool {
    if event.kind != EventKind::MessageDeleted {
        return false;
    }
    serde_json::from_value::<MessageRef>(event.payload.clone())
        .is_ok_and(|gone| Some(&gone.id) == reply)
}

/// 把一条回复打印到终端：文字原样输出，工具步骤各占一行。
#[derive(Default)]
struct Printer {
    /// 说话的 Bot 的名字，打在回复前面。
    name: String,
    /// 已经打印出来的回复文字。
    printed: String,
    /// 光标是不是在一行的开头。
    at_line_start: bool,
    /// 上一次打印的是不是步骤行。
    after_step: bool,
    started: bool,
    /// 每个步骤上次打印时的状态。
    steps: Vec<StepState>,
}

impl Printer {
    fn named(name: String) -> Self {
        Self {
            name,
            ..Self::default()
        }
    }

    fn text(&mut self, out: &mut impl Write, text: &str) -> std::io::Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        self.printed.push_str(text);
        // 步骤行后面紧跟的空行不打：步骤行本身已经把前后两段文字隔开了。
        let shown = if self.after_step {
            text.trim_start_matches('\n')
        } else {
            text
        };
        if shown.is_empty() {
            return Ok(());
        }
        if !self.started {
            self.started = true;
            write!(out, "\n{}> ", printable(&self.name))?;
        }
        write!(out, "{}", printable(shown))?;
        self.after_step = false;
        self.at_line_start = shown.ends_with('\n');
        out.flush()
    }

    fn line(&mut self, out: &mut impl Write, line: &str, is_step: bool) -> std::io::Result<()> {
        if !self.started {
            self.started = true;
            writeln!(out, "\n{}>", printable(&self.name))?;
            self.at_line_start = true;
        }
        if !self.at_line_start {
            writeln!(out)?;
        }
        writeln!(out, "{}", printable(line))?;
        self.at_line_start = true;
        self.after_step = is_step;
        out.flush()
    }

    fn steps(&mut self, out: &mut impl Write, steps: &[ToolStep]) -> std::io::Result<()> {
        for (index, step) in steps.iter().enumerate() {
            let before = self.steps.get(index).copied();
            if before == Some(step.state) {
                continue;
            }
            let line = match step.state {
                StepState::Waiting => format!("  ? {}（等你批准）", step.title),
                StepState::Running => format!("  · {} …", step.title),
                StepState::Denied => format!("  ✗ {}（没有批准，未执行）", step.title),
                StepState::Ok => format!("  ✓ {}", step.title),
                StepState::Error => format!("  ✗ {}（失败，原因已交给 Bot）", step.title),
                StepState::Cancelled => format!("  - {}（已停止）", step.title),
            };
            self.line(out, &line, true)?;
            match self.steps.get_mut(index) {
                Some(state) => *state = step.state,
                None => self.steps.push(step.state),
            }
        }
        Ok(())
    }

    fn finish(&mut self, out: &mut impl Write, message: &Message) -> std::io::Result<()> {
        // 以定稿的内容为准，把没来得及打印的部分补上。
        let rest = message
            .content
            .strip_prefix(self.printed.as_str())
            .unwrap_or(&message.content)
            .to_string();
        self.text(out, &rest)?;
        let note = match message.state {
            MessageState::Complete | MessageState::Streaming => None,
            MessageState::Cancelled => Some("（已停止）".to_string()),
            MessageState::Interrupted => Some("（回复被中断）".to_string()),
            MessageState::Failed => Some(format!(
                "出错了：{}",
                message.error.as_deref().unwrap_or("原因不明")
            )),
        };
        match note {
            Some(note) => self.line(out, &note, false),
            None if !self.at_line_start => writeln!(out),
            None => Ok(()),
        }
    }
}

/// 去掉会被终端当成指令的字符。回复和工具标题里可能带着模型或文件里来的转义序列，
/// 原样输出能改屏幕内容、窗口标题，有的终端还能写剪贴板。换行和制表符保留。
pub(crate) fn printable(text: &str) -> std::borrow::Cow<'_, str> {
    let unsafe_char = |c: char| c.is_control() && c != '\n' && c != '\t';
    if !text.contains(unsafe_char) {
        return text.into();
    }
    text.chars()
        .map(|c| if unsafe_char(c) { '\u{fffd}' } else { c })
        .collect::<String>()
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sa_llm::MockProvider;
    use sa_orchestrator::{ResolvedModel, SingleModel};
    use std::sync::Arc;
    use std::time::Duration;

    fn open(dir: &std::path::Path) -> App {
        open_tuned(dir, |_| {})
    }

    fn open_tuned(dir: &std::path::Path, tune: impl FnOnce(&mut sa_orchestrator::Settings)) -> App {
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("说明.md"), "项目代号：海豚\n").unwrap();
        let data_dir = DataDir::resolve(Some(dir.join("data"))).unwrap();
        let args = RunArgs {
            workdir: Some(work),
            mock: true,
        };
        // 和 --mock 用的是同一个假模型，只是出字更快。
        let models = |_: &crate::config::Config| -> Arc<dyn sa_orchestrator::ModelRouter> {
            Arc::new(SingleModel(ResolvedModel {
                provider: Arc::new(MockProvider::demo().with_chunk_delay(Duration::from_millis(4))),
                model: "mock".into(),
                thinking: None,
                context_window: None,
            }))
        };
        App::open_tuned(&data_dir, &args, models, tune).unwrap()
    }

    async fn say(app: &App, text: &str) -> (MessageState, String) {
        say_and_answer(app, text, &[]).await
    }

    /// 终端的输出，测试里另一头可以边跑边看。
    #[derive(Clone, Default)]
    struct Screen(Arc<std::sync::Mutex<Vec<u8>>>);

    impl Screen {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for Screen {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// 发一条消息；每出现一次审批提示，就「敲」一行 `answers` 里的回答。
    /// 回答用完之后输入就关了，再问就是没有输入。
    async fn say_and_answer(app: &App, text: &str, answers: &[&str]) -> (MessageState, String) {
        let (typed, lines) = tokio::sync::mpsc::channel(8);
        let answers: Vec<String> = answers.iter().map(|a| a.to_string()).collect();
        let screen = Screen::default();
        let watching = screen.clone();
        let typist = tokio::spawn(async move {
            for (index, answer) in answers.into_iter().enumerate() {
                while watching.text().matches("拒绝 > ").count() <= index {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                typed.send(answer).await.unwrap();
            }
        });
        let result = chat(app, text, screen, lines).await;
        typist.abort();
        result
    }

    async fn chat(
        app: &App,
        text: &str,
        mut screen: Screen,
        mut lines: Lines,
    ) -> (MessageState, String) {
        let room = pick_room(app, None).unwrap();
        let state = converse(
            app,
            &room,
            text,
            &mut screen,
            &mut lines,
            std::future::pending(),
        )
        .await
        .unwrap();
        (state, screen.text())
    }

    /// 对话里开了任务：接着等它交结果，任务里要执行命令时也在终端里问；验收之后改动才回到工作目录。
    #[tokio::test]
    async fn a_task_opened_in_chat_is_followed_to_review_and_can_be_accepted() {
        let tmp = tempfile::tempdir().unwrap();
        let app = open(tmp.path());
        let (state, screen) = say(&app, "任务：写入 `笔记.md`：第一版").await;
        assert_eq!(state, MessageState::Complete);
        assert!(screen.contains("开任务：写入 笔记.md：第一版"), "{screen}");
        assert!(screen.contains("任务「写入 笔记.md：第一版」"), "{screen}");
        assert!(screen.contains("· 写入 笔记.md"), "{screen}");
        assert!(screen.contains("[等验收]"), "{screen}");
        assert!(screen.contains("文件：笔记.md（新增）"), "{screen}");
        assert!(screen.contains("sa task accept"), "{screen}");
        let work = tmp.path().join("work");
        assert!(!work.join("笔记.md").exists());

        let room = pick_room(&app, None).unwrap();
        let task = app.engine.list_tasks(&room.id).unwrap().remove(0);
        app.orchestrator.accept_task(&task.id).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(work.join("笔记.md")).unwrap(),
            "第一版\n"
        );

        // 任务里执行命令：终端里问一次，拒绝之后任务照样交结果。
        let (_, screen) = say_and_answer(&app, "任务：运行 `echo hi`", &["n"]).await;
        assert!(screen.contains("需要你批准"), "{screen}");
        assert!(screen.contains("[等验收]"), "{screen}");
        app.orchestrator.shutdown().await;
    }

    /// 审批卡过期：不再等人敲回车，回复照常显示完。之前敲的回车不会被当成对后面卡片的回答。
    #[tokio::test]
    async fn expired_or_stale_input_never_approves() {
        let dir = tempfile::tempdir().unwrap();
        let app = open_tuned(dir.path(), |settings| {
            settings.approval_timeout = Duration::from_millis(200);
        });
        // 输入一直开着，但没有人敲。
        let (typed, lines) = tokio::sync::mpsc::channel(8);
        let (state, out) = tokio::time::timeout(
            Duration::from_secs(20),
            chat(&app, "运行 `echo ok`", Screen::default(), lines),
        )
        .await
        .expect("过期之后不应该还在等输入");
        assert_eq!(state, MessageState::Complete, "{out}");
        assert!(out.contains("太久没有处理，按拒绝算。"), "{out}");
        assert!(
            out.contains("没有批准，未执行") && !out.contains("退出码"),
            "{out}"
        );

        // 提示出现之前敲的 y：不算数。
        let (early, lines) = tokio::sync::mpsc::channel(8);
        early.send("y".to_string()).await.unwrap();
        let (state, out) = chat(&app, "运行 `echo ok`", Screen::default(), lines).await;
        drop((typed, early));
        assert_eq!(state, MessageState::Complete, "{out}");
        assert!(
            out.contains("没有批准，未执行") && !out.contains("退出码"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn commands_ask_in_the_terminal_and_follow_the_answer() {
        let dir = tempfile::tempdir().unwrap();
        let app = open(dir.path());

        let (state, out) = say_and_answer(&app, "运行 `echo ok`", &["y"]).await;
        assert_eq!(state, MessageState::Complete, "{out}");
        assert!(out.contains("需要你批准：要执行命令：echo ok"), "{out}");
        assert!(
            out.contains("[y] 允许一次  [a] 总是允许  [n] 拒绝"),
            "{out}"
        );
        assert!(
            out.contains("✓ 执行 echo ok") && out.contains("退出码 0"),
            "{out}"
        );

        // 拒绝：不执行。没有输入（比如输入已经关闭）也按拒绝算。
        for answers in [&["n"][..], &[]] {
            let (state, out) = say_and_answer(&app, "运行 `echo ok`", answers).await;
            assert_eq!(state, MessageState::Complete, "{out}");
            assert!(out.contains("没有批准，未执行"), "{out}");
            assert!(!out.contains("退出码"), "{out}");
        }

        // 总是允许：这次执行，下次同样的命令不再问。
        let (_, out) = say_and_answer(&app, "运行 `echo ok`", &["a"]).await;
        assert!(out.contains("退出码 0"), "{out}");
        let (state, out) = say_and_answer(&app, "运行 `echo ok`", &[]).await;
        assert_eq!(state, MessageState::Complete, "{out}");
        assert!(
            !out.contains("需要你批准") && out.contains("退出码 0"),
            "{out}"
        );
    }

    /// 多个 Bot：每条回复前面是说话的 Bot 的名字；一个 Bot 找了另一个，等两个都说完才算结束。
    #[tokio::test]
    async fn several_bots_reply_under_their_own_names() {
        let dir = tempfile::tempdir().unwrap();
        let app = open(dir.path());
        let room = pick_room(&app, None).unwrap();
        let coder = app
            .engine
            .create_bot("Coder", sa_protocol::ModelTier::Mid, "写代码")
            .unwrap();
        app.engine.add_room_member(&room.id, &coder.id).unwrap();

        // 点名 Coder，它转给助手：两个人依次说话。
        let (state, out) = say(&app, "@Coder 转给 `助手`：帮我看看").await;
        assert_eq!(state, MessageState::Complete);
        let coder_at = out.find("\nCoder>").expect(&out);
        let host_at = out.find("\n助手> ").expect(&out);
        assert!(coder_at < host_at, "{out}");
        assert!(out.contains("  ✓ 请 助手：帮我看看"), "{out}");
        // 带给助手的那句话在步骤行里已经有了，不再单独打一遍。
        let coder_said = &out[coder_at..host_at];
        assert!(coder_said.contains("已经转过去了"), "{out}");
        assert!(!coder_said.contains("@助手 帮我看看"), "{out}");

        // 没点名：假路由按消息里出现的名字挑人。
        let (_, out) = say(&app, "这个问题 coder 怎么看").await;
        assert!(out.starts_with("\nCoder> "), "{out}");
        assert!(!out.contains("助手>"), "{out}");

        // 被点到的 Bot 决定不说话：不会一直等下去。
        let (state, out) = say(&app, "@Coder 谢谢，不用回了").await;
        assert_eq!(state, MessageState::Complete);
        assert_eq!(out, "\n（没有 Bot 回应）\n");
    }

    #[tokio::test]
    async fn prints_the_reply_and_tool_steps() {
        let dir = tempfile::tempdir().unwrap();
        let app = open(dir.path());

        let (state, out) = say(&app, "你好").await;
        assert_eq!(state, MessageState::Complete);
        assert!(out.starts_with("\n助手> "), "{out:?}");
        assert!(out.contains("你刚才说的是：「你好」"), "{out}");
        assert!(out.ends_with('\n') && !out.ends_with("\n\n"), "{out:?}");

        let (state, out) = say(&app, "读一下 `说明.md`").await;
        assert_eq!(state, MessageState::Complete);
        let running = out.find("  · 读取 说明.md …").expect(&out);
        let done = out.find("  ✓ 读取 说明.md").expect(&out);
        let quoted = out.find("项目代号：海豚").expect(&out);
        assert!(running < done && done < quoted, "{out}");
        // 去掉步骤行之后，打印出来的就是房间里存的那段文字（步骤行顶替了段落间的空行）。
        let room = pick_room(&app, None).unwrap();
        let stored = app
            .engine
            .list_messages(&room.id, None, 1)
            .unwrap()
            .messages;
        let text: String = out
            .lines()
            .filter(|line| !line.starts_with("  · ") && !line.starts_with("  ✓ "))
            .collect::<Vec<_>>()
            .join("\n");
        let expected = stored[0].content.replacen("。\n\n（", "。\n（", 1);
        assert_eq!(text, format!("\n助手> {expected}"));
        app.orchestrator.shutdown().await;
    }

    #[tokio::test]
    async fn failed_tool_step_and_interrupt_are_shown() {
        let dir = tempfile::tempdir().unwrap();
        let app = open(dir.path());
        let (state, out) = say(&app, "读一下 `没有这个文件.md`").await;
        assert_eq!(state, MessageState::Complete);
        assert!(out.contains("  ✗ 读取 没有这个文件.md"), "{out}");

        let room = pick_room(&app, None).unwrap();
        let mut out = Vec::new();
        let state = converse(
            &app,
            &room,
            "你好",
            &mut out,
            &mut tokio::sync::mpsc::channel(1).1,
            tokio::time::sleep(Duration::from_millis(120)),
        )
        .await
        .unwrap();
        assert_eq!(state, MessageState::Cancelled);
        let out = String::from_utf8(out).unwrap();
        assert!(out.trim_end().ends_with("（已停止）"), "{out}");
        app.orchestrator.shutdown().await;
    }

    #[tokio::test]
    async fn history_carries_over_to_the_next_run() {
        let dir = tempfile::tempdir().unwrap();
        {
            let app = open(dir.path());
            say(&app, "第一次来").await;
            app.orchestrator.shutdown().await;
        }
        let app = open(dir.path());
        let room = pick_room(&app, None).unwrap();
        assert_eq!(
            app.engine
                .list_messages(&room.id, None, 10)
                .unwrap()
                .messages
                .len(),
            2
        );
        let (state, out) = say(&app, "又来了").await;
        assert_eq!(state, MessageState::Complete);
        // 假模型会报出它在上下文里看到了几条用户消息。
        assert!(out.contains("第 2 条"), "{out}");
        app.orchestrator.shutdown().await;
    }

    #[tokio::test]
    async fn control_characters_never_reach_the_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let app = open(dir.path());
        // 文件里藏着：改窗口标题、写剪贴板（OSC 52）、清屏、回车覆盖、C1 控制符。
        let hostile =
            "正常开头\x1b]0;标题\x07\x1b]52;c;ZWNobyBoYWNrZWQ=\x07\x1b[2J\r覆盖\u{9b}31m 结尾\n";
        std::fs::write(dir.path().join("work/恶意.md"), hostile).unwrap();
        let (state, out) = say(&app, "读一下 `恶意.md`").await;
        assert_eq!(state, MessageState::Complete);
        assert!(out.contains("正常开头") && out.contains("结尾"), "{out:?}");
        let leaked: Vec<char> = out
            .chars()
            .filter(|c| c.is_control() && *c != '\n' && *c != '\t')
            .collect();
        assert!(leaked.is_empty(), "{leaked:?} 出现在 {out:?}");
        // 房间里存的还是原文，只在打印时处理。
        let room = pick_room(&app, None).unwrap();
        let stored = app
            .engine
            .list_messages(&room.id, None, 1)
            .unwrap()
            .messages;
        assert!(stored[0].content.contains('\x1b'));
        app.orchestrator.shutdown().await;

        assert_eq!(printable("表格\t换行\n"), "表格\t换行\n");
        assert_eq!(printable("a\x1b[31mb"), "a\u{fffd}[31mb");
    }

    #[test]
    fn room_is_picked_by_name_or_id() {
        let dir = tempfile::tempdir().unwrap();
        let app = open(dir.path());
        let default = pick_room(&app, None).unwrap();
        // 创建时间只精确到毫秒，隔开一点，保证先后顺序确定。
        std::thread::sleep(Duration::from_millis(5));
        let other = app.engine.create_room("调研").unwrap();
        assert_eq!(pick_room(&app, None).unwrap().id, default.id);
        assert_eq!(pick_room(&app, Some("调研")).unwrap().id, other.id);
        assert_eq!(
            pick_room(&app, Some(&other.id.to_string())).unwrap().id,
            other.id
        );
        let err = pick_room(&app, Some("不存在")).unwrap_err().to_string();
        assert!(err.contains("不存在") && err.contains("调研"), "{err}");
        app.engine.create_room("调研").unwrap();
        assert!(pick_room(&app, Some("调研"))
            .unwrap_err()
            .to_string()
            .contains("2 个房间"));
    }
}
