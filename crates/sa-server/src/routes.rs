use crate::error::AppError;
use crate::AppState;
use axum::body::Bytes;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use sa_engine::{AuditFilter, BotPatch, Engine, EngineError, RoutinePatch};
use sa_protocol::{
    ApprovalId, AuditPage, Bot, BotId, CreateBotRequest, CreateLinkRequest, CreateRoomRequest,
    CreateRoutineRequest, CreateTaskRequest, DecideApprovalRequest, EditQueuedRequest, Extensions,
    HarnessDesk, Health, ImportReport, ImportTemplateRequest, Link, LinkId, Message, MessagePage,
    ModelSettings, ModelTier, Notice, NoticeId, Pairing, PermissionRule, PostMessageRequest,
    QueuedId, Room, RoomId, RoomQueue, Routine, RoutineId, RuleId, SaveModelsRequest, Share,
    ShareCall, SharePoll, ShareReply, StopResponse, Task, TaskActionRequest, TaskDetail, TaskId,
    TeamTemplate, TemplateFile, TestModelRequest, TestModelResult, UpdateBotRequest,
    UpdateRoomRequest, UpdateRoutineRequest, UsageSummary,
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
        .await?;
    Ok((StatusCode::CREATED, Json(message)))
}

/// 房间的待发队列，排在前面的先发。
pub async fn list_queue(
    State(state): State<AppState>,
    Path(room_id): Path<RoomId>,
) -> Result<Json<RoomQueue>, AppError> {
    let queue = blocking(&state, move |engine| engine.list_queue(&room_id)).await?;
    Ok(Json(queue))
}

/// Bot 忙的时候说的话排到队尾，房间闲下来按顺序发出去。
pub async fn queue_message(
    State(state): State<AppState>,
    Path(room_id): Path<RoomId>,
    body: Result<Json<PostMessageRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<RoomQueue>), AppError> {
    let Json(body) = body.map_err(|e| AppError::bad_request(e.body_text()))?;
    let queue = state
        .orchestrator
        .queue_message(&room_id, body.content)
        .await?;
    Ok((StatusCode::CREATED, Json(queue)))
}

/// 改排着的一句：改字、往前或往后挪一位。已经发出去的是 404。
pub async fn edit_queued(
    State(state): State<AppState>,
    Path(queued_id): Path<QueuedId>,
    body: Result<Json<EditQueuedRequest>, JsonRejection>,
) -> Result<Json<RoomQueue>, AppError> {
    let Json(body) = body.map_err(|e| AppError::bad_request(e.body_text()))?;
    let queue = blocking(&state, move |engine| engine.edit_queued(&queued_id, &body)).await?;
    Ok(Json(queue))
}

