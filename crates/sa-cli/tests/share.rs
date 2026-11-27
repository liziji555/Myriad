//! 真的启动 `sa serve` 和 `sa share`：服务那边的 Bot 看、改的是另一个目录（模拟另一台电脑）里的文件；
//! 共享目录外面的碰不到；`sa share` 断了，要用目录的那一步等着，重新连上后接着做；只读的不给改；
//! 加了 `--commands` 才能执行命令，每条先批准，可以停止。

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SA: &str = env!("CARGO_BIN_EXE_sa");

struct Server {
    child: Child,
    url: String,
    addr: String,
    token: String,
}

impl Server {
    fn start(dir: &Path) -> Self {
        let workdir = dir.join("ws");
        std::fs::create_dir_all(&workdir).unwrap();
        let mut child = Command::new(SA)
            .arg("--data-dir")
            .arg(dir.join("data"))
            .args(["serve", "--mock", "--port", "0", "--workdir"])
            .arg(&workdir)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let url = wait_line(&mut child, "在浏览器里打开：");
        let rest = url.strip_prefix("http://").unwrap();
        let (addr, token) = rest.split_once("/auth?token=").unwrap();
        Self {
            addr: addr.to_string(),
            token: token.to_string(),
            url,
            child,
        }
    }

    fn request(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let body = body.map(|body| body.to_string()).unwrap_or_default();
        let mut stream = TcpStream::connect(&self.addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.addr,
            self.token,
            body.len()
        )
        .unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        let (head, text) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, serde_json::from_str(text).unwrap_or(Value::Null))
    }

    fn room(&self) -> String {
        let (_, rooms) = self.request("GET", "/api/rooms", None);
        rooms[0]["id"].as_str().unwrap().to_string()
    }

    fn shares(&self) -> Vec<Value> {
        let (status, shares) = self.request("GET", "/api/shares", None);
        assert_eq!(status, 200, "{shares}");
        shares.as_array().unwrap().clone()
    }

    fn messages(&self, room: &str) -> Vec<Value> {
        let path = format!("/api/rooms/{room}/messages?limit=200");
        let (status, page) = self.request("GET", &path, None);
        assert_eq!(status, 200, "{page}");
        page["messages"].as_array().unwrap().clone()
    }

    /// 说一句，等 Bot 的回复写完，返回回复。等的时候 `waiting` 每轮看一眼还没写完的回复。
    fn ask(&self, room: &str, text: &str, mut waiting: impl FnMut(&Value)) -> Value {
        let before = self.messages(room).len();
        let path = format!("/api/rooms/{room}/messages");
        let (status, body) = self.request("POST", &path, Some(json!({ "content": text })));
        assert!(status == 200 || status == 201, "{status} {body}");
        let started = Instant::now();
        loop {
            let messages = self.messages(room);
            if let Some(reply) = messages.get(before + 1) {
                if reply["state"] != "streaming" {
                    return reply.clone();
                }
                waiting(reply);
            }
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "等了 60 秒也没等到「{text}」的回复：{messages:#?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 读子进程的输出，等到带 `marker` 的那一行，返回 `marker` 后面的部分。后面的输出接着读掉。
fn wait_line(child: &mut Child, marker: &str) -> String {
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let found = loop {
        let line = lines.next().expect("还没说到那一句就退出了").unwrap();
        if let Some((_, rest)) = line.split_once(marker) {
            break rest.trim().to_string();
        }
    };
    std::thread::spawn(move || lines.for_each(drop));
    found
}

/// `sa share` 进程，丢掉时杀掉（相当于电脑合盖、断网）。
struct Sharing(Child);

impl Sharing {
    fn start(dir: &Path, project: &Path, server: &Server, options: &[&str]) -> Self {
        let mut command = Command::new(SA);
        command
            .arg("--data-dir")
            .arg(dir.join("share-data"))
            .arg("share")
            .arg(project)
            .args(["--name", "我的项目", "--url", &server.url])
            .args(options);
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_line(&mut child, "已把");
        Self(child)
    }
}

impl Drop for Sharing {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn step(reply: &Value) -> &Value {
    let steps = reply["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 1, "{reply:#}");
    &steps[0]
}

#[test]
fn bots_read_and_change_files_in_a_shared_folder_and_wait_while_it_is_offline() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("另一台电脑").join("项目");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("说明.md"), "只在你电脑上的说明\n").unwrap();
    std::fs::write(dir.path().join("另一台电脑/外面.md"), "目录外面的秘密\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("../外面.md", project.join("捷径.md")).unwrap();

    let server = Server::start(dir.path());
    let room = server.room();
    assert!(server.shares().is_empty());

    let sharing = Sharing::start(dir.path(), &project, &server, &[]);
    let shares = server.shares();
    assert_eq!(shares.len(), 1, "{shares:?}");
    assert_eq!(shares[0]["name"], "我的项目");
    assert_eq!(shares[0]["online"], true);
    assert_eq!(shares[0]["read_only"], false);

    let path = format!("/api/rooms/{room}");
    let (status, body) = server.request("PATCH", &path, Some(json!({ "share": "我的项目" })));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["share"], "我的项目");

    // 读到的是共享目录里的文件，服务自己的工作目录里没有它。
    let reply = server.ask(&room, "读一下 `说明.md`", |_| {});
    assert_eq!(step(&reply)["state"], "ok", "{reply:#}");
    assert!(
        reply["content"]
            .as_str()
            .unwrap()
            .contains("只在你电脑上的说明"),
        "{reply:#}"
    );

    // 共享目录外面的碰不到，链接出去的也不行。
    let mut outside = vec!["读一下 `../外面.md`"];
    if cfg!(unix) {
        outside.push("读一下 `捷径.md`");
    }
    for text in outside {
        let reply = server.ask(&room, text, |_| {});
        assert_eq!(step(&reply)["state"], "error", "{reply:#}");
        assert!(!reply.to_string().contains("目录外面的秘密"), "{reply:#}");
    }

    // 改文件要先批准，写到的是那台电脑上的共享目录。
    let mut decided = false;
    let reply = server.ask(&room, "写入 `新笔记.md`：云端写的", |reply| {
        let waiting = reply["steps"]
            .as_array()
            .and_then(|steps| steps.iter().find(|s| s["state"] == "waiting"));
        if let (Some(step), false) = (waiting, decided) {
            let id = step["approval"]["id"].as_str().unwrap();
            let path = format!("/api/approvals/{id}/decide");
            let (status, body) = server.request("POST", &path, Some(json!({ "decision": "once" })));
            assert!(status < 300, "{status} {body}");
            decided = true;
        }
    });
    assert!(decided, "没有出审批卡：{reply:#}");
    assert_eq!(step(&reply)["state"], "ok", "{reply:#}");
    assert_eq!(
        std::fs::read_to_string(project.join("新笔记.md")).unwrap(),
        "云端写的\n"
    );
    assert!(!dir.path().join("ws/新笔记.md").exists());

    // 断开：要用目录的那一步等着，不算失败；重新连上后接着做完。
    drop(sharing);
    let mut sharing = None;
    let since = Instant::now();
    let reply = server.ask(&room, "读一下 `说明.md`", |_| {
        if sharing.is_none() && since.elapsed() > Duration::from_secs(3) {
            sharing = Some(Sharing::start(dir.path(), &project, &server, &[]));
        }
    });
    assert!(sharing.is_some(), "断开时没有等：{reply:#}");
    assert_eq!(step(&reply)["state"], "ok", "{reply:#}");
    assert!(reply["content"]
        .as_str()
        .unwrap()
        .contains("只在你电脑上的说明"));
    drop(sharing);

    // 只读：不给 Bot 改文件的工具，文件不会被改。
    let _sharing = Sharing::start(dir.path(), &project, &server, &["--read-only"]);
    assert_eq!(server.shares()[0]["read_only"], true);
    let reply = server.ask(&room, "写入 `说明.md`：被改了", |reply| {
        assert!(
            !reply.to_string().contains("\"waiting\""),
            "只读的共享还出了审批卡：{reply:#}"
        );
    });
    assert!(reply["steps"].as_array().unwrap().is_empty(), "{reply:#}");
    assert_eq!(
        std::fs::read_to_string(project.join("说明.md")).unwrap(),
        "只在你电脑上的说明\n"
    );
}

/// 每轮看一眼还没写完的回复：出了审批卡就批准一次，记在 `decided` 里。
fn approve_once(server: &Server, reply: &Value, decided: &mut bool) {
    let waiting = reply["steps"]
        .as_array()
        .and_then(|steps| steps.iter().find(|s| s["state"] == "waiting"));
    if let (Some(step), false) = (waiting, *decided) {
        assert_eq!(
            step["approval"]["can_allow_always"], false,
            "共享目录里的操作每次都问"
        );
        let id = step["approval"]["id"].as_str().unwrap();
        let path = format!("/api/approvals/{id}/decide");
        let (status, body) = server.request("POST", &path, Some(json!({ "decision": "once" })));
        assert!(status < 300, "{status} {body}");
        *decided = true;
    }
}

#[test]
fn bots_run_approved_commands_in_a_shared_folder_and_can_stop_them() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("另一台电脑").join("项目");
    std::fs::create_dir_all(&project).unwrap();
    let server = Server::start(dir.path());
    let room = server.room();
    let path = format!("/api/rooms/{room}");
    let (status, body) = server.request("PATCH", &path, Some(json!({ "share": "我的项目" })));
    assert_eq!(status, 200, "{body}");

    // 没加 --commands：不给执行命令的工具，什么也没跑。
    let sharing = Sharing::start(dir.path(), &project, &server, &[]);
    assert!(server.shares()[0]["shell"].is_null());
    let reply = server.ask(&room, "运行 `echo hi > 不该有.txt`", |_| {});
    assert!(reply["steps"].as_array().unwrap().is_empty(), "{reply:#}");
    assert!(!project.join("不该有.txt").exists());
    drop(sharing);

    let _sharing = Sharing::start(dir.path(), &project, &server, &["--commands"]);
    let shell = if cfg!(windows) { "PowerShell" } else { "bash" };
    assert_eq!(server.shares()[0]["shell"], shell);

    // 先批准，命令在共享目录里跑，不在服务的工作目录里。
    let mut decided = false;
    let reply = server.ask(&room, "运行 `echo hi > 命令写的.txt`", |reply| {
        approve_once(&server, reply, &mut decided)
    });
    assert!(decided, "没有出审批卡：{reply:#}");
    assert_eq!(step(&reply)["state"], "ok", "{reply:#}");
    assert!(project.join("命令写的.txt").exists());
    assert!(!dir.path().join("ws/命令写的.txt").exists());

    // 退出码不是 0 的照实报失败。
    let mut decided = false;
    let reply = server.ask(&room, "运行 `exit 3`", |reply| {
        approve_once(&server, reply, &mut decided)
    });
    assert_eq!(step(&reply)["state"], "error", "{reply:#}");
    assert!(reply.to_string().contains('3'), "{reply:#}");

    // 点停止：那边的命令连同它启动的进程一起结束，后面那半条不会再跑。
    let mut decided = false;
    let mut approved_at = None;
    let mut stopped = false;
    let reply = server.ask(
        &room,
        "运行 `sleep 4; echo done > 跑完了.txt`",
        |reply| {
            approve_once(&server, reply, &mut decided);
            if !decided || stopped {
                return;
            }
            if approved_at.get_or_insert_with(Instant::now).elapsed() > Duration::from_millis(1500)
            {
                let (status, body) =
                    server.request("POST", &format!("/api/rooms/{room}/stop"), None);
                assert!(status < 300, "{status} {body}");
                stopped = true;
            }
        },
    );
    assert!(stopped, "没来得及点停止：{reply:#}");
    assert_eq!(reply["state"], "cancelled", "{reply:#}");
    std::thread::sleep(Duration::from_secs(5));
    assert!(!project.join("跑完了.txt").exists());
}

#[test]
fn read_only_shares_never_run_commands() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("项目");
    std::fs::create_dir_all(&project).unwrap();
    let server = Server::start(dir.path());
    // --commands 和 --read-only 不能一起用。
    let output = Command::new(SA)
        .arg("--data-dir")
        .arg(dir.path().join("share-data"))
        .arg("share")
        .arg(&project)
        .args(["--url", &server.url, "--read-only", "--commands"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(server.shares().is_empty());
}

/// 加了 --commands 的共享里，harness 成员在那台电脑上的独立目录里干活：每次启动先批准，
/// 步骤报回房间，改动点「带回」才回到共享目录；点停止连它开出来的进程一起结束。
#[cfg(unix)]
#[test]
fn harness_members_work_on_the_shared_computer() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("另一台电脑").join("项目");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("说明.md"), "原来的\n").unwrap();
    let script = dir.path().join("fake-harness.sh");
    std::fs::write(
        &script,
        r#"prompt=$(cat)
echo '{"type":"session","id":"s1"}'
echo '{"type":"step","id":"1","tool":"write","title":"写 harness写的.txt"}'
pwd > harness写的.txt
echo '{"type":"step_end","id":"1","ok":true,"output":"写好了"}'
case "$prompt" in *慢*) sleep 30 & echo $! > sleep.pid; wait;; esac
echo '{"type":"text","text":"假 harness 做完了"}'
"#,
    )
    .unwrap();

    let server = Server::start(dir.path());
    let room = server.room();
    let (status, bot) = server.request(
        "POST",
        "/api/bots",
        Some(json!({
            "name": "Fake",
            "harness": "command",
            "harness_command": format!("bash {}", script.display()),
        })),
    );
    assert_eq!(status, 201, "{bot}");
    let bot_id = bot["id"].as_str().unwrap();
    let (status, body) =
        server.request("PUT", &format!("/api/rooms/{room}/members/{bot_id}"), None);
    assert!(status < 300, "{status} {body}");
    let path = format!("/api/rooms/{room}");
    let patch = json!({ "share": "我的项目", "host_bot_id": bot_id });
    let (status, body) = server.request("PATCH", &path, Some(patch));
    assert_eq!(status, 200, "{body}");

    // 没加 --commands：harness 不能在那边跑，什么都没动。
    let sharing = Sharing::start(dir.path(), &project, &server, &[]);
    let reply = server.ask(&room, "干活", |_| {});
    assert_eq!(reply["state"], "failed", "{reply:#}");
    assert!(reply.to_string().contains("--commands"), "{reply:#}");
    drop(sharing);

    let _sharing = Sharing::start(dir.path(), &project, &server, &["--commands"]);
    let mut decided = false;
    let reply = server.ask(&room, "干活", |reply| {
        approve_once(&server, reply, &mut decided)
    });
    assert!(decided, "没有出审批卡：{reply:#}");
    assert_eq!(reply["state"], "complete", "{reply:#}");
    assert!(reply.to_string().contains("假 harness 做完了"), "{reply:#}");
    let steps: Vec<(String, String)> = reply["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["tool"].as_str().unwrap().into(),
                s["state"].as_str().unwrap().into(),
            )
        })
        .collect();
    let ok = |tool: &str| steps.contains(&(tool.to_string(), "ok".to_string()));
    assert!(ok("harness") && ok("workdir") && ok("write"), "{steps:?}");

    // 改动在那台电脑的独立目录里，还没带回共享目录，服务这边也没有。
    let (_, desks) = server.request("GET", &format!("/api/rooms/{room}/desks"), None);
    let desk = &desks[0];
    assert_eq!(desk["changes"][0]["path"], "harness写的.txt", "{desks:#}");
    let desk_path = Path::new(desk["path"].as_str().unwrap());
    assert!(
        desk_path.starts_with(dir.path().join("share-data")),
        "{desks:#}"
    );
    assert!(desk_path.join("harness写的.txt").exists());
    assert!(!project.join("harness写的.txt").exists());
    assert!(!dir.path().join("ws/harness写的.txt").exists());

    // 带回：改动回到共享目录，独立目录收掉。
    let accept = format!("/api/rooms/{room}/desks/{bot_id}/accept");
    let (status, body) = server.request("POST", &accept, None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!([]));
    assert!(project.join("harness写的.txt").exists());
    assert!(!desk_path.exists());

    // 点停止：harness 和它开出来的进程一起结束。
    let mut decided = false;
    let mut stopped = false;
    let reply = server.ask(&room, "慢慢干", |reply| {
        approve_once(&server, reply, &mut decided);
        let wrote = reply["steps"].as_array().is_some_and(|steps| {
            steps
                .iter()
                .any(|s| s["tool"] == "write" && s["state"] == "ok")
        });
        if wrote && !stopped {
            std::thread::sleep(Duration::from_millis(500));
            let (status, body) = server.request("POST", &format!("/api/rooms/{room}/stop"), None);
            assert!(status < 300, "{status} {body}");
            stopped = true;
        }
    });
    assert!(stopped, "没来得及点停止：{reply:#}");
    assert_eq!(reply["state"], "cancelled", "{reply:#}");
    let (_, desks) = server.request("GET", &format!("/api/rooms/{room}/desks"), None);
    let desk_path = Path::new(desks[0]["path"].as_str().unwrap()).to_path_buf();
    let pid = std::fs::read_to_string(desk_path.join("sleep.pid")).unwrap();
    let alive = Path::new("/proc").join(pid.trim());
    let since = Instant::now();
    while alive.exists() && since.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!alive.exists(), "harness 开出来的进程还在");

    // 丢弃：独立目录删掉，共享目录不动。
    let discard = format!("/api/rooms/{room}/desks/{bot_id}/discard");
    let (status, body) = server.request("POST", &discard, None);
    assert_eq!(status, 200, "{body}");
    assert!(!desk_path.exists());
}

