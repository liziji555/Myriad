use crate::error::AppError;
use crate::routes::blocking;
use crate::AppState;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use futures_util::stream::{self, Stream};
use sa_protocol::{Event, StreamReady};
use serde::Deserialize;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::time::Duration;
use tokio::sync::watch;

const BATCH: u32 = 256;

#[derive(Deserialize)]
pub struct EventsQuery {
    cursor: Option<i64>,
}

struct Cursor {
    state: AppState,
    wake: watch::Receiver<i64>,
    closing: watch::Receiver<bool>,
    position: i64,
    pending: VecDeque<Event>,
    ready_sent: bool,
}

/// `GET /api/events`：事件流。
///
/// 起点依次取 `?cursor=`、`Last-Event-ID` 头（浏览器自动重连时会带）、当前最新位置。
/// 连接建立后先发一条 `ready`，随后把起点之后的事件按 `seq` 逐条发出，事件来自数据库，
/// 所以断线期间的事件不会丢。
pub async fn events(
    State(state): State<AppState>,
    query: Result<Query<EventsQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, AppError> {
    let Query(query) = query.map_err(|e| AppError::bad_request(e.body_text()))?;
    let wake = state.engine.subscribe();
    let closing = state.closing.subscribe();
    let head = state.engine.event_head();
    let requested = query.cursor.or_else(|| {
        headers
            .get("last-event-id")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
    });
    let cursor = Cursor {
        state,
        wake,
        closing,
        position: requested.unwrap_or(head).clamp(0, head),
        pending: VecDeque::new(),
        ready_sent: false,
    };
    let stream = stream::unfold(cursor, |mut cursor| async move {
        let event = cursor.next().await?;
        Some((Ok(event), cursor))
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

impl Cursor {
    /// 下一条要发的消息；返回 `None` 表示结束这条连接。
    async fn next(&mut self) -> Option<SseEvent> {
        if *self.closing.borrow() {
            return None;
        }
        if !self.ready_sent {
            self.ready_sent = true;
            let ready = StreamReady {
                cursor: self.position,
            };
            return Some(
                SseEvent::default()
                    .event("ready")
                    .id(self.position.to_string())
                    .data(to_json(&ready)),
            );
        }
        loop {
            if let Some(event) = self.pending.pop_front() {
                self.position = event.seq;
                return Some(
                    SseEvent::default()
                        .id(event.seq.to_string())
                        .data(to_json(&event)),
                );
            }
            // 先记下「已看到的最新位置」再查库：查完之后才写入的事件会让 changed() 立刻返回。
            self.wake.borrow_and_update();
            let position = self.position;
            let batch = blocking(&self.state, move |e| e.events_after(position, BATCH))
                .await
                .ok()?;
            if batch.is_empty() {
                tokio::select! {
                    changed = self.wake.changed() => changed.ok()?,
                    _ = self.closing.wait_for(|closing| *closing) => return None,
                }
            } else {
                self.pending = batch.into();
            }
        }
    }
}

fn to_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("协议类型总能序列化成 JSON")
}
