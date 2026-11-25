//! 本机 HTTP 服务。
//!
//! - `/api/*`：REST 接口与 SSE 事件流，需要访问令牌（`/api/health` 除外）。
//! - `/auth?token=…`：把令牌换成 Cookie，然后跳回首页。
//! - 其余路径：内嵌的前端页面。

mod assets;
mod auth;
mod error;
mod hooks;
mod routes;
mod sse;

use axum::routing::{delete, get, patch, post, put};
use axum::{middleware, Router};
use sa_adapters::github_events;
use sa_engine::Engine;
use sa_orchestrator::Orchestrator;
use sa_protocol::{ModelSettings, SaveModelsRequest, TestModelRequest, TestModelResult};
use std::future::Future;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::watch;

pub use assets::frontend_is_embedded;

/// 界面里的「模型」页背后的东西：看、改、试模型服务和档位。失败时返回给人看的原因。
pub trait ModelAdmin: Send + Sync {
    fn settings(&self) -> ModelSettings;
    /// 保存整份设置，马上生效。
    fn save(&self, request: SaveModelsRequest) -> Result<ModelSettings, String>;
    /// 调一次模型看通不通。不保存。
    fn test(
        &self,
        request: TestModelRequest,
    ) -> futures_util::future::BoxFuture<'static, TestModelResult>;
}

#[derive(Clone)]
pub struct AppState {
    pub(crate) engine: Arc<Engine>,
    pub(crate) orchestrator: Arc<Orchestrator>,
    pub(crate) token: Arc<str>,
    /// GitHub Webhook 的密钥和筛选。没有配密钥时 `/hooks/github` 不收任何东西。
    pub(crate) github_hook: Option<Arc<GithubHook>>,
    /// 模型设置。用假模型时没有。
    pub(crate) models: Option<Arc<dyn ModelAdmin>>,
    /// 置为 `true` 表示服务正在关闭：长连接（SSE）据此主动结束，否则优雅关闭会一直等它们。
    pub(crate) closing: watch::Sender<bool>,
}

impl AppState {
    pub fn new(
        engine: Arc<Engine>,
        orchestrator: Arc<Orchestrator>,
        token: impl Into<Arc<str>>,
    ) -> Self {
        Self {
            engine,
            orchestrator,
            token: token.into(),
            github_hook: None,
            models: None,
            closing: watch::Sender::new(false),
        }
    }

    pub fn orchestrator(&self) -> Arc<Orchestrator> {
        self.orchestrator.clone()
    }

    /// 让界面能看、能改模型设置。
    pub fn with_models(mut self, models: Arc<dyn ModelAdmin>) -> Self {
        self.models = Some(models);
        self
    }

    /// 打开 `/hooks/github`：用 `secret` 验签名。
    pub fn with_github_hook(mut self, secret: &str, options: github_events::Options) -> Self {
        self.github_hook = Some(Arc::new(GithubHook {
            secret: secret.to_string(),
            options,
        }));
        self
    }
}

pub(crate) struct GithubHook {
    pub secret: String,
    pub options: github_events::Options,
}

pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/rooms", get(routes::list_rooms).post(routes::create_room))
        .route(
            "/rooms/{room_id}/messages",
            get(routes::list_messages).post(routes::post_message),
        )
        .route(
            "/rooms/{room_id}/queue",
            get(routes::list_queue).post(routes::queue_message),
        )
        .route(
            "/queue/{queued_id}",
            patch(routes::edit_queued).delete(routes::cancel_queued),
        )
        .route("/rooms/{room_id}/stop", post(routes::stop))
        .route(
            "/rooms/{room_id}/bots/{bot_id}/stop",
            post(routes::stop_bot),
        )
        .route("/rooms/{room_id}/desks", get(routes::list_desks))
        .route(
            "/rooms/{room_id}/desks/{bot_id}/accept",
            post(routes::accept_desk),
        )
        .route(
            "/rooms/{room_id}/desks/{bot_id}/discard",
            post(routes::discard_desk),
        )
        .route("/rooms/{room_id}/rules", get(routes::list_rules))
        .route("/rules/{rule_id}", delete(routes::delete_rule))
        .route(
            "/approvals/{approval_id}/decide",
            post(routes::decide_approval),
        )
        .route("/rooms/{room_id}", patch(routes::update_room))
        .route(
            "/rooms/{room_id}/members/{bot_id}",
            put(routes::add_member).delete(routes::remove_member),
        )
        .route(
            "/rooms/{room_id}/tasks",
            get(routes::list_tasks).post(routes::create_task),
        )
        .route("/tasks/{task_id}", get(routes::get_task))
        .route("/tasks/{task_id}/resume", post(routes::resume_task))
        .route("/tasks/{task_id}/accept", post(routes::accept_task))
        .route("/tasks/{task_id}/reject", post(routes::reject_task))
        .route("/tasks/{task_id}/stop", post(routes::stop_task))
        .route("/tasks/{task_id}/cancel", post(routes::cancel_task))
        .route("/bots", get(routes::list_bots).post(routes::create_bot))
        .route(
            "/bots/{bot_id}",
            patch(routes::update_bot).delete(routes::delete_bot),
        )
        .route("/usage", get(routes::usage))
        .route("/notices", get(routes::list_notices))
        .route("/notices/{notice_id}/dismiss", post(routes::dismiss_notice))
        .route(
            "/routines",
            get(routes::list_routines).post(routes::create_routine),
        )
        .route(
            "/routines/{routine_id}",
            patch(routes::update_routine).delete(routes::delete_routine),
        )
        .route("/routines/{routine_id}/run", post(routes::run_routine))
        .route("/links", get(routes::list_links).post(routes::create_link))
        .route("/links/{link_id}", delete(routes::delete_link))
        .route("/rooms/{room_id}/pairing", post(routes::create_pairing))
        .route("/extensions", get(routes::extensions))
        .route("/models", get(routes::models).put(routes::save_models))
        .route("/models/test", post(routes::test_model))
        .route("/shares", get(routes::list_shares))
        .route("/shares/poll", post(routes::poll_share))
        .route("/shares/reply", post(routes::reply_share))
        .route("/audit", get(routes::audit))
        .route("/templates/bots", get(routes::export_bots))
        .route("/templates/rooms/{room_id}", get(routes::export_room))
        .route("/templates/import", post(routes::import_team))
        .route("/events", get(sse::events))
        .fallback(routes::api_not_found)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_token,
        ));

    Router::new()
        .route("/api/health", get(routes::health))
        .nest("/api", protected)
        .route("/auth", get(auth::exchange_token))
        // 外面的服务调的地址：不用访问令牌，靠各自的签名认人。
        .route("/hooks/github", post(hooks::github))
        .fallback(assets::serve)
        .with_state(state)
}

/// 在已绑定的 `listener` 上提供服务，直到 `shutdown` 完成。
pub async fn serve(
    listener: TcpListener,
    state: AppState,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let closing = state.closing.clone();
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            shutdown.await;
            closing.send_replace(true);
        })
        .await
}
