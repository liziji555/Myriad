use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, COOKIE, LOCATION, SET_COOKIE};
use axum::http::{Request, Response, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use sa_engine::Engine;
use sa_llm::MockProvider;
use sa_orchestrator::{Orchestrator, ResolvedModel, Settings, SingleModel};
use sa_protocol::{
    ApiError, Bot, Event, EventKind, Health, Message, MessageDelta, MessagePage, MessageState,
    PermissionRule, Room, RoomActivity, Sender, StepState, StopResponse, StreamReady,
};
use sa_server::{router, AppState};
use sa_store::Store;
use sa_tools::Fence;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

const TOKEN: &str = "test-token";

fn app() -> (Router, Arc<Engine>) {
    app_with(|state| state)
}

fn app_with(configure: impl FnOnce(AppState) -> AppState) -> (Router, Arc<Engine>) {
    let engine = Arc::new(Engine::new(Store::open_in_memory().unwrap()).unwrap());
    engine.ensure_defaults().unwrap();
    // 目录留在磁盘上，测试进程结束后由系统清理。
    let dir = tempfile::tempdir().unwrap().keep();
    std::fs::create_dir_all(dir.join("work")).unwrap();
    std::fs::write(dir.join("work/说明.md"), "项目代号：海豚\n").unwrap();
    let models = SingleModel(ResolvedModel {
        provider: Arc::new(MockProvider::demo().with_chunk_delay(Duration::from_millis(2))),
        model: "mock".into(),
        thinking: None,
        context_window: None,
    });
    let settings = Settings {
        sessions_dir: dir.join("sessions"),
        fence: Fence::new(&dir.join("work"), &[]).unwrap(),
        prompt: None,
        max_steps: 6,
        allow_private_network: false,
        approval_timeout: std::time::Duration::from_secs(600),
        work_dir: dir.join("tasks"),
        github: None,
        budget: Default::default(),
        watchdog: Default::default(),
        routines: Default::default(),
    };
    let orchestrator = Arc::new(Orchestrator::new(
        engine.clone(),
        Arc::new(models),
        settings,
    ));
    let state = configure(AppState::new(engine.clone(), orchestrator, TOKEN));
    (router(state), engine)
}

fn post(path: &str, body: &str) -> Request<Body> {
    Request::post(path)
        .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn messages(app: &Router, room: &Room, query: &str) -> MessagePage {
    let response = app
        .clone()
        .oneshot(get(&format!("/api/rooms/{}/messages{query}", room.id)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    json(response).await
}

/// 等到房间里有 `count` 条消息，并且都写完了。
async fn settled(app: &Router, room: &Room, count: usize) -> Vec<Message> {
    for _ in 0..500 {
        let page = messages(app, room, "").await;
        let writing = page
            .messages
            .iter()
            .any(|m| m.state == MessageState::Streaming);
        if page.messages.len() >= count && !writing {
            return page.messages;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("等不到 {count} 条写完的消息");
}

fn get(path: &str) -> Request<Body> {
    Request::get(path)
        .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap()
}

async fn json<T: serde::de::DeserializeOwned>(response: Response<Body>) -> T {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// 读取 SSE 响应体，直到攒够 `count` 条消息（以空行分隔）。
async fn read_sse(response: Response<Body>, count: usize) -> Vec<String> {
    let mut body = response.into_body();
    let mut text = String::new();
    let read = async {
        while text.matches("\n\n").count() < count {
            let frame = body.frame().await.unwrap().unwrap();
            text.push_str(std::str::from_utf8(frame.data_ref().unwrap()).unwrap());
        }
    };
    tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .expect("等待 SSE 消息超时");
    text.split("\n\n")
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .collect()
}

fn sse_data<T: serde::de::DeserializeOwned>(message: &str) -> T {
    let data = message
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .unwrap();
    serde_json::from_str(data).unwrap()
}

#[tokio::test]
async fn health_needs_no_token() {
    let (app, _) = app();
    let response = app
        .oneshot(Request::get("/api/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json::<Health>(response).await.status, "ok");
}

#[tokio::test]
async fn api_rejects_missing_or_wrong_token() {
    let (app, _) = app();
    for request in [
        Request::get("/api/rooms").body(Body::empty()).unwrap(),
        Request::get("/api/rooms")
            .header(AUTHORIZATION, "Bearer wrong")
            .body(Body::empty())
            .unwrap(),
        Request::get("/api/events").body(Body::empty()).unwrap(),
        Request::get("/api/nope").body(Body::empty()).unwrap(),
    ] {
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(json::<ApiError>(response).await.code, "unauthorized");
    }
}

#[tokio::test]
async fn auth_link_sets_cookie_that_the_api_accepts() {
    let (app, _) = app();
    let response = app
        .clone()
        .oneshot(
            Request::get(format!("/auth?token={TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[LOCATION], "/");
    let set_cookie = response.headers()[SET_COOKIE].to_str().unwrap();
    assert!(set_cookie.contains("HttpOnly"));
    assert!(set_cookie.contains("SameSite=Strict"));
    let cookie = set_cookie.split(';').next().unwrap().to_string();

    let response = app
        .clone()
        .oneshot(
            Request::get("/api/bots")
                .header(COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json::<Vec<Bot>>(response).await.len(), 1);

    let response = app
        .oneshot(
            Request::get("/auth?token=wrong")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers().get(SET_COOKIE).is_none());
}

#[tokio::test]
async fn create_room_then_list() {
    let (app, _) = app();
    let response = app
        .clone()
        .oneshot(
            Request::post("/api/rooms")
                .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"个人助理"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let created: Room = json(response).await;
    assert_eq!(created.name, "个人助理");

    let rooms: Vec<Room> = json(app.oneshot(get("/api/rooms")).await.unwrap()).await;
    assert_eq!(rooms.len(), 2);
    assert!(rooms.contains(&created));
}

#[tokio::test]
async fn create_room_with_blank_name_is_a_400() {
    let (app, _) = app();
    let response = app
        .oneshot(
            Request::post("/api/rooms")
                .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":"  "}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json::<ApiError>(response).await.code, "invalid");
}

#[tokio::test]
async fn malformed_requests_get_a_structured_400() {
    let (app, _) = app();
    let response = app
        .clone()
        .oneshot(
            Request::post("/api/rooms")
                .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"name":3}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json::<ApiError>(response).await.code, "bad_request");

    let response = app.oneshot(get("/api/events?cursor=abc")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json::<ApiError>(response).await.code, "bad_request");
}

#[tokio::test]
async fn unknown_api_path_is_a_json_404() {
    let (app, _) = app();
    let response = app.oneshot(get("/api/nope")).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(json::<ApiError>(response).await.code, "not_found");
}

#[tokio::test]
async fn events_replay_from_cursor() {
    let (app, engine) = app();
    let room = engine.create_room("回放").unwrap();

    let response = app.oneshot(get("/api/events?cursor=0")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], "text/event-stream");

    let messages = read_sse(response, 4).await;
    assert!(messages[0].contains("event: ready"));
    assert_eq!(sse_data::<StreamReady>(&messages[0]).cursor, 0);
    let events: Vec<Event> = messages[1..].iter().map(|m| sse_data(m)).collect();
    assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2, 3]);
    assert!(messages[3].contains("id: 3"));
    assert_eq!(events[2].kind, EventKind::RoomCreated);
    assert_eq!(events[2].room_id.as_ref(), Some(&room.id));
}

#[tokio::test]
async fn events_without_cursor_start_at_head_and_stream_live() {
    let (app, engine) = app();
    let head = engine.event_head();

    let response = app.oneshot(get("/api/events")).await.unwrap();
    let writer = engine.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        writer.create_room("直播").unwrap();
    });

    let messages = read_sse(response, 2).await;
    assert_eq!(sse_data::<StreamReady>(&messages[0]).cursor, head);
    let event: Event = sse_data(&messages[1]);
    assert_eq!(event.seq, head + 1);
    assert_eq!(event.kind, EventKind::RoomCreated);
}

#[tokio::test]
async fn events_resume_from_last_event_id_header() {
    let (app, engine) = app();
    engine.create_room("续传").unwrap();

    let request = Request::get("/api/events")
        .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header("Last-Event-ID", "2")
        .body(Body::empty())
        .unwrap();
    let messages = read_sse(app.oneshot(request).await.unwrap(), 2).await;
    assert_eq!(sse_data::<StreamReady>(&messages[0]).cursor, 2);
    assert_eq!(sse_data::<Event>(&messages[1]).seq, 3);
}

#[tokio::test]
async fn events_cursor_beyond_head_is_clamped() {
    let (app, engine) = app();
    let head = engine.event_head();
    let response = app.oneshot(get("/api/events?cursor=9999")).await.unwrap();
    let messages = read_sse(response, 1).await;
    assert_eq!(sse_data::<StreamReady>(&messages[0]).cursor, head);
}

#[tokio::test]
async fn missing_build_asset_is_a_404_not_the_index_page() {
    let (app, _) = app();
    let response = app.oneshot(get("/assets/missing.js")).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn post_message_streams_a_reply_over_sse() {
    let (app, engine) = app();
    let room = engine.list_rooms().unwrap().pop().unwrap();
    let stream = app.clone().oneshot(get("/api/events")).await.unwrap();

    let response = app
        .clone()
        .oneshot(post(
            &format!("/api/rooms/{}/messages", room.id),
            r#"{"content":"你好"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let sent: Message = json(response).await;
    assert_eq!(sent.sender, Sender::User);
    assert_eq!(sent.content, "你好");

    let all = settled(&app, &room, 2).await;
    let reply = &all[1];
    assert!(matches!(reply.sender, Sender::Bot { .. }));
    assert_eq!(reply.state, MessageState::Complete);
    assert!(!reply.content.is_empty());

    // 事件流里：ready、用户消息、Bot 消息创建、「它在说话」、定稿、「没人说话了」，外加每段文字一个片段。
    // 落库的事件之间保持顺序；片段和事件之间不保证先后，靠版本号对齐。
    let deltas = reply.rev as usize - 1;
    let frames = read_sse(stream, 6 + deltas).await;
    assert!(frames[0].contains("event: ready"));
    let (pieces, events): (Vec<_>, Vec<_>) = frames[1..]
        .iter()
        .partition(|frame| frame.contains("event: delta"));

    let events: Vec<Event> = events.iter().map(|f| sse_data(f)).collect();
    let kinds: Vec<_> = events.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        [
            EventKind::MessageCreated,
            EventKind::MessageCreated,
            EventKind::RoomActivity,
            EventKind::MessageUpdated,
            EventKind::RoomActivity
        ]
    );
    let (activity, events): (Vec<_>, Vec<_>) = events
        .into_iter()
        .partition(|e| e.kind == EventKind::RoomActivity);
    let activity: Vec<RoomActivity> = activity
        .into_iter()
        .map(|e| serde_json::from_value(e.payload).unwrap())
        .collect();
    assert_eq!(activity[0].speaking, room.host_bot_id);
    assert_eq!(activity[1].speaking, None);
    assert!(activity[1].queued.is_empty());
    assert_eq!(activity[1].handled_seq, sent.seq);
    let payloads: Vec<Message> = events
        .into_iter()
        .map(|e| serde_json::from_value(e.payload).unwrap())
        .collect();
    assert_eq!(payloads[0].id, sent.id);
    assert_eq!(payloads[1].id, reply.id);
    assert_eq!(payloads[1].state, MessageState::Streaming);
    assert_eq!(payloads[2], *reply);

    let mut text = String::new();
    for (index, frame) in pieces.iter().enumerate() {
        // 片段不带 id：断线重连的起点只看落库的事件。
        assert!(
            !frame.lines().any(|line| line.starts_with("id:")),
            "{frame}"
        );
        let delta: MessageDelta = sse_data(frame);
        assert_eq!(delta.message_id, reply.id);
        assert_eq!(delta.rev, index as i64 + 1);
        text.push_str(&delta.text);
    }
    assert_eq!(text, reply.content);
}

#[tokio::test]
async fn reply_can_read_a_workspace_file_and_pages_are_cut_by_seq() {
    let (app, engine) = app();
    let room = engine.list_rooms().unwrap().pop().unwrap();
    let path = format!("/api/rooms/{}/messages", room.id);
    let response = app
        .clone()
        .oneshot(post(&path, r#"{"content":"读一下 `说明.md`"}"#))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let all = settled(&app, &room, 2).await;
    let reply = &all[1];
    assert_eq!(reply.state, MessageState::Complete, "{reply:?}");
    assert_eq!(reply.steps.len(), 1);
    assert_eq!(reply.steps[0].tool, "read");
    assert_eq!(reply.steps[0].state, StepState::Ok);
    assert!(
        reply.content.contains("项目代号：海豚"),
        "{}",
        reply.content
    );

    let newest = messages(&app, &room, "?limit=1").await;
    assert!(newest.has_more);
    assert_eq!(newest.messages[0].id, reply.id);
    let older = messages(&app, &room, &format!("?limit=1&before={}", reply.seq)).await;
    assert!(!older.has_more);
    assert_eq!(older.messages[0].id, all[0].id);
}

#[tokio::test]
async fn message_endpoints_validate_input() {
    let (app, engine) = app();
    let room = engine.list_rooms().unwrap().pop().unwrap();
    let path = format!("/api/rooms/{}/messages", room.id);
    let missing = "/api/rooms/room_00000000-0000-7000-8000-000000000000";

    for (request, status, code) in [
        (
            post(&path, r#"{"content":"  "}"#),
            StatusCode::BAD_REQUEST,
            "invalid",
        ),
        (post(&path, "{"), StatusCode::BAD_REQUEST, "bad_request"),
        (post(&path, "{}"), StatusCode::BAD_REQUEST, "bad_request"),
        (
            get(&format!("{path}?limit=abc")),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            post(&format!("{missing}/messages"), r#"{"content":"在吗"}"#),
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            get(&format!("{missing}/messages")),
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            post(&format!("{missing}/stop"), ""),
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            Request::post(&path)
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"content":"没带令牌"}"#))
                .unwrap(),
            StatusCode::UNAUTHORIZED,
            "unauthorized",
        ),
    ] {
        let uri = request.uri().clone();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), status, "{uri}");
        assert_eq!(json::<ApiError>(response).await.code, code, "{uri}");
    }
    assert!(messages(&app, &room, "").await.messages.is_empty());

    // 没有东西在写时，停止是空操作。
    let response = app
        .clone()
        .oneshot(post(&format!("/api/rooms/{}/stop", room.id), ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!json::<StopResponse>(response).await.stopped);
}

#[tokio::test]
async fn slow_stream_readers_are_told_to_resync() {
    let (app, engine) = app();
    let room = engine.list_rooms().unwrap().pop().unwrap();
    let bot = engine.list_bots().unwrap().pop().unwrap();
    let stream = app.clone().oneshot(get("/api/events")).await.unwrap();

    // 没人读的时候塞进远超缓冲区的片段。
    let draft = engine.begin_bot_message(&room.id, &bot.id, 0).unwrap();
    for _ in 0..5000 {
        engine.append_delta(&draft.id, "字");
    }
    let frames = read_sse(stream, 3).await;
    assert!(frames[0].contains("event: ready"));
    assert_eq!(
        sse_data::<Event>(&frames[1]).kind,
        EventKind::MessageCreated
    );
    assert!(frames[2].contains("event: resync"), "{}", frames[2]);

    // 重新拉取能拿到完整内容。
    let page = messages(&app, &room, "").await;
    assert_eq!(page.messages[0].content.chars().count(), 5000);
    assert_eq!(page.messages[0].rev, 5000);
}

fn delete(path: &str) -> Request<Body> {
    Request::delete(path)
        .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap()
}

/// 等到最后一条消息里有一步在等批准，返回审批卡的 id。
async fn waiting_approval(app: &Router, room: &Room) -> String {
    for _ in 0..500 {
        let page = messages(app, room, "").await;
        let waiting = page.messages.last().and_then(|m| {
            m.steps
                .iter()
                .find(|s| s.state == StepState::Waiting)
                .and_then(|s| s.approval.clone())
        });
        if let Some(approval) = waiting {
            return approval.id.to_string();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("等不到审批卡");
}

/// 命令先出审批卡；「总是允许」之后同样的命令直接跑；许可可以查、可以撤销。
#[tokio::test]
async fn approval_card_is_decided_over_http_and_rules_can_be_revoked() {
    let (app, engine) = app();
    let room = engine.list_rooms().unwrap().pop().unwrap();
    let path = format!("/api/rooms/{}/messages", room.id);
    let rules_path = format!("/api/rooms/{}/rules", room.id);
    let say = |text: &str| post(&path, &serde_json::json!({ "content": text }).to_string());
    let status = |request: Request<Body>| {
        let app = app.clone();
        async move { app.oneshot(request).await.unwrap().status() }
    };

    assert_eq!(status(say("运行 `echo ok`")).await, StatusCode::CREATED);
    let id = waiting_approval(&app, &room).await;
    let decide = format!("/api/approvals/{id}/decide");
    // 答复的格式不对：400，卡片还在等。
    let response = app
        .clone()
        .oneshot(post(&decide, r#"{"decision":"maybe"}"#))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // 没带令牌：401。
    let anonymous = Request::post(&decide)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"decision":"once"}"#))
        .unwrap();
    assert_eq!(status(anonymous).await, StatusCode::UNAUTHORIZED);

    assert_eq!(
        status(post(&decide, r#"{"decision":"always"}"#)).await,
        StatusCode::NO_CONTENT
    );
    // 同一张卡再答一次：409。
    let again = app
        .clone()
        .oneshot(post(&decide, r#"{"decision":"deny"}"#))
        .await
        .unwrap();
    assert_eq!(again.status(), StatusCode::CONFLICT);
    assert_eq!(json::<ApiError>(again).await.code, "approval_closed");
    assert_eq!(
        status(post(
            "/api/approvals/apr_none/decide",
            r#"{"decision":"once"}"#
        ))
        .await,
        StatusCode::CONFLICT
    );

    let all = settled(&app, &room, 2).await;
    assert_eq!(all[1].state, MessageState::Complete, "{:?}", all[1]);
    assert_eq!(all[1].steps[0].state, StepState::Ok);
    assert!(all[1].content.contains("退出码 0"), "{}", all[1].content);

    // 许可记下来了，同样的命令不再问。
    let rules: Vec<PermissionRule> =
        json(app.clone().oneshot(get(&rules_path)).await.unwrap()).await;
    assert_eq!(rules.len(), 1);
    assert_eq!(
        (rules[0].tool.as_str(), rules[0].pattern.as_str()),
        ("shell", "echo ok")
    );
    assert_eq!(status(say("运行 `echo ok`")).await, StatusCode::CREATED);
    let all = settled(&app, &room, 4).await;
    assert_eq!(all[3].steps[0].state, StepState::Ok);
    assert_eq!(all[3].steps[0].approval, None);

    // 撤销之后又要问了。
    let rule_path = format!("/api/rules/{}", rules[0].id);
    assert_eq!(status(delete(&rule_path)).await, StatusCode::NO_CONTENT);
    assert_eq!(status(delete(&rule_path)).await, StatusCode::NOT_FOUND);
    let rules: Vec<PermissionRule> =
        json(app.clone().oneshot(get(&rules_path)).await.unwrap()).await;
    assert!(rules.is_empty());
    assert_eq!(status(say("运行 `echo ok`")).await, StatusCode::CREATED);
    let id = waiting_approval(&app, &room).await;
    assert_eq!(
        status(post(
            &format!("/api/approvals/{id}/decide"),
            r#"{"decision":"deny"}"#
        ))
        .await,
        StatusCode::NO_CONTENT
    );
    let all = settled(&app, &room, 6).await;
    assert_eq!(all[5].steps[0].state, StepState::Denied);

    assert_eq!(
        status(get("/api/rooms/room_none/rules")).await,
        StatusCode::NOT_FOUND
    );
}

fn send(method: &str, path: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Bot 的增删改、房间成员和入口 Bot 都走接口；点名另一个 Bot 时由它来回复。
#[tokio::test]
async fn bots_and_room_members_are_managed_over_http() {
    let (app, engine) = app();
    let room = engine.list_rooms().unwrap().pop().unwrap();
    let call = |request: Request<Body>| {
        let app = app.clone();
        async move { app.oneshot(request).await.unwrap() }
    };

    let response = call(post("/api/bots", r#"{"name":"Coder","persona":"写代码"}"#)).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let coder: Bot = json(response).await;
    assert_eq!(
        (coder.name.as_str(), coder.persona.as_str()),
        ("Coder", "写代码")
    );
    assert_eq!(coder.tier, sa_protocol::ModelTier::Mid);
    // 重名、名字里有空格：400，说明原因。
    for body in [
        r#"{"name":"coder"}"#,
        r#"{"name":"a b"}"#,
        r#"{"tier":"mid"}"#,
    ] {
        let response = call(post("/api/bots", body)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
    }

    let bot_path = format!("/api/bots/{}", coder.id);
    let response = call(send(
        "PATCH",
        &bot_path,
        r#"{"tier":"strong","enabled":false}"#,
    ))
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let updated: Bot = json(response).await;
    assert_eq!(
        (updated.tier, updated.enabled),
        (sa_protocol::ModelTier::Strong, false)
    );
    assert_eq!(updated.persona, "写代码", "没给的字段不变");
    let response = call(send("PATCH", &bot_path, r#"{"enabled":true}"#)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let missing = call(send("PATCH", "/api/bots/bot_none", "{}")).await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    // 拉进房间、设成入口、改房间名。
    let member_path = format!("/api/rooms/{}/members/{}", room.id, coder.id);
    let room_path = format!("/api/rooms/{}", room.id);
    let not_member = format!(r#"{{"host_bot_id":"{}"}}"#, coder.id);
    let response = call(send("PATCH", &room_path, &not_member)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let joined: Room = json(call(send("PUT", &member_path, "")).await).await;
    assert_eq!(joined.member_bot_ids.len(), 2);
    let body = format!(r#"{{"name":"新名字","host_bot_id":"{}"}}"#, coder.id);
    let changed: Room = json(call(send("PATCH", &room_path, &body)).await).await;
    assert_eq!(changed.name, "新名字");
    assert_eq!(changed.host_bot_id, Some(coder.id.clone()));

    // 点名原来的 Bot：由它回复，不是入口。
    let original = room.host_bot_id.clone().unwrap();
    let messages_path = format!("/api/rooms/{}/messages", room.id);
    let response = call(post(&messages_path, r#"{"content":"@助手 你好"}"#)).await;
    let sent: Message = json(response).await;
    assert_eq!(sent.mentions, std::slice::from_ref(&original));
    let all = settled(&app, &room, 2).await;
    assert_eq!(all[1].sender, Sender::Bot { bot_id: original });

    // 请出房间、删掉。
    let left: Room = json(call(delete(&member_path)).await).await;
    assert_eq!(left.member_bot_ids.len(), 1);
    assert_eq!(left.host_bot_id, left.member_bot_ids.first().cloned());
    assert_eq!(
        call(delete(&member_path)).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(delete(&bot_path)).await.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(delete(&bot_path)).await.status(),
        StatusCode::NOT_FOUND
    );
    let bots: Vec<Bot> = json(call(get("/api/bots")).await).await;
    assert_eq!(bots.len(), 1);
}

/// 等任务走到某个状态。
async fn task_in(
    app: &Router,
    id: &sa_protocol::TaskId,
    state: sa_protocol::TaskState,
) -> sa_protocol::TaskDetail {
    for _ in 0..1000 {
        let response = app
            .clone()
            .oneshot(get(&format!("/api/tasks/{id}")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let detail: sa_protocol::TaskDetail = json(response).await;
        let running = detail
            .runs
            .iter()
            .any(|run| run.state == sa_protocol::RunState::Running);
        if detail.task.state == state && !running {
            return detail;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("任务没有变成 {state:?}");
}

#[tokio::test]
async fn a_task_runs_is_rejected_then_accepted_over_http() {
    use sa_protocol::{EventKind, Task, TaskState};
    let (app, engine) = app();
    let room = engine.list_rooms().unwrap().remove(0);
    let bot = room.host_bot_id.clone().unwrap();
    let tasks_path = format!("/api/rooms/{}/tasks", room.id);

    // 没写做什么：400。没有负责人：草稿，不会开工。
    let response = app
        .clone()
        .oneshot(post(&tasks_path, r#"{"title":"空的","scope":" "}"#))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = app
        .clone()
        .oneshot(post(
            &tasks_path,
            r#"{"title":"先放着","scope":"以后再说"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let draft: Task = json(response).await;
    assert_eq!(draft.state, TaskState::Draft);

    let body = serde_json::json!({
        "title": "写笔记",
        "scope": "写入 `笔记.md`：第一版",
        "owner_bot_id": bot,
    });
    let response = app
        .clone()
        .oneshot(post(&tasks_path, &body.to_string()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let task: Task = json(response).await;
    assert_eq!(task.state, TaskState::Open);

    let detail = task_in(&app, &task.id, TaskState::Review).await;
    assert_eq!(detail.runs.len(), 1);
    assert_eq!(detail.task.artifacts[0].reference, "笔记.md");

    // 等验收的任务：不能「继续」，打回要写意见，可以不带请求体的不带。
    let action = |name: &str| format!("/api/tasks/{}/{name}", task.id);
    let response = app
        .clone()
        .oneshot(post(&action("resume"), ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = app
        .clone()
        .oneshot(post(&action("reject"), ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = app
        .clone()
        .oneshot(post(&action("reject"), "{oops"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = app
        .clone()
        .oneshot(post(
            &action("reject"),
            r#"{"note":"写入 `笔记.md`：第二版"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let detail = task_in(&app, &task.id, TaskState::Review).await;
    assert_eq!(detail.runs.len(), 2);

    // 没有运行在进行时点停止：成功，但什么都没停。
    let response = app
        .clone()
        .oneshot(post(&action("stop"), ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let stopped: sa_protocol::StopResponse = json(response).await;
    assert!(!stopped.stopped);

    let response = app
        .clone()
        .oneshot(post(&action("accept"), ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let done: Task = json(response).await;
    assert_eq!(done.state, TaskState::Done);
    // 验收过的不能再验收、也不能取消。
    let response = app
        .clone()
        .oneshot(post(&action("accept"), ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = app
        .clone()
        .oneshot(post(&action("cancel"), ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // 草稿可以取消；列表里两个任务都在。
    let response = app
        .clone()
        .oneshot(post(
            &format!("/api/tasks/{}/cancel", draft.id),
            r#"{"note":"不做了"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let listed: Vec<Task> = json(app.clone().oneshot(get(&tasks_path)).await.unwrap()).await;
    let states: Vec<_> = listed.iter().map(|t| t.state).collect();
    assert_eq!(states, [TaskState::Cancelled, TaskState::Done]);
    let response = app
        .clone()
        .oneshot(get("/api/tasks/task_nope"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // 任务和运行的变化都走事件流。
    let kinds: Vec<_> = engine
        .events_after(0, 10_000)
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    for kind in [
        EventKind::TaskCreated,
        EventKind::TaskUpdated,
        EventKind::RunUpdated,
    ] {
        assert!(kinds.contains(&kind), "{kind:?}");
    }
}

#[tokio::test]
async fn a_team_is_exported_and_imported_over_the_api() {
    let (app, engine) = app();
    let room = engine.list_rooms().unwrap().remove(0);
    let coder = engine
        .create_bot("Coder", sa_protocol::ModelTier::Strong, "写代码")
        .unwrap();
    engine.add_room_member(&room.id, &coder.id).unwrap();
    engine.update_room(&room.id, None, Some(&coder.id)).unwrap();

    let path = format!("/api/templates/rooms/{}", room.id);
    let team: sa_protocol::TeamTemplate =
        json(app.clone().oneshot(get(&path)).await.unwrap()).await;
    assert_eq!(team.version, 1);
    assert_eq!(team.bots.len(), 2);
    let wanted = team.room.clone().unwrap();
    assert_eq!(wanted.host.as_deref(), Some("Coder"));
    let everyone: sa_protocol::TeamTemplate = json(
        app.clone()
            .oneshot(get("/api/templates/bots"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!((everyone.bots.len(), everyone.room), (2, None));

    // 导回同一个地方：Bot 都在，只多一个房间。
    let body = serde_json::to_string(&team).unwrap();
    let response = app
        .clone()
        .oneshot(post("/api/templates/import", &body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let report: sa_protocol::ImportReport = json(response).await;
    assert!(report.created.is_empty());
    assert_eq!(report.skipped.len(), 2);
    let made = report.room.unwrap();
    assert_eq!(made.host_bot_id.as_ref(), Some(&coder.id));
    assert_eq!(engine.list_rooms().unwrap().len(), 2);

    // 加一个新的 Bot 再导：只建这一个。
    let more = r#"{"version":1,"bots":[{"name":"Tester","tier":"cheap"}]}"#;
    let response = app
        .clone()
        .oneshot(post("/api/templates/import", more))
        .await
        .unwrap();
    let report: sa_protocol::ImportReport = json(response).await;
    assert_eq!(report.created.len(), 1);
    assert!(report.created[0].enabled && report.created[0].persona.is_empty());

    // 版本不对、缺版本、不是 JSON：都是 400，什么都不建。
    for bad in [r#"{"version":2,"bots":[]}"#, r#"{"bots":[]}"#, "不是 JSON"] {
        let response = app
            .clone()
            .oneshot(post("/api/templates/import", bad))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
    assert_eq!(engine.list_bots().unwrap().len(), 3);
    let missing = format!("/api/templates/rooms/{}", sa_protocol::RoomId::new());
    let response = app.clone().oneshot(get(&missing)).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = app
        .oneshot(
            Request::get("/api/templates/bots")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_audit_log_is_served_filtered_and_paged() {
    let (app, engine) = app();
    let room = engine.list_rooms().unwrap().remove(0);
    let created = app
        .clone()
        .oneshot(post("/api/bots", r#"{"name":"Coder","persona":"写代码"}"#))
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let other = engine.create_room("另一个").unwrap();
    engine.link_repo(&room.id, "acme/web").unwrap();

    let all: sa_protocol::AuditPage =
        json(app.clone().oneshot(get("/api/audit")).await.unwrap()).await;
    let actions: Vec<&str> = all.entries.iter().map(|e| e.action.as_str()).collect();
    assert_eq!(actions, ["link.created", "room.created", "bot.created"]);
    assert!(!all.has_more);

    let in_room = format!("/api/audit?room={}", room.id);
    let page: sa_protocol::AuditPage =
        json(app.clone().oneshot(get(&in_room)).await.unwrap()).await;
    assert_eq!(page.entries.len(), 1);
    assert_eq!(
        page.entries[0].summary,
        "把 GitHub 仓库 acme/web 连到了房间"
    );
    let elsewhere = format!("/api/audit?room={}&action=link.", other.id);
    let page: sa_protocol::AuditPage =
        json(app.clone().oneshot(get(&elsewhere)).await.unwrap()).await;
    assert!(page.entries.is_empty());

    // 翻页：一次一条，接着上一页最后一条往前。
    let first: sa_protocol::AuditPage = json(
        app.clone()
            .oneshot(get("/api/audit?limit=1"))
            .await
            .unwrap(),
    )
    .await;
    assert!(first.has_more);
    let next = format!(
        "/api/audit?limit=1&before={}&action=bot.",
        first.entries[0].id
    );
    let second: sa_protocol::AuditPage = json(app.clone().oneshot(get(&next)).await.unwrap()).await;
    assert_eq!(second.entries[0].action, "bot.created");
    assert!(!second.has_more);

    let bad = app
        .clone()
        .oneshot(get("/api/audit?limit=abc"))
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    // 没带令牌的看不了。
    let anonymous = Request::get("/api/audit").body(Body::empty()).unwrap();
    let refused = app.clone().oneshot(anonymous).await.unwrap();
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn usage_and_notices_are_served_and_a_notice_can_be_dismissed() {
    let (app, engine) = app();
    let response = app.clone().oneshot(get("/api/usage")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let usage: sa_protocol::UsageSummary = json(response).await;
    assert_eq!(usage.today.calls, 0);
    assert_eq!(usage.days.len(), 7);

    let notice = engine
        .notify(
            "test:1",
            sa_engine::NewNotice {
                kind: sa_protocol::NoticeKind::Budget,
                level: sa_protocol::NoticeLevel::Warning,
                room_id: None,
                task_id: None,
                text: "今天的花费到上限了".into(),
            },
        )
        .unwrap()
        .unwrap();
    // 同一个 key 只记一次。
    let again = sa_engine::NewNotice {
        kind: sa_protocol::NoticeKind::Budget,
        level: sa_protocol::NoticeLevel::Info,
        room_id: None,
        task_id: None,
        text: "重复的".into(),
    };
    assert!(engine.notify("test:1", again).unwrap().is_none());

    let response = app.clone().oneshot(get("/api/notices")).await.unwrap();
    let notices: Vec<sa_protocol::Notice> = json(response).await;
    assert_eq!(notices, std::slice::from_ref(&notice));

    let path = format!("/api/notices/{}/dismiss", notice.id);
    let response = app.clone().oneshot(post(&path, "")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let dismissed: sa_protocol::Notice = json(response).await;
    assert!(dismissed.dismissed_at.is_some());
    let response = app.clone().oneshot(get("/api/notices")).await.unwrap();
    assert!(json::<Vec<sa_protocol::Notice>>(response).await.is_empty());

    let response = app
        .oneshot(post("/api/notices/ntc_nope/dismiss", ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(json::<ApiError>(response).await.code, "not_found");
}

#[tokio::test]
async fn routines_are_created_changed_and_deleted() {
    let (app, engine) = app();
    let call = |request: Request<Body>| {
        let app = app.clone();
        async move { app.oneshot(request).await.unwrap() }
    };
    let room = engine.list_rooms().unwrap().remove(0);
    let body = serde_json::json!({
        "room_id": room.id, "title": "早报", "prompt": "看看新消息", "schedule": "0 9 * * *"
    })
    .to_string();
    let response = call(post("/api/routines", &body)).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let routine: sa_protocol::Routine = json(response).await;
    assert_eq!(Some(&routine.bot_id), room.host_bot_id.as_ref());
    assert!(routine.enabled && routine.next_at.is_some());

    // 时间写错：400，说明原因。
    let bad = body.replace("0 9 * * *", "每天九点");
    let response = call(post("/api/routines", &bad)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let listed: Vec<sa_protocol::Routine> = json(call(get("/api/routines")).await).await;
    assert_eq!(listed, std::slice::from_ref(&routine));

    let path = format!("/api/routines/{}", routine.id);
    let response = call(send("PATCH", &path, r#"{"enabled":false}"#)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let stopped: sa_protocol::Routine = json(response).await;
    assert!(!stopped.enabled && stopped.next_at.is_none());

    let response = call(send("PATCH", "/api/routines/rtn_none", "{}")).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = call(post("/api/routines/rtn_none/run", "")).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    assert_eq!(call(delete(&path)).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(call(delete(&path)).await.status(), StatusCode::NOT_FOUND);
    assert!(engine.list_routines().unwrap().is_empty());
}

/// 一次 GitHub 投递：不带访问令牌，带签名。
fn delivery(event: &str, id: &str, secret: &str, body: &str) -> Request<Body> {
    Request::post("/hooks/github")
        .header(CONTENT_TYPE, "application/json")
        .header("X-GitHub-Event", event)
        .header("X-GitHub-Delivery", id)
        .header(
            "X-Hub-Signature-256",
            sa_adapters::github_events::sign(secret.as_bytes(), body.as_bytes()),
        )
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn github_deliveries_are_verified_and_taken_once() {
    let issue = serde_json::json!({
        "action": "opened",
        "repository": { "full_name": "Me/App" },
        "sender": { "login": "alice", "type": "User" },
        "issue": { "number": 12, "title": "登录报错", "body": "点登录就 500", "labels": [] },
    })
    .to_string();

    // 没配密钥：什么都不收。
    let (closed, _) = app();
    let response = closed
        .oneshot(delivery("issues", "d-1", "s3cret", &issue))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    let (app, engine) = app_with(|state| state.with_github_hook("s3cret", Default::default()));
    let call = |request: Request<Body>| {
        let app = app.clone();
        async move { app.oneshot(request).await.unwrap() }
    };
    let outcome = |response: Response<Body>| async move {
        assert_eq!(response.status(), StatusCode::OK);
        let answer: serde_json::Value = json(response).await;
        answer["outcome"].as_str().unwrap().to_string()
    };
    let room = engine.list_rooms().unwrap().remove(0);

    // 签名不对（密钥不对、内容被改过）：401，什么都不记。
    let response = call(delivery("issues", "d-1", "wrong", &issue)).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let mut tampered = delivery("issues", "d-1", "s3cret", &issue);
    *tampered.body_mut() = Body::from(issue.replace("alice", "mallory"));
    assert_eq!(call(tampered).await.status(), StatusCode::UNAUTHORIZED);

    assert_eq!(
        outcome(call(delivery("ping", "d-0", "s3cret", "{}")).await).await,
        "pong"
    );
    let push = r#"{"repository":{"full_name":"Me/App"}}"#;
    assert_eq!(
        outcome(call(delivery("push", "d-p", "s3cret", push)).await).await,
        "ignored"
    );

    // 仓库还没连到房间：送到了，但是不收。
    assert_eq!(
        outcome(call(delivery("issues", "d-1", "s3cret", &issue)).await).await,
        "filtered"
    );

    let body = serde_json::json!({ "room_id": room.id, "repo": "Me/App" }).to_string();
    let response = call(post("/api/links", &body)).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let link: sa_protocol::Link = json(response).await;
    assert_eq!(link.external, "me/app");
    assert_eq!(
        call(post("/api/links", &body)).await.status(),
        StatusCode::BAD_REQUEST
    );
    let listed: Vec<sa_protocol::Link> = json(call(get("/api/links")).await).await;
    assert_eq!(listed, std::slice::from_ref(&link));

    assert_eq!(
        outcome(call(delivery("issues", "d-2", "s3cret", &issue)).await).await,
        "stored"
    );
    // GitHub 重试同一次投递：不会变成第二条消息。
    assert_eq!(
        outcome(call(delivery("issues", "d-2", "s3cret", &issue)).await).await,
        "duplicate"
    );
    let events: Vec<Message> = engine
        .list_messages(&room.id, None, 50)
        .unwrap()
        .messages
        .into_iter()
        .filter(|message| message.kind == sa_protocol::MessageKind::Event)
        .collect();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].origin.as_ref().unwrap().label, "Me/App#12");
    assert_eq!(
        events[0].mentions,
        room.host_bot_id.clone().into_iter().collect::<Vec<_>>()
    );

    let path = format!("/api/links/{}", link.id);
    assert_eq!(call(delete(&path)).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(call(delete(&path)).await.status(), StatusCode::NOT_FOUND);

    // 配对码：只给一次，十分钟内有效。
    let response = call(post(&format!("/api/rooms/{}/pairing", room.id), "")).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let pairing: sa_protocol::Pairing = json(response).await;
    assert_eq!(pairing.code.len(), 6);
    assert_eq!(pairing.bot_username, None);
    let linked = engine.redeem_pairing(&pairing.code, "42", "张三").unwrap();
    assert_eq!(linked.unwrap().room_id, room.id);
}

#[tokio::test]
async fn extensions_list_skills_from_disk_each_time() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    let orchestrator = Arc::new(std::sync::Mutex::new(None));
    let slot = orchestrator.clone();
    let (app, _) = app_with(move |state| {
        *slot.lock().unwrap() = Some(state.orchestrator());
        state
    });
    let found: sa_protocol::Extensions =
        json(app.clone().oneshot(get("/api/extensions")).await.unwrap()).await;
    assert_eq!(found, sa_protocol::Extensions::default());

    let orchestrator = orchestrator.lock().unwrap().clone().unwrap();
    orchestrator.use_skills(skills.clone());
    std::fs::create_dir_all(&skills).unwrap();
    std::fs::write(skills.join("review.md"), "# 代码评审\n先看测试。\n").unwrap();
    std::fs::write(skills.join("bad name.md"), "正文").unwrap();
    let found: sa_protocol::Extensions =
        json(app.clone().oneshot(get("/api/extensions")).await.unwrap()).await;
    assert_eq!(found.skills.len(), 1);
    assert_eq!(found.skills[0].name, "review");
    assert_eq!(found.skills[0].description, "代码评审");
    assert_eq!(found.skill_problems.len(), 1);
    assert!(found.mcp.is_empty());

    // 没带访问令牌的看不到。
    let anonymous = Request::get("/api/extensions").body(Body::empty()).unwrap();
    let response = app.oneshot(anonymous).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