/// 取消排着的一句。已经发出去的是 404。
pub async fn cancel_queued(
    State(state): State<AppState>,
    Path(queued_id): Path<QueuedId>,
) -> Result<Json<RoomQueue>, AppError> {
    let queue = blocking(&state, move |engine| engine.cancel_queued(&queued_id)).await?;
    Ok(Json(queue))
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

/// 只停下房间里这一个 Bot 正在写的回复（它排着的也不再说），别的 Bot 照常。
pub async fn stop_bot(
    State(state): State<AppState>,
    Path((room_id, bot_id)): Path<(RoomId, BotId)>,
) -> Result<Json<StopResponse>, AppError> {
    let room = room_id.clone();
    blocking(&state, move |engine| engine.get_room(&room)).await?;
    Ok(Json(StopResponse {
        stopped: state.orchestrator.stop_bot(&room_id, &bot_id),
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
        engine.create_harness_bot(
            &body.name,
            body.tier.unwrap_or(ModelTier::Mid),
            body.persona.as_deref().unwrap_or(""),
            body.harness.unwrap_or_default(),
            body.harness_command.as_deref().unwrap_or(""),
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
        harness: body.harness,
        harness_command: body.harness_command,
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
        blocking(&state, move |engine| engine.edit_room(&room_id, &body)).await?,
    ))
}

/// 房间里 harness 成员开着的独立目录，和里面改了哪些文件。
pub async fn list_desks(
    State(state): State<AppState>,
    Path(room_id): Path<RoomId>,
) -> Result<Json<Vec<HarnessDesk>>, AppError> {
    let room = room_id.clone();
    blocking(&state, move |engine| engine.get_room(&room)).await?;
    Ok(Json(state.orchestrator.room_desks(&room_id).await?))
}

/// 入口 Bot 在这个房间里的对话占了上下文窗口多少。
pub async fn room_context(
    State(state): State<AppState>,
    Path(room_id): Path<RoomId>,
) -> Result<Json<Option<sa_protocol::ContextUsage>>, AppError> {
    Ok(Json(state.orchestrator.room_context(&room_id).await?))
}

/// 把 harness 工位里的改动带回工作目录。带不回去时返回 409，工位留着。
pub async fn accept_desk(
    State(state): State<AppState>,
    Path((room_id, bot_id)): Path<(RoomId, BotId)>,
) -> Result<Json<Vec<HarnessDesk>>, AppError> {
    Ok(Json(
        state.orchestrator.accept_desk(&room_id, &bot_id).await?,
    ))
}

/// 丢掉 harness 工位里的改动。
pub async fn discard_desk(
    State(state): State<AppState>,
    Path((room_id, bot_id)): Path<(RoomId, BotId)>,
) -> Result<Json<Vec<HarnessDesk>>, AppError> {
    Ok(Json(
        state.orchestrator.discard_desk(&room_id, &bot_id).await?,
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

/// 房间里的全部任务，按创建时间升序。
pub async fn list_tasks(
    State(state): State<AppState>,
    Path(room_id): Path<RoomId>,
) -> Result<Json<Vec<Task>>, AppError> {
    Ok(Json(
        blocking(&state, move |engine| engine.list_tasks(&room_id)).await?,
    ))
}

/// 人开一个任务。指定了负责人就排队开工，没指定就是草稿。
pub async fn create_task(
    State(state): State<AppState>,
    Path(room_id): Path<RoomId>,
    body: Result<Json<CreateTaskRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Task>), AppError> {
    let Json(body) = body.map_err(|e| AppError::bad_request(e.body_text()))?;
    let task = state.orchestrator.create_task(&room_id, body).await?;
    Ok((StatusCode::CREATED, Json(task)))
}

/// 一个任务和它的执行历史。
pub async fn get_task(
    State(state): State<AppState>,
    Path(task_id): Path<TaskId>,
) -> Result<Json<TaskDetail>, AppError> {
    let detail = blocking(&state, move |engine| {
        Ok(TaskDetail {
            task: engine.get_task(&task_id)?,
            runs: engine.task_runs(&task_id)?,
        })
    })
    .await?;
    Ok(Json(detail))
}

/// 任务操作的请求体可以不带；带了就必须是合法的 JSON。
fn action_body(body: &Bytes) -> Result<TaskActionRequest, AppError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(TaskActionRequest::default());
    }
    serde_json::from_slice(body).map_err(|e| AppError::bad_request(format!("请求体不对：{e}")))
}

/// 让任务继续：草稿、等回答、卡住的任务重新排队。`note` 是给负责的 Bot 的话，任务正在做时只把话带到。
pub async fn resume_task(
    State(state): State<AppState>,
    Path(task_id): Path<TaskId>,
    body: Bytes,
) -> Result<Json<Task>, AppError> {
    let body = action_body(&body)?;
    let note = body.note.unwrap_or_default();
    Ok(Json(
        state
            .orchestrator
            .resume_task(&task_id, note, body.owner_bot_id)
            .await?,
    ))
}

/// 验收通过：任务的改动带回工作目录。带不回去时返回 409，任务留在「等验收」。
pub async fn accept_task(
    State(state): State<AppState>,
    Path(task_id): Path<TaskId>,
) -> Result<Json<Task>, AppError> {
    Ok(Json(state.orchestrator.accept_task(&task_id).await?))
}

/// 验收不通过：带着 `note` 里的意见打回重做。
pub async fn reject_task(
    State(state): State<AppState>,
    Path(task_id): Path<TaskId>,
    body: Bytes,
) -> Result<Json<Task>, AppError> {
    let note = action_body(&body)?.note.unwrap_or_default();
    Ok(Json(state.orchestrator.reject_task(&task_id, note).await?))
}

/// 停下任务正在进行的运行。没有运行在进行时也算成功，`stopped` 为 `false`。
pub async fn stop_task(
    State(state): State<AppState>,
    Path(task_id): Path<TaskId>,
) -> Result<Json<StopResponse>, AppError> {
    let id = task_id.clone();
    blocking(&state, move |engine| engine.get_task(&id)).await?;
    Ok(Json(StopResponse {
        stopped: state.orchestrator.stop_task(&task_id),
    }))
}

/// 取消任务。
pub async fn cancel_task(
    State(state): State<AppState>,
    Path(task_id): Path<TaskId>,
    body: Bytes,
) -> Result<Json<Task>, AppError> {
    let note = action_body(&body)?.note.unwrap_or_default();
    Ok(Json(state.orchestrator.cancel_task(&task_id, note).await?))
}

/// 「用量」面板：今天和最近几天花了多少，按模型、Bot、任务分。
pub async fn usage(State(state): State<AppState>) -> Result<Json<UsageSummary>, AppError> {
    Ok(Json(state.orchestrator.usage_summary().await?))
}

#[derive(Deserialize)]
pub struct AuditQuery {
    room: Option<RoomId>,
    task: Option<TaskId>,
    /// 动作名的开头，例如 `approval.`。
    action: Option<String>,
    /// 只要 `id` 比它小的；不给就从最新的开始。
    before: Option<i64>,
    limit: Option<u32>,
}

/// 审计日志，新的在前。可以按房间、任务、动作找。
pub async fn audit(
    State(state): State<AppState>,
    query: Result<Query<AuditQuery>, QueryRejection>,
) -> Result<Json<AuditPage>, AppError> {
    let Query(query) = query.map_err(|e| AppError::bad_request(e.body_text()))?;
    let filter = AuditFilter {
        room_id: query.room,
        task_id: query.task,
        action: query.action.filter(|action| !action.trim().is_empty()),
        before: query.before,
    };
    let limit = query.limit.unwrap_or(DEFAULT_PAGE);
    Ok(Json(
        blocking(&state, move |engine| engine.list_audit(&filter, limit)).await?,
    ))
}

/// 所有 Bot 导出成一份模板。
pub async fn export_bots(State(state): State<AppState>) -> Result<Json<TemplateFile>, AppError> {
    let team = blocking(&state, |engine| engine.export_bots(&[])).await?;
    Ok(Json(template_file(&team)))
}

fn template_file(team: &TeamTemplate) -> TemplateFile {
    TemplateFile {
        name: sa_engine::template_file_name(team),
        text: sa_engine::template_text(team),
    }
}

/// 一个房间的团队导出成一份模板：成员、谁是入口、房间叫什么。
pub async fn export_room(
    State(state): State<AppState>,
    Path(room_id): Path<RoomId>,
) -> Result<Json<TemplateFile>, AppError> {
    let team = blocking(&state, move |engine| engine.export_room(&room_id)).await?;
    Ok(Json(template_file(&team)))
}

/// 导入一份模板。已经有同名的 Bot 跳过；模板有一处不对就什么都不建。
pub async fn import_team(
    State(state): State<AppState>,
    body: Result<Json<ImportTemplateRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ImportReport>), AppError> {
    let Json(body) = body.map_err(|e| AppError::bad_request(e.body_text()))?;
    let report = blocking(&state, move |engine| {
        let team = sa_engine::parse_template(&body.text)?;
        engine.import_team(&team, body.update_existing.unwrap_or(false))
    })
    .await?;
    Ok((StatusCode::CREATED, Json(report)))
}

/// 还没收起来的提醒，新的在前。
pub async fn list_notices(State(state): State<AppState>) -> Result<Json<Vec<Notice>>, AppError> {
    Ok(Json(blocking(&state, Engine::list_notices).await?))
}

/// 收起一条提醒。
pub async fn dismiss_notice(
    State(state): State<AppState>,
    Path(notice_id): Path<NoticeId>,
) -> Result<Json<Notice>, AppError> {
    Ok(Json(
        blocking(&state, move |engine| engine.dismiss_notice(&notice_id)).await?,
    ))
}

pub async fn list_routines(State(state): State<AppState>) -> Result<Json<Vec<Routine>>, AppError> {
    Ok(Json(
        blocking(&state, |engine| engine.list_routines()).await?,
    ))
}

pub async fn create_routine(
    State(state): State<AppState>,
    body: Result<Json<CreateRoutineRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Routine>), AppError> {
    let Json(body) = body.map_err(|e| AppError::bad_request(e.body_text()))?;
    let routine = blocking(&state, move |engine| {
        engine.create_routine(
            &body.room_id,
            body.bot_id.as_ref(),
            &body.title,
            &body.prompt,
            &body.schedule,
        )
    })
    .await?;
    Ok((StatusCode::CREATED, Json(routine)))
}

pub async fn update_routine(
    State(state): State<AppState>,
    Path(routine_id): Path<RoutineId>,
    body: Result<Json<UpdateRoutineRequest>, JsonRejection>,
) -> Result<Json<Routine>, AppError> {
    let Json(body) = body.map_err(|e| AppError::bad_request(e.body_text()))?;
    let patch = RoutinePatch {
        bot_id: body.bot_id,
        title: body.title,
        prompt: body.prompt,
        schedule: body.schedule,
        enabled: body.enabled,
    };
    Ok(Json(
        blocking(&state, move |engine| {
            engine.update_routine(&routine_id, patch)
        })
        .await?,
    ))
}

pub async fn delete_routine(
    State(state): State<AppState>,
    Path(routine_id): Path<RoutineId>,
) -> Result<StatusCode, AppError> {
    blocking(&state, move |engine| engine.delete_routine(&routine_id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// 现在就做一次。马上返回；做完之后结果通过 `routine.updated` 事件到达。
pub async fn run_routine(
    State(state): State<AppState>,
    Path(routine_id): Path<RoutineId>,
) -> Result<(StatusCode, Json<Routine>), AppError> {
    let routine = state.orchestrator.run_routine_now(&routine_id).await?;
    Ok((StatusCode::ACCEPTED, Json(routine)))
}

pub async fn list_links(State(state): State<AppState>) -> Result<Json<Vec<Link>>, AppError> {
    Ok(Json(blocking(&state, |engine| engine.list_links()).await?))
}

/// 把一个 GitHub 仓库连到房间。
pub async fn create_link(
    State(state): State<AppState>,
    body: Result<Json<CreateLinkRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Link>), AppError> {
    let Json(body) = body.map_err(|e| AppError::bad_request(e.body_text()))?;
    let link = blocking(&state, move |engine| {
        engine.link_repo(&body.room_id, &body.repo)
    })
    .await?;
    Ok((StatusCode::CREATED, Json(link)))
}

pub async fn delete_link(
    State(state): State<AppState>,
    Path(link_id): Path<LinkId>,
) -> Result<StatusCode, AppError> {
    blocking(&state, move |engine| engine.delete_link(&link_id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// 给房间生成一个 Telegram 配对码。码只在这里给一次。
pub async fn create_pairing(
    State(state): State<AppState>,
    Path(room_id): Path<RoomId>,
) -> Result<(StatusCode, Json<Pairing>), AppError> {
    let mut pairing = blocking(&state, move |engine| engine.create_pairing(&room_id)).await?;
    pairing.bot_username = state.orchestrator.telegram_username();
    Ok((StatusCode::CREATED, Json(pairing)))
}

/// 现在有哪些技能、哪些外部 MCP 服务。技能每次都从磁盘上重新读。
pub async fn extensions(State(state): State<AppState>) -> Result<Json<Extensions>, AppError> {
    let orchestrator = state.orchestrator.clone();
    let found = tokio::task::spawn_blocking(move || orchestrator.extensions())
        .await
        .map_err(AppError::internal)?;
    Ok(Json(found))
}

fn model_admin(state: &AppState) -> Result<std::sync::Arc<dyn crate::ModelAdmin>, AppError> {
    state.models.clone().ok_or_else(|| {
        AppError::new(
            StatusCode::CONFLICT,
            "conflict",
            "现在用的是不联网的假模型（--mock），没有模型设置",
        )
    })
}

/// 接了哪些模型服务，三个档位各用哪个模型。
pub async fn models(State(state): State<AppState>) -> Result<Json<ModelSettings>, AppError> {
    Ok(Json(model_admin(&state)?.settings()))
}

/// 保存模型设置，马上生效。写得不对时什么都不改。
pub async fn save_models(
    State(state): State<AppState>,
    body: Result<Json<SaveModelsRequest>, JsonRejection>,
) -> Result<Json<ModelSettings>, AppError> {
    let Json(body) = body.map_err(|e| AppError::bad_request(e.body_text()))?;
    let admin = model_admin(&state)?;
    let saved = tokio::task::spawn_blocking(move || admin.save(body))
        .await
        .map_err(AppError::internal)?
        .map_err(|message| AppError::new(StatusCode::BAD_REQUEST, "invalid", message))?;
    Ok(Json(saved))
}

/// 调一次模型看通不通。没通也是 200，原因在 `message` 里。
pub async fn test_model(
    State(state): State<AppState>,
    body: Result<Json<TestModelRequest>, JsonRejection>,
) -> Result<Json<TestModelResult>, AppError> {
    let Json(body) = body.map_err(|e| AppError::bad_request(e.body_text()))?;
    Ok(Json(model_admin(&state)?.test(body).await))
}

pub async fn list_shares(State(state): State<AppState>) -> Json<Vec<Share>> {
    Json(state.orchestrator.shares().list())
}

/// `sa share` 来取活最多等这么久，没有活就回 204，它再来。
const SHARE_POLL_WAIT: std::time::Duration = std::time::Duration::from_secs(25);

/// `sa share` 来取下一件要在共享目录里做的事。
pub async fn poll_share(
    State(state): State<AppState>,
    body: Result<Json<SharePoll>, JsonRejection>,
) -> Result<axum::response::Response, AppError> {
    use axum::response::IntoResponse;
    let Json(body) = body.map_err(|e| AppError::bad_request(e.body_text()))?;
    let shares = state.orchestrator.shares();
    let mut closing = state.closing.subscribe();
    let call: Option<ShareCall> = tokio::select! {
        polled = shares.poll(body, SHARE_POLL_WAIT) => {
            polled.map_err(|e| AppError::new(StatusCode::CONFLICT, "share_taken", e))?
        }
        _ = closing.wait_for(|closing| *closing) => None,
    };
    Ok(match call {
        Some(call) => Json(call).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    })
}

/// `sa share` 交回一件事的结果。
pub async fn reply_share(
    State(state): State<AppState>,
    body: Result<Json<ShareReply>, JsonRejection>,
) -> Result<StatusCode, AppError> {
    let Json(body) = body.map_err(|e| AppError::bad_request(e.body_text()))?;
    state
        .orchestrator
        .shares()
        .reply(body)
        .map_err(|e| AppError::new(StatusCode::CONFLICT, "share_taken", e))?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn api_not_found() -> AppError {
    AppError::new(StatusCode::NOT_FOUND, "not_found", "没有这个接口")
}
