//! harness：本机装好的 Agent 命令行（Claude Code、Codex、OpenCode，或者自己写的命令）。
//!
//! Myriad 在 Bot 的工位目录里启动它，把这一轮的话交给它，一行一行读它的 JSON 输出，
//! 转成和内置 Bot 一样的文字和步骤。点停止时连它开出来的子进程一起结束。

use sa_kernel::{Observer, StepEnd, TurnEnd};
use sa_protocol::Harness;
use sa_tools::process::Tree;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// 出错时最多带上这么多字的标准错误输出。
const STDERR_TAIL: usize = 2000;
/// harness 退出后，再等它剩下的输出多久。
const DRAIN: std::time::Duration = std::time::Duration::from_secs(1);

/// 启动一次 harness 要的东西。
pub(crate) struct Launch<'a> {
    pub harness: Harness,
    /// Bot 上写的命令。空的就用默认程序。
    pub command: &'a str,
    pub dir: &'a Path,
    pub prompt: &'a str,
    /// 上次的会话号，有的话接着聊。
    pub session: Option<&'a str>,
}

pub(crate) struct Finished {
    pub end: TurnEnd,
    /// harness 这次报的会话号。下次接着用。
    pub session: Option<String>,
    /// 它说的话（不含步骤）。
    pub text: String,
}

/// harness 输出里的一件事。
#[derive(Debug, Clone, PartialEq)]
enum Event {
    Session(String),
    Text(String),
    Started {
        id: String,
        tool: String,
        title: String,
    },
    Ended {
        id: String,
        tool: String,
        title: String,
        ok: bool,
        output: String,
    },
    Failed(String),
}

/// 把命令拆成程序和参数：按空白分开，双引号里的空白不算。
pub(crate) fn split_command(text: &str) -> Vec<String> {
    let mut parts = vec![];
    let mut current = String::new();
    let (mut quoted, mut any) = (false, false);
    for c in text.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any || !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
                any = false;
            }
            c => current.push(c),
        }
    }
    if any || !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// 在 PATH 里找程序。Windows 上按 PATHEXT 补后缀（`claude` 其实是 `claude.cmd`）。
pub fn find_program(program: &str, dir: &Path) -> Option<PathBuf> {
    let path = Path::new(program);
    let exts: Vec<String> = if cfg!(windows) {
        let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        std::iter::once(String::new())
            .chain(pathext.split(';').map(|e| e.to_ascii_lowercase()))
            .collect()
    } else {
        vec![String::new()]
    };
    let found = |base: &Path| {
        exts.iter()
            .map(|ext| PathBuf::from(format!("{}{ext}", base.display())))
            .find(|candidate| candidate.is_file())
    };
    // `./scripts/harness` 这种带目录的相对路径从 harness 的工作目录算起。
    if path.components().count() > 1 || path.is_absolute() {
        return found(&dir.join(path));
    }
    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|dir| found(&dir.join(path)))
}

