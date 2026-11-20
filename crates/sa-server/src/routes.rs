use crate::error::AppError;
use crate::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use sa_engine::{Engine, EngineError};
use sa_protocol::{Bot, CreateRoomRequest, Health, Room};

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
    Json(body): Json<CreateRoomRequest>,
) -> Result<(StatusCode, Json<Room>), AppError> {
    let room = blocking(&state, move |engine| engine.create_room(&body.name)).await?;
    Ok((StatusCode::CREATED, Json(room)))
}

pub async fn list_bots(State(state): State<AppState>) -> Result<Json<Vec<Bot>>, AppError> {
    Ok(Json(blocking(&state, Engine::list_bots).await?))
}

pub async fn api_not_found() -> AppError {
    AppError::new(StatusCode::NOT_FOUND, "not_found", "没有这个接口")
}
