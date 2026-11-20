use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, COOKIE, LOCATION, SET_COOKIE};
use axum::http::{Request, Response, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use sa_engine::Engine;
use sa_protocol::{ApiError, Bot, Event, EventKind, Health, Room, StreamReady};
use sa_server::{router, AppState};
use sa_store::Store;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

const TOKEN: &str = "test-token";

fn app() -> (Router, Arc<Engine>) {
    let engine = Arc::new(Engine::new(Store::open_in_memory().unwrap()).unwrap());
    engine.ensure_defaults().unwrap();
    (router(AppState::new(engine.clone(), TOKEN)), engine)
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