/// 这种 harness 用哪个程序、哪些参数，提示词从哪给。
fn plan(launch: &Launch<'_>) -> Result<(PathBuf, Vec<String>, bool), String> {
    let mut words = split_command(launch.command);
    if words.is_empty() {
        match launch.harness.default_program() {
            Some(program) => words.push(program.to_string()),
            None => return Err("没有写要跑的命令".into()),
        }
    }
    let program = words.remove(0);
    let extra = words;
    let has = |flag: &str| {
        extra
            .iter()
            .any(|w| w == flag || w.starts_with(&format!("{flag}=")))
    };
    let mut args = vec![];
    let mut stdin = true;
    match launch.harness {
        Harness::ClaudeCode => {
            args.extend(extra.iter().cloned());
            args.extend(["-p", "--output-format", "stream-json", "--verbose"].map(String::from));
            if !has("--permission-mode") && !has("--dangerously-skip-permissions") {
                args.extend(["--permission-mode", "acceptEdits"].map(String::from));
            }
            if let Some(session) = launch.session {
                args.extend(["--resume".to_string(), session.to_string()]);
            }
        }
        Harness::Codex => {
            args.push("exec".into());
            args.extend(extra.iter().cloned());
            args.extend(["--json", "--skip-git-repo-check"].map(String::from));
            if !has("--sandbox") && !has("-s") && !has("--full-auto") {
                args.extend(["--sandbox", "workspace-write"].map(String::from));
            }
            if let Some(session) = launch.session {
                args.extend(["resume".to_string(), session.to_string()]);
            }
            args.push("-".into());
        }
        Harness::OpenCode => {
            args.push("run".into());
            args.extend(extra.iter().cloned());
            args.extend(["--format", "json"].map(String::from));
            if let Some(session) = launch.session {
                args.extend(["--session".to_string(), session.to_string()]);
            }
            args.push(launch.prompt.to_string());
            stdin = false;
        }
        Harness::Command => args.extend(extra.iter().cloned()),
        Harness::Builtin => return Err("内置 Bot 不走 harness".into()),
    }
    let resolved = find_program(&program, launch.dir).ok_or_else(|| {
        format!(
            "这台电脑上找不到 {program}，{} 没法启动。装好之后再试，或者在 Bot 设置里写上它的完整路径",
            launch.harness.label()
        )
    })?;
    Ok((resolved, args, stdin))
}

/// 「以后都允许」记的是哪个 Bot 用哪个 harness、哪条命令：换了 harness 或命令就要重新问。
pub fn approval_key(bot: &str, harness: Harness, command: &str) -> String {
    format!("{bot}\n{harness:?}\n{}", command.trim())
}

