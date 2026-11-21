use crate::error::AppError;
use crate::AppState;
use axum::extract::{Query, Request, State};
use axum::http::header::{AUTHORIZATION, COOKIE, LOCATION, SET_COOKIE};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

const COOKIE_NAME: &str = "sa_token";

pub async fn require_token(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    match presented_token(request.headers()) {
        Some(token) if tokens_match(token, &state.token) => Ok(next.run(request).await),
        _ => Err(AppError::unauthorized()),
    }
}

#[derive(Deserialize)]
pub struct ExchangeQuery {
    token: Option<String>,
}

/// `GET /auth?token=…`：令牌正确就写入 Cookie 并跳回首页，这样令牌不会留在地址栏里。
pub async fn exchange_token(
    State(state): State<AppState>,
    Query(query): Query<ExchangeQuery>,
) -> Response {
    match query.token {
        Some(token) if tokens_match(&token, &state.token) => {
            let cookie = format!("{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Strict");
            (
                StatusCode::SEE_OTHER,
                [(SET_COOKIE, cookie), (LOCATION, "/".to_string())],
            )
                .into_response()
        }
        _ => (
            StatusCode::UNAUTHORIZED,
            Html(
                "<!doctype html><meta charset=\"utf-8\"><title>苦乐</title>\
                 <p>访问令牌不正确。请用 <code>sa serve</code> 打印的链接重新打开。</p>",
            ),
        )
            .into_response(),
    }
}

fn presented_token(headers: &HeaderMap) -> Option<&str> {
    if let Some(value) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if let Some(token) = value.strip_prefix("Bearer ") {
            return Some(token.trim());
        }
    }
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE_NAME)
        .map(|(_, value)| value)
}

/// 不随内容提前返回的比较，避免通过响应时间猜出令牌。
fn tokens_match(presented: &str, expected: &str) -> bool {
    let (a, b) = (presented.as_bytes(), expected.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn reads_bearer_then_cookie() {
        let mut headers = HeaderMap::new();
        headers.insert(COOKIE, HeaderValue::from_static("a=1; sa_token=abc; b=2"));
        assert_eq!(presented_token(&headers), Some("abc"));
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer xyz"));
        assert_eq!(presented_token(&headers), Some("xyz"));
        assert_eq!(presented_token(&HeaderMap::new()), None);
    }

    #[test]
    fn compares_whole_token() {
        assert!(tokens_match("abc", "abc"));
        assert!(!tokens_match("abd", "abc"));
        assert!(!tokens_match("ab", "abc"));
        assert!(!tokens_match("", "abc"));
    }
}
