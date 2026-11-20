//! 本机 HTTP 服务。
//!
//! - `/api/*`：REST 接口与 SSE 事件流，需要访问令牌（`/api/health` 除外）。
//! - `/auth?token=…`：把令牌换成 Cookie，然后跳回首页。
//! - 其余路径：内嵌的前端页面。

mod assets;
mod auth;
mod error;
mod routes;
mod sse;

use axum::routing::{delete, get, patch, post, put};
use axum::{middleware, Router};
use sa_engine::Engine;
use sa_orchestrator::Orchestrator;
use std::future::Future;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::watch;

pub use assets::frontend_is_embedded;

#[derive(Clone)]
pub struct AppState {
    pub(crate) engine: Arc<Engine>,
    pub(crate) orchestrator: Arc<Orchestrator>,
    pub(crate) token: Arc<str>,
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
            closing: watch::Sender::new(false),
        }
    }
}

pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/rooms", get(routes::list_rooms).post(routes::create_room))
        .route(
            "/rooms/{room_id}/messages",
            get(routes::list_messages).post(routes::post_message),
        )
        .route("/rooms/{room_id}/stop", post(routes::stop))
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