/// 远端共享工位全能力端到端：
/// 1) Claude Code 在 `sa share` 侧常驻 `ClaudePool` 跨轮复用进程（同 PID）；
/// 2) 远端 `can_use_tool` 双向审批卡（允许 + 拒绝）经共享隧道透传回房间；
/// 3) 运行中实时插话（`_steer` + `Progress::SteerAck`）注入远端进程且不触发重复轮次；
/// 4) 重置会话经 `_desk_evict` 立即回收远端常驻进程（新 PID），带回改动后落盘共享目录。
#[cfg(unix)]
#[test]
fn shared_claude_supports_persistent_pool_approvals_live_steer_and_evict() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("另一台电脑").join("claude-proj");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("README.md"), "remote repo\n").unwrap();

    let fake_claude = dir.path().join("fake-claude.py");
    std::fs::write(
        &fake_claude,
        r#"#!/usr/bin/env python3
import json, os, sys

def emit(obj):
    sys.stdout.write(json.dumps(obj, ensure_ascii=False) + "\n")
    sys.stdout.flush()

def user_text(msg):
    content = msg.get("message", {}).get("content", "")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(b.get("text", "") for b in content if isinstance(b, dict))
    return ""

def read_frame():
    line = sys.stdin.readline()
    if not line:
        return None
    return json.loads(line)

session_id = f"remote-sess-{os.getpid()}"
emit({"type": "system", "subtype": "init", "session_id": session_id})

while True:
    frame = read_frame()
    if frame is None:
        break
    kind = frame.get("type")
    if kind == "control_request":
        req_id = frame.get("request_id", "")
        sub = frame.get("request", {}).get("subtype", "")
        if sub == "initialize":
            emit({
                "type": "control_response",
                "response": {"subtype": "success", "request_id": req_id, "response": {"commands": []}},
            })
        elif sub == "interrupt":
            emit({
                "type": "control_response",
                "response": {"subtype": "success", "request_id": req_id, "response": {}},
            })
        continue
    if kind != "user":
        continue

    uid = frame.get("uuid")
    text = user_text(frame)
    emit({"type": "user", "uuid": uid, "message": frame.get("message", {})})

    if "ALLOW_AND_STEER" in text:
        req_id = "cr-allow-1"
        emit({
            "type": "control_request",
            "request_id": req_id,
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Bash",
                "tool_use_id": "tu-allow-1",
                "input": {"command": "echo remote-ok"},
            },
        })
        resp = read_frame()
        behavior = resp.get("response", {}).get("response", {}).get("behavior")
        if behavior != "allow":
            emit({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": f"UNEXPECTED_BEHAVIOR={behavior}"}}})
            emit({"type": "result", "subtype": "success", "session_id": session_id, "is_error": False})
            continue
        with open("remote_approved.txt", "w", encoding="utf-8") as f:
            f.write(f"pid={os.getpid()}\n")
        emit({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": f"APPROVED_PID={os.getpid()} WAITING_STEER "}}})
        steer_frame = read_frame()
        steer_uid = steer_frame.get("uuid")
        steer_text = user_text(steer_frame)
        emit({"type": "user", "uuid": steer_uid, "message": steer_frame.get("message", {})})
        emit({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": f"GOT_STEER={steer_text}"}}})
        emit({"type": "result", "subtype": "success", "session_id": session_id, "is_error": False})
    elif "DENY_TEST" in text:
        req_id = "cr-deny-1"
        emit({
            "type": "control_request",
            "request_id": req_id,
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Bash",
                "tool_use_id": "tu-deny-1",
                "input": {"command": "rm -rf /tmp/never"},
            },
        })
        resp = read_frame()
        behavior = resp.get("response", {}).get("response", {}).get("behavior")
        emit({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": f"DENY_RESULT={behavior} REUSE_PID={os.getpid()}"}}})
        emit({"type": "result", "subtype": "success", "session_id": session_id, "is_error": False})
    elif "AFTER_RESET" in text:
        emit({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": f"FRESH_PID={os.getpid()}"}}})
        emit({"type": "result", "subtype": "success", "session_id": session_id, "is_error": False})
    else:
        emit({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": f"ECHO_PID={os.getpid()}"}}})
        emit({"type": "result", "subtype": "success", "session_id": session_id, "is_error": False})
"#,
    )
    .unwrap();

    let server = Server::start(dir.path());
    let room = server.room();
    let (status, bot) = server.request(
        "POST",
        "/api/bots",
        Some(json!({
            "name": "RemoteClaude",
            "harness": "claude_code",
            "harness_command": format!("python3 {}", fake_claude.display()),
        })),
    );
    assert_eq!(status, 201, "{bot}");
    let bot_id = bot["id"].as_str().unwrap().to_string();
    let (status, body) =
        server.request("PUT", &format!("/api/rooms/{room}/members/{bot_id}"), None);
    assert!(status < 300, "{status} {body}");
    let (status, body) = server.request(
        "PATCH",
        &format!("/api/rooms/{room}"),
        Some(json!({ "share": "我的项目", "host_bot_id": bot_id })),
    );
    assert_eq!(status, 200, "{body}");

    let _sharing = Sharing::start(dir.path(), &project, &server, &["--commands"]);

    // 辅助函数：批准或拒绝某张尚未处理的审批卡（按 step.tool 过滤或全部）。
    let decide_step = |reply: &Value,
                       tool: &str,
                       decision: &str,
                       seen: &mut std::collections::HashSet<String>| {
        if let Some(steps) = reply["steps"].as_array() {
            for s in steps {
                if s["tool"].as_str() == Some(tool) && s["state"] == "waiting" {
                    if let Some(id) = s["approval"]["id"].as_str() {
                        if seen.insert(id.to_string()) {
                            let (st, body) = server.request(
                                "POST",
                                &format!("/api/approvals/{id}/decide"),
                                Some(json!({ "decision": decision })),
                            );
                            assert!(st < 300, "{st} {body}");
                        }
                    }
                }
            }
        }
    };

    // 第 1 轮：启动审批 + 远端 claude_approval 允许 + 运行中实时插话（不触发重复第 2 轮）。
    let mut handled_approvals = std::collections::HashSet::new();
    let mut steered = false;
    let reply1 = server.ask(&room, "ALLOW_AND_STEER", |reply| {
        decide_step(reply, "harness", "once", &mut handled_approvals);
        decide_step(reply, "claude_approval", "once", &mut handled_approvals);
        if !steered
            && reply["content"]
                .as_str()
                .is_some_and(|c| c.contains("WAITING_STEER"))
        {
            let (st, body) = server.request(
                "POST",
                &format!("/api/rooms/{room}/messages"),
                Some(json!({ "content": "MID_TURN_INTERJECTION" })),
            );
            assert!(st == 200 || st == 201, "{st} {body}");
            steered = true;
        }
    });
    assert!(steered, "没有触发运行中实时插话：{reply1:#}");
    assert_eq!(reply1["state"], "complete", "{reply1:#}");
    let content1 = reply1["content"].as_str().unwrap();
    assert!(content1.contains("APPROVED_PID="), "{content1}");
    assert!(content1.contains("MID_TURN_INTERJECTION"), "{content1}");
    let pid1: String = content1
        .split("APPROVED_PID=")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    // 等待片刻确认插话已提交游标，不会额外多出一轮回复。
    let msgs_after_turn1 = server.messages(&room).len();
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        server.messages(&room).len(),
        msgs_after_turn1,
        "已实时插话的消息不应再触发重复轮次"
    );

    // 第 2 轮：复用远端 ClaudePool 同一进程（同 PID），并测试拒绝 claude_approval。
    let reply2 = server.ask(&room, "DENY_TEST", |reply| {
        decide_step(reply, "harness", "once", &mut handled_approvals);
        decide_step(reply, "claude_approval", "deny", &mut handled_approvals);
    });
    assert_eq!(reply2["state"], "complete", "{reply2:#}");
    let content2 = reply2["content"].as_str().unwrap();
    assert!(content2.contains("DENY_RESULT=deny"), "{content2}");
    assert!(
        content2.contains(&format!("REUSE_PID={pid1}")),
        "第 2 轮应复用远端常驻进程 {pid1}，实际：{content2}"
    );

    // 重置会话：经 `_desk_evict` 回收远端常驻进程，第 3 轮应启动新 PID。
    let (st, body) = server.request(
        "POST",
        &format!("/api/rooms/{room}/desks/{bot_id}/reset-session"),
        None,
    );
    assert!(st < 300, "{st} {body}");

    let reply3 = server.ask(&room, "AFTER_RESET", |reply| {
        decide_step(reply, "harness", "once", &mut handled_approvals);
    });
    assert_eq!(reply3["state"], "complete", "{reply3:#}");
    let content3 = reply3["content"].as_str().unwrap();
    assert!(content3.contains("FRESH_PID="), "{content3}");
    let pid3: String = content3
        .split("FRESH_PID=")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    assert_ne!(pid3, pid1, "重置会话后远端会话池应已回收，不能再复用旧 PID");

    // 验收带回：远端先回收进程再把 remote_approved.txt 带回共享目录。
    let (st, body) = server.request(
        "POST",
        &format!("/api/rooms/{room}/desks/{bot_id}/accept"),
        None,
    );
    assert_eq!(st, 200, "{body}");
    assert!(project.join("remote_approved.txt").exists());
}
