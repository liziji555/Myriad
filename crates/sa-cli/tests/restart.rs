//! 故障注入：真的启动 `sa serve`，在它回话的中途强行杀掉进程，再启动，反复三次。
//! 验收的是「重启 3 次不丢消息」：说过的话一句不少、一句不重，没有回复永远停在「正在写」，
//! 每句话后面都有回应（写完的，或者标成中断的），而且重启之后还能接着聊。

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SA: &str = env!("CARGO_BIN_EXE_sa");

struct Server {
    child: Child,
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
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let url = loop {
            let line = lines
                .next()
                .expect("sa serve 没说完地址就退出了")
                .unwrap();
            if let Some((_, url)) = line.split_once("在浏览器里打开：") {
                break url.trim().to_string();
            }
        };
        // 后面的输出接着读掉，免得管道写满把它卡住。
        std::thread::spawn(move || lines.for_each(drop));
        let rest = url.strip_prefix("http://").unwrap();
        let (addr, token) = rest.split_once("/auth?token=").unwrap();
        Self {
            child,
            addr: addr.to_string(),
            token: token.to_string(),
        }
    }

    /// 不给它收尾的机会：相当于断电、`kill -9`、任务管理器里结束进程。
    fn kill(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }

    fn request(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let body = body.map(|body| body.to_string()).unwrap_or_default();
        let mut stream = TcpStream::connect(&self.addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
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

    fn messages(&self, room: &str) -> Vec<Value> {
        let path = format!("/api/rooms/{room}/messages?limit=200");
        let (status, page) = self.request("GET", &path, None);
        assert_eq!(status, 200, "{page}");
        page["messages"].as_array().unwrap().clone()
    }

    fn say(&self, room: &str, text: &str) {
        let path = format!("/api/rooms/{room}/messages");
        let (status, body) = self.request("POST", &path, Some(json!({ "content": text })));
        assert!(status == 200 || status == 201, "{status} {body}");
    }

    /// 等到房间里的消息满足 `ready`。
    fn until(&self, room: &str, what: &str, ready: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        let started = Instant::now();
        loop {
            let messages = self.messages(room);
            if ready(&messages) {
                return messages;
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "等了 30 秒也没等到：{what}。现在是 {messages:#?}"
            );
            std::thread::sleep(Duration::from_millis(40));
        }
    }
}

fn is_human(message: &Value) -> bool {
    message["sender"]["kind"] == "user"
}

/// 每句人说的话后面、下一句人说的话之前，都有 Bot 的回应；没有一条还停在「正在写」。
fn settled(messages: &[Value]) -> bool {
    let answered = messages.iter().enumerate().all(|(index, message)| {
        !is_human(message) || messages.get(index + 1).is_some_and(|next| !is_human(next))
    });
    answered && messages.iter().all(|message| message["state"] != "streaming")
}

fn human_texts(messages: &[Value]) -> Vec<String> {
    let human = messages.iter().filter(|message| is_human(message));
    human
        .map(|message| message["content"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn killing_the_server_three_times_loses_no_message() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = Server::start(dir.path());
    let room = server.room();
    let mut said = vec![];

    for round in 1..=3 {
        // 一句话回到一半的时候杀掉。
        let half = format!("第 {round} 轮：回到一半就断电");
        server.say(&room, &half);
        said.push(half);
        server.until(&room, "回复写到一半", |messages| {
            let last = messages.last().unwrap();
            last["state"] == "streaming" && last["content"].as_str().unwrap().chars().count() > 5
        });
        // 紧接着再说一句，还没人来得及接就杀掉。
        let unseen = format!("第 {round} 轮：刚说完就断电");
        server.say(&room, &unseen);
        said.push(unseen);
        server.kill();

        // 同一个数据目录马上能再启动：锁没有留下，数据库和会话记录打得开。
        server = Server::start(dir.path());
        let messages = server.until(&room, "重启之后都有了着落", settled);
        // 说过的话一句不少、一句不重、顺序不变。
        assert_eq!(human_texts(&messages), said);
        // 断电时写到一半的那条标成了中断，写了的字还在。
        let cut = messages
            .iter()
            .filter(|message| message["state"] == "interrupted")
            .count();
        assert_eq!(cut, round, "{messages:#?}");
        // 没人接的那句，重启之后有了完整的回复。
        let last = messages.last().unwrap();
        assert_eq!(last["state"], "complete", "{last:#}");
        assert!(last["content"].as_str().unwrap().contains("刚说完就断电"), "{last:#}");
    }

    // 三次之后照常聊：会话里之前的话都还在（假模型会报这是第几条）。
    server.say(&room, "还在吗");
    said.push("还在吗".to_string());
    let messages = server.until(&room, "最后一句有了回复", |messages| {
        settled(messages) && messages.last().unwrap()["state"] == "complete"
    });
    assert_eq!(human_texts(&messages), said);
    let last = messages.last().unwrap()["content"].as_str().unwrap();
    assert!(last.contains("第 7 条消息"), "{last}");
    server.kill();
}
