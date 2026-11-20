//! `sa chat`：在终端里和房间的入口 Bot 对话。和 `sa serve` 用同一份数据，所以两边看到的历史一样。

use crate::app::{init_logging, App, RunArgs, BUDGET_NOTICE};
use crate::paths::DataDir;
use anyhow::{bail, Context, Result};
use sa_protocol::{
    Event, EventKind, Message, MessageId, MessageState, Room, Sender, StepState, ToolStep,
};
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
            let mut out = std::io::stdout();
            let result = match args.message {
                Some(text) => {
                    let state = converse(&app, &room, &text, &mut out, ctrl_c()).await?;
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

async fn ctrl_c() {
    let _ = tokio::signal::ctrl_c().await;
}

fn pick_room(app: &App, wanted: Option<&str>) -> Result<Room> {
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
    if app.mock {
        writeln!(
            out,
            "当前用的是不联网的假模型（--mock），回复是固定规则生成的。"
        )?;
    } else {
        writeln!(out, "{BUDGET_NOTICE}")?;
    }
    if !earlier.messages.is_empty() {
        writeln!(out, "这个房间之前聊过，Bot 记得前面的内容。")?;
    }
    writeln!(
        out,
        "输入内容后回车发送；回复过程中按 Ctrl+C 停止；输入 /exit 或按 Ctrl+D 退出。"
    )?;

    // 读终端会阻塞，放到单独的线程里。
    let (lines, mut next_line) = tokio::sync::mpsc::channel::<String>(1);
    std::thread::spawn(move || {
        for line in std::io::stdin().lines() {
            let Ok(line) = line else { break };
            if lines.blocking_send(line).is_err() {
                break;
            }
        }
    });

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
        if let Err(error) = converse(app, room, text, out, ctrl_c()).await {
            writeln!(out, "没发出去：{error:#}")?;
        }
    }
}

/// 发一条消息，把回复边写边打印出来，直到写完。`interrupt` 完成时停下回复。
/// 返回回复最后的状态。
pub async fn converse(
    app: &App,
    room: &Room,
    text: &str,
    out: &mut impl Write,
    interrupt: impl Future<Output = ()>,
) -> Result<MessageState> {
    // 先订阅再发：不会漏掉回复的开头。
    let mut live = app.engine.subscribe_live();
    let mut wake = app.engine.subscribe();
    let mut position = app.engine.event_head();
    let sent = app
        .orchestrator
        .post_message(&room.id, text.to_string())
        .await?;

    let mut reply: Option<MessageId> = None;
    let mut printer = Printer::default();
    tokio::pin!(interrupt);
    let mut interrupted = false;
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
                        let Some(message) = bot_message(&event, room, sent.seq) else {
                            continue;
                        };
                        let id = reply.get_or_insert_with(|| message.id.clone());
                        if *id != message.id {
                            continue;
                        }
                        printer.steps(out, &message.steps)?;
                        if message.state != MessageState::Streaming {
                            printer.finish(out, &message)?;
                            return Ok(message.state);
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
    let from_bot = matches!(message.sender, Sender::Bot { .. });
    (message.room_id == room.id && from_bot && message.seq > after_seq).then_some(message)
}

/// 把一条回复打印到终端：文字原样输出，工具步骤各占一行。
#[derive(Default)]
struct Printer {
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
            write!(out, "\nBot> ")?;
        }
        write!(out, "{}", printable(shown))?;
        self.after_step = false;
        self.at_line_start = shown.ends_with('\n');
        out.flush()
    }

    fn line(&mut self, out: &mut impl Write, line: &str, is_step: bool) -> std::io::Result<()> {
        if !self.started {
            self.started = true;
            writeln!(out, "\nBot>")?;
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
                StepState::Running => format!("  · {} …", step.title),
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
fn printable(text: &str) -> std::borrow::Cow<'_, str> {
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
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("说明.md"), "项目代号：海豚\n").unwrap();
        let data_dir = DataDir::resolve(Some(dir.join("data"))).unwrap();
        let args = RunArgs {
            workdir: Some(work),
            mock: true,
        };
        // 和 --mock 用的是同一个假模型，只是出字更快。
        App::open_with(&data_dir, &args, |_| {
            Arc::new(SingleModel(ResolvedModel {
                provider: Arc::new(MockProvider::demo().with_chunk_delay(Duration::from_millis(4))),
                model: "mock".into(),
                thinking: None,
            }))
        })
        .unwrap()
    }

    async fn say(app: &App, text: &str) -> (MessageState, String) {
        let room = pick_room(app, None).unwrap();
        let mut out = Vec::new();
        let state = converse(app, &room, text, &mut out, std::future::pending())
            .await
            .unwrap();
        (state, String::from_utf8(out).unwrap())
    }

    #[tokio::test]
    async fn prints_the_reply_and_tool_steps() {
        let dir = tempfile::tempdir().unwrap();
        let app = open(dir.path());

        let (state, out) = say(&app, "你好").await;
        assert_eq!(state, MessageState::Complete);
        assert!(out.starts_with("\nBot> "), "{out:?}");
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
        assert_eq!(text, format!("\nBot> {expected}"));
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
