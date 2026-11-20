use crate::error::AppError;
use crate::AppState;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use sa_engine::{BotPatch, Engine, EngineError};
use sa_orchestrator::OrchestratorError;
use sa_protocol::{
    ApprovalId, Bot, BotId, CreateBotRequest, CreateRoomRequest, DecideApprovalRequest, Health,
    Message, MessagePage, ModelTier, PermissionRule, PostMessageRequest, Room, RoomId, RuleId,
    StopResponse, UpdateBotRequest, UpdateRoomRequest,
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

/// 答复一张审批卡。卡片已经不在等了（答过、过期、那一轮停了）时返回 409。
pub async fn decide_approval(
    State(state): State<AppState>,
    Path(approval_id): Path<ApprovalId>,
    body: Result<Json<DecideApprovalRequest>, JsonRejection>,
) -> Result<StatusCode, AppError> {
    let Json(body) = body.map_err(|e| AppError::bad_request(e.body_text()))?;
    if state.orchestrator.decide(&approval_id, body.decision) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::new(
            StatusCode::CONFLICT,
            "approval_closed",
            "这张审批卡已经处理过或者过期了",
        ))
    }
}

/// 房间里已经保存的「总是允许」。
pub async fn list_rules(
    State(state): State<AppState>,
    Path(room_id): Path<RoomId>,
) -> Result<Json<Vec<PermissionRule>>, AppError> {
    Ok(Json(
        blocking(&state, move |engine| engine.list_rules(&room_id)).await?,
    ))
}

/// 撤销一条「总是允许」。
pub async fn delete_rule(
    State(state): State<AppState>,
    Path(rule_id): Path<RuleId>,
) -> Result<StatusCode, AppError> {
    if blocking(&state, move |engine| engine.delete_rule(&rule_id)).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "没有这条许可",
        ))
    }
}

pub async fn create_bot(
    State(state): State<AppState>,
    body: Result<Json<CreateBotRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Bot>), AppError> {
    let Json(body) = body.map_err(|e| AppError::bad_request(e.body_text()))?;
    let bot = blocking(&state, move |engine| {
        engine.create_bot(
            &body.name,
            body.tier.unwrap_or(ModelTier::Mid),
            body.persona.as_deref().unwrap_or(""),
        )
    })
    .await?;
    Ok((StatusCode::CREATED, Json(bot)))
}

/// 改一个 Bot 的名字、档位、人设，或者停用、启用它。只改请求里给出的字段。
pub async fn update_bot(
    State(state): State<AppState>,
    Path(bot_id): Path<BotId>,
    body: Result<Json<UpdateBotRequest>, JsonRejection>,
) -> Result<Json<Bot>, AppError> {
    let Json(body) = body.map_err(|e| AppError::bad_request(e.body_text()))?;
    let patch = BotPatch {
        name: body.name,
        tier: body.tier,
        persona: body.persona,
        enabled: body.enabled,
    };
    Ok(Json(
        blocking(&state, move |engine| engine.update_bot(&bot_id, patch)).await?,
    ))
}

/// 删掉一个 Bot。它发过的消息留着。
pub async fn delete_bot(
    State(state): State<AppState>,
    Path(bot_id): Path<BotId>,
) -> Result<StatusCode, AppError> {
    blocking(&state, move |engine| engine.delete_bot(&bot_id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// 改房间的名字或入口 Bot。
pub async fn update_room(
    State(state): State<AppState>,
    Path(room_id): Path<RoomId>,
    body: Result<Json<UpdateRoomRequest>, JsonRejection>,
) -> Result<Json<Room>, AppError> {
    let Json(body) = body.map_err(|e| AppError::bad_request(e.body_text()))?;
    Ok(Json(
        blocking(&state, move |engine| {
            engine.update_room(&room_id, body.name.as_deref(), body.host_bot_id.as_ref())
        })
        .await?,
    ))
}

/// 把一个 Bot 拉进房间。已经在里面时什么都不变。
pub async fn add_member(
    State(state): State<AppState>,
    Path((room_id, bot_id)): Path<(RoomId, BotId)>,
) -> Result<Json<Room>, AppError> {
    Ok(Json(
        blocking(&state, move |engine| {
            engine.add_room_member(&room_id, &bot_id)
        })
        .await?,
    ))
}

/// 把一个 Bot 请出房间。
pub async fn remove_member(
    State(state): State<AppState>,
    Path((room_id, bot_id)): Path<(RoomId, BotId)>,
) -> Result<Json<Room>, AppError> {
    Ok(Json(
        blocking(&state, move |engine| {
            engine.remove_room_member(&room_id, &bot_id)
        })
        .await?,
    ))
}

pub async fn api_not_found() -> AppError {
    AppError::new(StatusCode::NOT_FOUND, "not_found", "没有这个接口")
}
