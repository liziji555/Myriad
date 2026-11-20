//! 外面的服务调进来的地址。不用访问令牌：每个来源靠自己的签名认人。

use crate::error::AppError;
use crate::AppState;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use sa_adapters::github_events::{self, Parsed};
use sa_engine::Receipt;
use serde_json::{json, Value};

/// GitHub Webhook。先验签名，再看是什么事；同一次投递（`X-GitHub-Delivery`）只收一次。
///
/// 回 200 表示「送到了」，不代表这件事被收下：没连到房间的仓库、不关心的事件也回 200，
/// 这样 GitHub 不会一直重试。结果写在回应的 `outcome` 里。
pub async fn github(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    let Some(hook) = state.github_hook.clone() else {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "not_configured",
            "没有配置 Webhook 密钥，不接收 GitHub 事件",
        ));
    };
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    let signature = header("x-hub-signature-256").unwrap_or("");
    if !github_events::verify_signature(hook.secret.as_bytes(), &body, signature) {
        return Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            "bad_signature",
            "签名不对",
        ));
    }
    let (Some(event), Some(delivery)) = (header("x-github-event"), header("x-github-delivery"))
    else {
        return Err(AppError::bad_request(
            "缺少 X-GitHub-Event 或 X-GitHub-Delivery",
        ));
    };
    let payload: Value = serde_json::from_slice(&body)
        .map_err(|error| AppError::bad_request(format!("请求体不是 JSON：{error}")))?;
    let incoming = match github_events::parse(event, delivery, &payload, &hook.options) {
        Parsed::Ping => return Ok(Json(json!({ "outcome": "pong" }))),
        Parsed::Ignored(note) => return Ok(Json(json!({ "outcome": "ignored", "note": note }))),
        Parsed::Event(incoming) => incoming,
    };
    let answer = match state.orchestrator.receive(incoming).await? {
        Receipt::Stored(message) => json!({ "outcome": "stored", "message_id": message.id }),
        Receipt::Duplicate => json!({ "outcome": "duplicate" }),
        Receipt::Filtered(note) => json!({ "outcome": "filtered", "note": note }),
    };
    Ok(Json(answer))
}