/// 跑一次 harness，直到它结束、出错或者被停下。
pub(crate) async fn run(
    launch: &Launch<'_>,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> Finished {
    let mut session = launch.session.map(String::from);
    let failed = |reason: String, session: Option<String>| Finished {
        end: TurnEnd::Failed(reason),
        session,
        text: String::new(),
    };
    let (program, args, use_stdin) = match plan(launch) {
        Ok(plan) => plan,
        Err(reason) => return failed(reason, session),
    };
    let mut command = Command::new(&program);
    command
        .args(&args)
        .current_dir(launch.dir)
        .env("SA_HARNESS_DIR", launch.dir)
        .stdin(if use_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // sa 自己用的密钥不交给 harness：它和 Bot 执行的命令一样拿不到。
    for name in sa_tools::hidden_env() {
        command.env_remove(name);
    }
    match launch.session {
        Some(id) => command.env("SA_HARNESS_SESSION", id),
        None => command.env_remove("SA_HARNESS_SESSION"),
    };
    let mut child = match Tree::spawn(command) {
        Ok(child) => child,
        Err(error) => {
            return failed(
                format!("{} 启动不了：{error}", launch.harness.label()),
                session,
            )
        }
    };
    if let Some(mut stdin) = child.stdin() {
        let prompt = launch.prompt.to_string();
        // 写完就关上，harness 读到结尾才开始干活。它不读也不卡在这里。
        tokio::spawn(async move {
            let _ = stdin.write_all(prompt.as_bytes()).await;
            let _ = stdin.shutdown().await;
        });
    }
    let stderr = child.stderr().map(|mut err| {
        tokio::spawn(async move {
            let mut text = String::new();
            let _ = err.read_to_string(&mut text).await;
            text
        })
    });
    let mut lines = BufReader::new(child.stdout().expect("上面接了管道")).lines();
    let mut text = String::new();
    let mut open: HashMap<String, ()> = HashMap::new();
    let mut failure: Option<String> = None;
    let mut seen = 0usize;

    // 它自己退出了，开出来的后台进程可能还占着输出：再读一小会儿剩下的就不等了。
    let mut exited = None;
    let stopped = loop {
        let draining = exited.is_some();
        let line = tokio::select! {
            _ = cancel.cancelled() => break true,
            status = child.wait(), if !draining => {
                exited = Some(status);
                continue;
            }
            line = async {
                if draining {
                    tokio::time::timeout(DRAIN, lines.next_line())
                        .await
                        .unwrap_or(Ok(None))
                } else {
                    lines.next_line().await
                }
            } => line,
        };
        let Ok(Some(line)) = line else { break false };
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        for event in parse(launch.harness, &value) {
            match event {
                Event::Session(id) => session = Some(id),
                Event::Text(said) => {
                    let said = said.trim();
                    if said.is_empty() {
                        continue;
                    }
                    let chunk = if text.is_empty() {
                        said.to_string()
                    } else {
                        format!("\n\n{said}")
                    };
                    observer.text_delta(&chunk).await;
                    text.push_str(&chunk);
                }
                Event::Started { id, tool, title } => {
                    seen += 1;
                    let id = format!("h{seen}-{id}");
                    if open.insert(id.clone(), ()).is_none() {
                        observer.tool_started(&id, &tool, &title, "").await;
                    }
                }
                Event::Ended {
                    id,
                    tool,
                    title,
                    ok,
                    output,
                } => {
                    // 开始时编了号：按原来的 id 找回去。没报过开始的，现在补一个。
                    let key = open
                        .keys()
                        .find(|key| key.split_once('-').map(|(_, raw)| raw) == Some(id.as_str()))
                        .cloned();
                    let key = match key {
                        Some(key) => key,
                        None => {
                            seen += 1;
                            let key = format!("h{seen}-{id}");
                            observer.tool_started(&key, &tool, &title, "").await;
                            key
                        }
                    };
                    open.remove(&key);
                    let end = if ok { StepEnd::Ok } else { StepEnd::Failed };
                    observer.tool_finished(&key, end, &output).await;
                }
                Event::Failed(reason) => failure = Some(reason),
            }
        }
    };

    if stopped {
        child.kill();
        let _ = child.wait().await;
        return Finished {
            end: TurnEnd::Cancelled,
            session,
            text,
        };
    }
    let status = match exited {
        Some(status) => status,
        None => tokio::select! {
            _ = cancel.cancelled() => {
                child.kill();
                let _ = child.wait().await;
                return Finished { end: TurnEnd::Cancelled, session, text };
            }
            status = child.wait() => status,
        },
    };
    // 它自己退出了，但开出来的子进程可能还在：一起结束。
    child.kill();
    // 万一还有没结束掉的后台进程占着错误输出：也只等一小会儿。
    let said = match stderr {
        Some(mut task) => match tokio::time::timeout(DRAIN, &mut task).await {
            Ok(said) => said.unwrap_or_default(),
            Err(_) => {
                task.abort();
                String::new()
            }
        },
        None => String::new(),
    };
    let end = match (failure, status) {
        (Some(reason), _) => TurnEnd::Failed(reason),
        (None, Ok(status)) if status.success() => TurnEnd::Completed,
        (None, status) => {
            let code = status
                .ok()
                .and_then(|s| s.code())
                .map(|c| format!("退出码 {c}"))
                .unwrap_or_else(|| "被结束了".into());
            let tail = tail(said.trim(), STDERR_TAIL);
            TurnEnd::Failed(if tail.is_empty() {
                format!("{} 中途退出了（{code}）", launch.harness.label())
            } else {
                format!("{} 中途退出了（{code}）：{tail}", launch.harness.label())
            })
        }
    };
    Finished { end, session, text }
}

fn tail(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let rest: String = text.chars().skip(count - max).collect();
    format!("…{rest}")
}

fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}

/// 一行 JSON 里有哪些事。不认识的行什么都不是。
fn parse(harness: Harness, value: &Value) -> Vec<Event> {
    match harness {
        Harness::ClaudeCode => parse_claude(value),
        Harness::Codex => parse_codex(value),
        Harness::OpenCode => parse_opencode(value),
        Harness::Command | Harness::Builtin => parse_plain(value),
    }
}

