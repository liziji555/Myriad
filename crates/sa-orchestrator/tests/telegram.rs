//! 端到端：Telegram 那边的话收进房间、回复送回去。真实的 Engine、脚本化的假模型、假的 Telegram 服务。

use sa_engine::{Engine, Incoming, IncomingKind, Receipt};
use sa_llm::{MockProvider, MockReply};
use sa_orchestrator::{
    Budget, ModelRouter, Orchestrator, ResolvedModel, Routines, Settings, Telegram, TelegramOptions,
};
use sa_protocol::{ModelTier, RoomId, Source};
use sa_store::Store;
use sa_tools::Fence;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
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

const TOKEN: &str = "123:secret";

/// 假的 Telegram：`updates` 是那边的人发的消息，`sent` 是机器人发出去的。
#[derive(Default)]
struct Fake {
    updates: Vec<Value>,
    sent: Vec<(String, String)>,
    /// 接下来几次 sendMessage 不成功，按顺序回这些。
    refuse: Vec<(u16, Value)>,
    offsets: Vec<i64>,
}

#[derive(Clone, Default)]
struct FakeTelegram(Arc<Mutex<Fake>>);

impl FakeTelegram {
    async fn serve(&self) -> String {
        use axum::extract::{Path, State};
        use axum::http::StatusCode;
        use axum::routing::post;

        async fn call(
            State(fake): State<FakeTelegram>,
            Path((bot, method)): Path<(String, String)>,
            axum::Json(body): axum::Json<Value>,
        ) -> (StatusCode, axum::Json<Value>) {
            let ok = |result: Value| {
                (
                    StatusCode::OK,
                    axum::Json(json!({ "ok": true, "result": result })),
                )
            };
            if bot != format!("bot{TOKEN}") {
                let body = json!({ "ok": false, "description": "Unauthorized" });
                return (StatusCode::UNAUTHORIZED, axum::Json(body));
            }
            match method.as_str() {
                "getMe" => ok(json!({
                    "id": 900, "is_bot": true, "first_name": "SA", "username": "sa_bot"
                })),
                "getUpdates" => {
                    let offset = body["offset"].as_i64().unwrap();
                    let fresh: Vec<Value> = {
                        let mut fake = fake.0.lock().unwrap();
                        fake.offsets.push(offset);
                        // 和真的一样：带着位置来取，就是确认了这之前的；之后不带位置也不会再给。
                        if offset > 0 {
                            fake.updates
                                .retain(|update| update["update_id"].as_i64().unwrap() >= offset);
                        }
                        fake.updates.clone()
                    };
                    if fresh.is_empty() {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    ok(json!(fresh))
                }
                "sendMessage" => {
                    let mut fake = fake.0.lock().unwrap();
                    if !fake.refuse.is_empty() {
                        let (status, body) = fake.refuse.remove(0);
                        return (StatusCode::from_u16(status).unwrap(), axum::Json(body));
                    }
                    let chat = body["chat_id"].as_str().unwrap().to_string();
                    let text = body["text"].as_str().unwrap().to_string();
                    fake.sent.push((chat, text));
                    ok(json!({}))
                }
                other => panic!("没想到会调用 {other}"),
            }
        }

        let app = axum::Router::new()
            .route("/{bot}/{method}", post(call))
            .with_state(self.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}")
    }

    /// 那边有人发来一条消息。
    fn says(&self, update_id: i64, chat: i64, kind: &str, text: &str) {
        let update = json!({
            "update_id": update_id,
            "message": {
                "message_id": update_id,
                "chat": { "id": chat, "type": kind, "title": if kind == "private" { Value::Null } else { json!("项目群") } },
                "from": { "id": 7, "first_name": "张三" },
                "text": text,
            },
        });
        self.0.lock().unwrap().updates.push(update);
    }

    fn sent(&self) -> Vec<(String, String)> {
        self.0.lock().unwrap().sent.clone()
    }

    /// 等到机器人发出去 `count` 条。
    async fn sent_count(&self, count: usize) -> Vec<String> {
        for _ in 0..400 {
            let sent = self.sent();
            if sent.len() >= count {
                return sent.into_iter().map(|(_, text)| text).collect();
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("等了 10 秒，只发出去 {:?}", self.sent());
    }
}

async fn connect(rig: &Rig, fake: &FakeTelegram) {
    let client = Telegram::new(&fake.serve().await, TOKEN);
    let options = TelegramOptions {
        wait_secs: 0,
        every: Duration::from_millis(20),
        backoff: Duration::from_millis(50),
    };
    rig.orchestrator.start_telegram(client, options);
}

#[tokio::test]
async fn a_private_chat_is_paired_and_then_talks_to_the_room() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(
        dir.path(),
        vec![MockReply::text("看过了，没问题。")],
        Budget::default(),
    );
    let pairing = rig.engine.create_pairing(&rig.room).unwrap();
    let fake = FakeTelegram::default();
    // 还没配对就说话：告诉对方怎么连，话不进房间。
    fake.says(1, 42, "private", "你好");
    fake.says(2, 42, "private", "/pair NOPE22");
    fake.says(3, 42, "private", &format!("/start {}", pairing.code));
    fake.says(4, 42, "private", "帮我看看今天的日志");
    connect(&rig, &fake).await;

    let sent = fake.sent_count(4).await;
    assert!(sent[0].contains("/pair 配对码"), "{sent:?}");
    assert!(sent[1].contains("配对码不对"), "{sent:?}");
    assert!(sent[2].starts_with("已经连到房间「"), "{sent:?}");
    assert_eq!(sent[3], "看过了，没问题。");
    assert!(fake.sent().iter().all(|(chat, _)| chat == "42"));
    assert_eq!(
        rig.orchestrator.telegram_username().as_deref(),
        Some("sa_bot")
    );

    // 只有配对之后的那一句进了房间，叫了一次模型。
    assert_eq!(rig.mock.requests().len(), 1);
    let links = rig.engine.list_links().unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(
        (links[0].external.as_str(), links[0].title.as_str()),
        ("42", "张三")
    );

    // 四条都向 Telegram 确认过了；同一条再送来（重启之后重新收到）不会变成第二条消息。
    assert!(fake.0.lock().unwrap().offsets.contains(&5));
    assert!(fake.0.lock().unwrap().updates.is_empty());
    let again = Incoming {
        source: Source::Telegram,
        key: "900:4".into(),
        name: "message".into(),
        external: "42".into(),
        label: "张三".into(),
        url: None,
        kind: IncomingKind::Chat {
            text: "帮我看看今天的日志".into(),
        },
    };
    assert_eq!(
        rig.orchestrator.receive(again).await.unwrap(),
        Receipt::Duplicate
    );
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn in_a_group_only_what_is_said_to_the_bot_is_taken() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(
        dir.path(),
        vec![MockReply::text("日志没问题。")],
        Budget::default(),
    );
    let pairing = rig.engine.create_pairing(&rig.room).unwrap();
    let fake = FakeTelegram::default();
    // 没连接的群里 @ 机器人：不出声。
    fake.says(1, -100, "supergroup", "@sa_bot 在吗");
    fake.says(
        2,
        -100,
        "supergroup",
        &format!("/pair@sa_bot {}", pairing.code.to_lowercase()),
    );
    fake.says(3, -100, "supergroup", "今天中午吃什么");
    fake.says(4, -100, "supergroup", "@SA_bot 看下日志");
    connect(&rig, &fake).await;

    let sent = fake.sent_count(2).await;
    assert!(sent[0].starts_with("已经连到房间「"), "{sent:?}");
    assert_eq!(sent[1], "日志没问题。");
    assert_eq!(rig.engine.list_links().unwrap()[0].title, "项目群");
    let requests = rig.mock.requests();
    assert_eq!(requests.len(), 1);
    let input = format!("{:?}", requests[0].messages.last().unwrap());
    assert!(input.contains("【张三（Telegram）】看下日志"), "{input}");
    assert!(!input.contains("吃什么"), "{input}");
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn guessing_pairing_codes_is_cut_off() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(dir.path(), vec![], Budget::default());
    let pairing = rig.engine.create_pairing(&rig.room).unwrap();
    let fake = FakeTelegram::default();
    for guess in 1..=5 {
        fake.says(guess, 66, "private", &format!("/pair WRONG{guess}"));
    }
    // 猜了五次之后碰对了也不算：这个聊天这十分钟里不再理会。
    fake.says(6, 66, "private", &format!("/pair {}", pairing.code));
    // 别的聊天不受连累，码也还在。
    fake.says(7, 42, "private", &format!("/pair {}", pairing.code));
    connect(&rig, &fake).await;

    let sent = fake.sent_count(6).await;
    assert!(
        sent[..4].iter().all(|text| text.contains("请重新生成一个")),
        "{sent:?}"
    );
    assert!(sent[4].contains("试的次数太多了"), "{sent:?}");
    assert!(sent[5].starts_with("已经连到房间「"), "{sent:?}");
    assert_eq!(fake.sent()[5].0, "42");
    let links = rig.engine.list_links().unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].external, "42");
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn updates_numbered_below_the_last_one_are_still_taken() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(
        dir.path(),
        vec![
            MockReply::text("第一句收到。"),
            MockReply::text("第二句也收到。"),
        ],
        Budget::default(),
    );
    let pairing = rig.engine.create_pairing(&rig.room).unwrap();
    let fake = FakeTelegram::default();
    fake.says(5_000, 42, "private", &format!("/pair {}", pairing.code));
    fake.says(5_001, 42, "private", "第一句");
    connect(&rig, &fake).await;
    assert_eq!(fake.sent_count(2).await[1], "第一句收到。");

