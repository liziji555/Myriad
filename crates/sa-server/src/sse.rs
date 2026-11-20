use crate::error::AppError;
use crate::routes::blocking;
use crate::AppState;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use futures_util::stream::{self, Stream};
use sa_protocol::{Event, MessageDelta, StreamReady};
use serde::Deserialize;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, watch};

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
    live: broadcast::Receiver<MessageDelta>,
    /// 已经收到、等事件补齐之后再发的片段。
    held: Option<MessageDelta>,
    ready_sent: bool,
}

/// `GET /api/events`：事件流。
///
/// 起点依次取 `?cursor=`、`Last-Event-ID` 头（浏览器自动重连时会带）、当前最新位置。
/// 连接建立后先发一条 `ready`，随后把起点之后的事件按 `seq` 逐条发出，事件来自数据库，
/// 所以断线期间的事件不会丢。
///
/// 另有两种不带 `id` 的消息，不落库、断线不补：
/// - `delta`：正在写的消息新增的一段文字，带版本号 `rev`。客户端只在 `rev` 正好接上时追加。
/// - `resync`：片段来得太快、这条连接丢了一些。客户端应当重新拉取消息列表。
pub async fn events(
    State(state): State<AppState>,
    query: Result<Query<EventsQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, AppError> {
    let Query(query) = query.map_err(|e| AppError::bad_request(e.body_text()))?;
    // 先订阅再做别的：之后客户端拉到的消息快照不会比收到的第一个片段新。
    let live = state.engine.subscribe_live();
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
        live,
        held: None,
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
            let head = *self.wake.borrow_and_update();
            if head > self.position {
                let position = self.position;
                let batch = blocking(&self.state, move |e| e.events_after(position, BATCH))
                    .await
                    .ok()?;
                if !batch.is_empty() {
                    self.pending = batch.into();
                    continue;
                }
            }
            // 事件都发完了才轮到片段：「消息创建」总在它的第一个片段之前到达。
            if let Some(delta) = self.held.take() {
                return Some(SseEvent::default().event("delta").data(to_json(&delta)));
            }
            tokio::select! {
                changed = self.wake.changed() => changed.ok()?,
                delta = self.live.recv() => match delta {
                    Ok(delta) => self.held = Some(delta),
                    Err(RecvError::Lagged(_)) => {
                        return Some(SseEvent::default().event("resync").data("{}"));
                    }
                    Err(RecvError::Closed) => return None,
                },
                _ = self.closing.wait_for(|closing| *closing) => return None,
            }
        }
    }
}

fn to_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("协议类型总能序列化成 JSON")
}
