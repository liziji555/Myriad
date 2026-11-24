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