    // 隔了很久（期间取过空的），Telegram 重新编号，新的编号比原来的小。
    for _ in 0..400 {
        if fake.0.lock().unwrap().offsets.last() == Some(&0) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    fake.says(17, 42, "private", "第二句");
    assert_eq!(fake.sent_count(3).await[2], "第二句也收到。");
    assert_eq!(rig.mock.requests().len(), 2);
    rig.orchestrator.shutdown().await;
}

#[tokio::test]
async fn sending_waits_when_told_to_keeps_order_and_gives_up_on_refusals() {
    let dir = tempfile::tempdir().unwrap();
    let rig = start(dir.path(), vec![], Budget::default());
    let fake = FakeTelegram::default();
    fake.0.lock().unwrap().refuse.push((
        429,
        json!({ "ok": false, "description": "Too Many Requests", "parameters": { "retry_after": 1 } }),
    ));
    let send = |chat: &str, text: &str, key: &str| {
        rig.engine
            .send_out(Source::Telegram, chat, text, key)
            .unwrap()
    };
    assert!(send("42", "一", "k1"));
    assert!(send("42", "二", "k2"));
    // 同一个键只记一次。
    assert!(!send("42", "二", "k2"));
    connect(&rig, &fake).await;

    // 第一条被限速：等够了再送，后面的不抢先。
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(fake.sent().is_empty());
    assert_eq!(fake.sent_count(2).await, ["一", "二"]);

    // 被拒绝的（对方拉黑了机器人）不再试，也不耽误别的聊天。
    fake.0.lock().unwrap().refuse.push((
        403,
        json!({ "ok": false, "description": "Forbidden: bot was blocked by the user" }),
    ));
    send("43", "三", "k3");
    send("42", &("长".repeat(3_999) + "\n" + &"话".repeat(600)), "k4");
    let sent = fake.sent_count(4).await;
    assert_eq!(sent[2].chars().count(), 4_000);
    assert_eq!(sent[3], "话".repeat(600));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fake.sent().len(), 4);
    assert!(rig
        .engine
        .pending_outbound(Source::Telegram, 10)
        .unwrap()
        .is_empty());
    rig.orchestrator.shutdown().await;
}