/// Claude Code 的工具换成 Myriad 的叫法，标题挑最有用的那个参数。
fn claude_tool(name: &str, input: &Value) -> (String, String) {
    let pick = |keys: &[&str]| {
        keys.iter()
            .map(|k| str_of(input, k))
            .find(|v| !v.is_empty())
            .unwrap_or("")
            .to_string()
    };
    let (tool, detail) = match name {
        "Bash" => ("shell", pick(&["command"])),
        "Read" => ("read", pick(&["file_path"])),
        "Write" => ("write", pick(&["file_path"])),
        "Edit" | "MultiEdit" | "NotebookEdit" => ("edit", pick(&["file_path", "notebook_path"])),
        "Glob" | "Grep" | "LS" => ("search", pick(&["pattern", "path"])),
        "WebFetch" | "WebSearch" => ("web_fetch", pick(&["url", "query"])),
        "Task" | "Agent" => ("bot", pick(&["description"])),
        other => (other, String::new()),
    };
    let title = if detail.is_empty() {
        name.to_string()
    } else {
        format!("{name} {detail}")
    };
    (tool.to_string(), title)
}

fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn parse_claude(value: &Value) -> Vec<Event> {
    let mut events = vec![];
    if let Some(id) = value["session_id"].as_str().filter(|id| !id.is_empty()) {
        events.push(Event::Session(id.to_string()));
    }
    let parts = value["message"]["content"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    match str_of(value, "type") {
        "assistant" => {
            for part in &parts {
                match str_of(part, "type") {
                    "text" => events.push(Event::Text(str_of(part, "text").to_string())),
                    "tool_use" => {
                        let (tool, title) = claude_tool(str_of(part, "name"), &part["input"]);
                        events.push(Event::Started {
                            id: str_of(part, "id").to_string(),
                            tool,
                            title,
                        });
                    }
                    _ => {}
                }
            }
        }
        "user" => {
            for part in parts.iter().filter(|p| str_of(p, "type") == "tool_result") {
                events.push(Event::Ended {
                    id: str_of(part, "tool_use_id").to_string(),
                    tool: "tool".into(),
                    title: "工具".into(),
                    ok: !part["is_error"].as_bool().unwrap_or(false),
                    output: content_text(&part["content"]),
                });
            }
        }
        "result" if value["is_error"].as_bool().unwrap_or(false) => {
            let said = str_of(value, "result");
            let reason = if said.is_empty() {
                format!("Claude Code 没做完（{}）", str_of(value, "subtype"))
            } else {
                said.to_string()
            };
            events.push(Event::Failed(reason));
        }
        _ => {}
    }
    events
}

fn codex_item(item: &Value) -> Option<(String, String)> {
    let kind = str_of(item, "type");
    Some(match kind {
        "command_execution" => ("shell".into(), str_of(item, "command").to_string()),
        "file_change" => {
            let paths: Vec<&str> = item["changes"]
                .as_array()
                .map(|c| c.iter().filter_map(|c| c["path"].as_str()).collect())
                .unwrap_or_default();
            ("edit".into(), format!("改文件 {}", paths.join("、")))
        }
        "mcp_tool_call" => (
            str_of(item, "tool").to_string(),
            format!("{} {}", str_of(item, "server"), str_of(item, "tool")),
        ),
        "web_search" => (
            "web_fetch".into(),
            format!("搜索 {}", str_of(item, "query")),
        ),
        _ => return None,
    })
}

fn parse_codex(value: &Value) -> Vec<Event> {
    let item = &value["item"];
    let id = str_of(item, "id").to_string();
    match str_of(value, "type") {
        "thread.started" => vec![Event::Session(str_of(value, "thread_id").to_string())],
        "item.started" => codex_item(item)
            .map(|(tool, title)| vec![Event::Started { id, tool, title }])
            .unwrap_or_default(),
        "item.completed" => {
            if str_of(item, "type") == "agent_message" {
                return vec![Event::Text(str_of(item, "text").to_string())];
            }
            let Some((tool, title)) = codex_item(item) else {
                return vec![];
            };
            let ok = match item["exit_code"].as_i64() {
                Some(code) => code == 0,
                None => str_of(item, "status") != "failed",
            };
            let output = match str_of(item, "aggregated_output") {
                "" => str_of(&item["result"], "content").to_string(),
                text => text.to_string(),
            };
            vec![Event::Ended {
                id,
                tool,
                title,
                ok,
                output,
            }]
        }
        "turn.failed" => vec![Event::Failed(
            str_of(&value["error"], "message").to_string(),
        )],
        "error" => vec![Event::Failed(str_of(value, "message").to_string())],
        _ => vec![],
    }
}

fn parse_opencode(value: &Value) -> Vec<Event> {
    let part = &value["part"];
    let mut events = vec![];
    let session = [str_of(value, "sessionID"), str_of(part, "sessionID")]
        .into_iter()
        .find(|id| !id.is_empty());
    if let Some(id) = session {
        events.push(Event::Session(id.to_string()));
    }
    match str_of(value, "type") {
        "text" => events.push(Event::Text(str_of(part, "text").to_string())),
        "tool_use" => {
            let state = &part["state"];
            let tool = str_of(part, "tool").to_string();
            let title = match str_of(state, "title") {
                "" => tool.clone(),
                title => format!("{tool} {title}"),
            };
            let id = match str_of(part, "callID") {
                "" => str_of(part, "id").to_string(),
                id => id.to_string(),
            };
            match str_of(state, "status") {
                "completed" | "error" => events.push(Event::Ended {
                    id,
                    tool,
                    title,
                    ok: str_of(state, "status") == "completed",
                    output: match str_of(state, "output") {
                        "" => str_of(state, "error").to_string(),
                        out => out.to_string(),
                    },
                }),
                _ => events.push(Event::Started { id, tool, title }),
            }
        }
        "error" => {
            let error = &value["error"];
            let reason = [
                str_of(&error["data"], "message"),
                str_of(error, "message"),
                error.as_str().unwrap_or(""),
            ]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or("OpenCode 出错了");
            events.push(Event::Failed(reason.to_string()));
        }
        _ => {}
    }
    events
}

/// 自定义命令的格式：一行一个 JSON。
///
/// - `{"type":"session","id":"…"}` 会话号，下次通过环境变量 `SA_HARNESS_SESSION` 交回来
/// - `{"type":"text","text":"…"}` 说的话
/// - `{"type":"step","id":"1","tool":"shell","title":"cargo test"}` 开始一步
/// - `{"type":"step_end","id":"1","ok":true,"output":"…"}` 这一步结束
/// - `{"type":"error","message":"…"}` 做不下去了
fn parse_plain(value: &Value) -> Vec<Event> {
    let id = str_of(value, "id").to_string();
    match str_of(value, "type") {
        "session" => vec![Event::Session(id)],
        "text" => vec![Event::Text(str_of(value, "text").to_string())],
        "step" => vec![Event::Started {
            id,
            tool: str_of(value, "tool").to_string(),
            title: str_of(value, "title").to_string(),
        }],
        "step_end" => vec![Event::Ended {
            id,
            tool: str_of(value, "tool").to_string(),
            title: str_of(value, "title").to_string(),
            ok: value["ok"].as_bool().unwrap_or(true),
            output: str_of(value, "output").to_string(),
        }],
        "error" => vec![Event::Failed(str_of(value, "message").to_string())],
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn relative_programs_with_a_directory_start_from_the_desk() {
        let desk = tempfile::tempdir().unwrap();
        std::fs::create_dir(desk.path().join("scripts")).unwrap();
        std::fs::write(desk.path().join("scripts/run"), "").unwrap();
        assert_eq!(
            find_program("./scripts/run", desk.path()),
            Some(desk.path().join("./scripts/run"))
        );
        assert_eq!(find_program("./scripts/missing", desk.path()), None);
    }

    #[test]
    fn approvals_are_kept_per_harness_and_command() {
        let first = approval_key("b1", Harness::Command, "a.sh");
        assert_eq!(first, approval_key("b1", Harness::Command, " a.sh "));
        assert_ne!(first, approval_key("b1", Harness::Command, "b.sh"));
        assert_ne!(first, approval_key("b1", Harness::Codex, "a.sh"));
    }

    #[test]
    fn commands_split_on_spaces_outside_quotes() {
        assert_eq!(
            split_command(r#" "C:\Program Files\x\claude.exe"  --model  opus "" "#),
            vec![r"C:\Program Files\x\claude.exe", "--model", "opus", ""]
        );
    }

    #[test]
    fn claude_stream_json_turns_into_text_and_steps() {
        let init = json!({"type":"system","subtype":"init","session_id":"s1"});
        assert_eq!(parse_claude(&init), vec![Event::Session("s1".into())]);
        let said = json!({"type":"assistant","session_id":"s1","message":{"content":[
            {"type":"text","text":"看一下"},
            {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}]}});
        assert_eq!(
            parse_claude(&said)[1..],
            [
                Event::Text("看一下".into()),
                Event::Started {
                    id: "t1".into(),
                    tool: "shell".into(),
                    title: "Bash cargo test".into()
                }
            ]
        );
        let result = json!({"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"t1","is_error":true,"content":[{"type":"text","text":"1 failed"}]}]}});
        assert!(matches!(
            &parse_claude(&result)[0],
            Event::Ended { id, ok: false, output, .. } if id == "t1" && output == "1 failed"
        ));
        let bad =
            json!({"type":"result","subtype":"error_max_turns","is_error":true,"session_id":"s1"});
        assert!(
            matches!(&parse_claude(&bad)[1], Event::Failed(r) if r.contains("error_max_turns"))
        );
    }

    #[test]
    fn codex_json_events_turn_into_text_and_steps() {
        let start = json!({"type":"thread.started","thread_id":"th"});
        assert_eq!(parse_codex(&start), vec![Event::Session("th".into())]);
        let run = json!({"type":"item.completed","item":{"id":"i1","type":"command_execution",
            "command":"ls","aggregated_output":"a\n","exit_code":2,"status":"failed"}});
        assert!(matches!(
            &parse_codex(&run)[0],
            Event::Ended { ok: false, tool, output, .. } if tool == "shell" && output == "a\n"
        ));
        let said = json!({"type":"item.completed","item":{"id":"i2","type":"agent_message","text":"好了"}});
        assert_eq!(parse_codex(&said), vec![Event::Text("好了".into())]);
        let failed = json!({"type":"turn.failed","error":{"message":"超额"}});
        assert_eq!(parse_codex(&failed), vec![Event::Failed("超额".into())]);
    }

    #[test]
    fn opencode_json_events_turn_into_text_and_steps() {
        let tool = json!({"type":"tool_use","sessionID":"o1","part":{"tool":"bash","callID":"c1",
            "state":{"status":"completed","title":"ls","output":"x"}}});
        assert_eq!(
            parse_opencode(&tool),
            vec![
                Event::Session("o1".into()),
                Event::Ended {
                    id: "c1".into(),
                    tool: "bash".into(),
                    title: "bash ls".into(),
                    ok: true,
                    output: "x".into()
                }
            ]
        );
    }

    #[test]
    fn programs_that_are_not_installed_are_reported_by_name() {
        let launch = Launch {
            harness: Harness::Command,
            command: "sa-surely-not-installed --x",
            dir: Path::new("."),
            prompt: "",
            session: None,
        };
        let error = plan(&launch).unwrap_err();
        assert!(error.contains("sa-surely-not-installed"), "{error}");
    }

    #[test]
    fn codex_resumes_its_thread_and_reads_the_prompt_from_stdin() {
        let launch = Launch {
            harness: Harness::Codex,
            command: "sh",
            dir: Path::new("."),
            prompt: "hi",
            session: Some("th"),
        };
        if let Ok((_, args, stdin)) = plan(&launch) {
            assert!(stdin);
            assert_eq!(args[0], "exec");
            assert_eq!(&args[args.len() - 3..], ["resume", "th", "-"]);
        }
    }
}
