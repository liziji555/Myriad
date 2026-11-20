use crate::error::AppError;
use crate::AppState;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use sa_engine::{Engine, EngineError};
use sa_orchestrator::OrchestratorError;
use sa_protocol::{
    Bot, CreateRoomRequest, Health, Message, MessagePage, PostMessageRequest, Room, RoomId,
    StopResponse,
};
use serde::Deserialize;

const DEFAULT_PAGE: u32 = 50;

/// 数据库调用是阻塞的，放到阻塞线程池里跑，不占用异步线程。
pub(crate) async fn blocking<T: Send + 'static>(
    state: &AppState,
    f: impl FnOnce(&Engine) -> Result<T, EngineError> + Send + 'static,
) -> Result<T, AppError> {
    let engine = state.engine.clone();
    tokio::task::spawn_blocking(move || f(&engine))
        .await
        .map_err(AppError::internal)?
        .map_err(AppError::from)
}

pub async fn health() -> Json<Health> {
    Json(Health {
        status: "ok".into(),
        version: env!("CARGO_PKG_VERSION").into(),
    })
}

pub async fn list_rooms(State(state): State<AppState>) -> Result<Json<Vec<Room>>, AppError> {
    Ok(Json(blocking(&state, Engine::list_rooms).await?))
}

pub async fn create_room(
    State(state): State<AppState>,
    body: Result<Json<CreateRoomRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Room>), AppError> {
    let Json(body) = body.map_err(|e| AppError::bad_request(e.body_text()))?;
    let room = blocking(&state, move |engine| engine.create_room(&body.name)).await?;
    Ok((StatusCode::CREATED, Json(room)))
}

pub async fn list_bots(State(state): State<AppState>) -> Result<Json<Vec<Bot>>, AppError> {
    Ok(Json(blocking(&state, Engine::list_bots).await?))
}

#[derive(Deserialize)]
pub struct MessagesQuery {
    /// 只要 `seq` 比它小的消息；不给就从最新的开始。
    before: Option<i64>,
    limit: Option<u32>,
}

pub async fn list_messages(
    State(state): State<AppState>,
    Path(room_id): Path<RoomId>,
    query: Result<Query<MessagesQuery>, QueryRejection>,
) -> Result<Json<MessagePage>, AppError> {
    let Query(query) = query.map_err(|e| AppError::bad_request(e.body_text()))?;
    let limit = query.limit.unwrap_or(DEFAULT_PAGE);
    let page = blocking(&state, move |engine| {
        engine.list_messages(&room_id, query.before, limit)
    })
    .await?;
    Ok(Json(page))
}

/// 发一条消息。消息落库后就返回，Bot 的回复通过事件流送达。
pub async fn post_message(
    State(state): State<AppState>,
    Path(room_id): Path<RoomId>,
    body: Result<Json<PostMessageRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Message>), AppError> {
    let Json(body) = body.map_err(|e| AppError::bad_request(e.body_text()))?;
    let message = state
        .orchestrator
        .post_message(&room_id, body.content)
        .await
        .map_err(|error| match error {
            OrchestratorError::Engine(e) => AppError::from(e),
            OrchestratorError::ShuttingDown => AppError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "shutting_down",
                "服务正在关闭，消息没有发出去",
            ),
        })?;
    Ok((StatusCode::CREATED, Json(message)))
}

/// 停下房间里正在写的回复。没有东西在写时也算成功，`stopped` 为 `false`。
pub async fn stop(
    State(state): State<AppState>,
    Path(room_id): Path<RoomId>,
) -> Result<Json<StopResponse>, AppError> {
    let room = room_id.clone();
    blocking(&state, move |engine| engine.get_room(&room)).await?;
    Ok(Json(StopResponse {
        stopped: state.orchestrator.stop(&room_id),
    }))
}

pub async fn api_not_found() -> AppError {
    AppError::new(StatusCode::NOT_FOUND, "not_found", "没有这个接口")
}
