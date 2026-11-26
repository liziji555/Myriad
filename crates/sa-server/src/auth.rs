use crate::error::AppError;
use crate::AppState;
use axum::extract::{Query, Request, State};
use axum::http::header::{AUTHORIZATION, COOKIE, HOST, LOCATION, ORIGIN, SET_COOKIE};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

const COOKIE_NAME: &str = "sa_token";

/// Cookie 不分端口：同一台电脑上 `127.0.0.1:4517` 的本机服务和 `127.0.0.1:随便` 的隧道
/// 会互相覆盖同名 Cookie。所以名字里带上浏览器看到的端口。
fn cookie_name(headers: &HeaderMap) -> String {
    let port = headers
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .and_then(|host| host.rsplit_once(':'))
        .map(|(_, port)| port)
        .filter(|port| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()));
    match port {
        Some(port) => format!("{COOKIE_NAME}_{port}"),
        None => COOKIE_NAME.to_string(),
    }
}

pub async fn require_token(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    match presented_token(request.headers()) {
        Some(token) if tokens_match(token, &state.token) => {
            if !same_origin_mutation(&request) {
                return Err(AppError::new(
                    StatusCode::FORBIDDEN,
                    "forbidden",
                    "拒绝跨源请求",
                ));
            }
            Ok(next.run(request).await)
        }
        _ => Err(AppError::unauthorized()),
    }
}

/// 浏览器发来的会改状态的请求，只认同一个源（协议 + 主机 + 端口都对得上）。
/// 本机别的端口上的网页和我们同站但不同源，不能借着 Cookie 替人操作。
/// 非浏览器客户端不带 `Origin` 和 `Sec-Fetch-Site`，不受影响。
fn same_origin_mutation(request: &Request) -> bool {
    if matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) {
        return true;
    }
    let headers = request.headers();
    if let Some(site) = headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
    {
        if !matches!(site, "same-origin" | "none") {
            return false;
        }
    }
    if let Some(origin) = headers.get(ORIGIN).and_then(|value| value.to_str().ok()) {
        let Some(host) = headers.get(HOST).and_then(|value| value.to_str().ok()) else {
            return false;
        };
        let Some(origin_authority) = origin
            .strip_prefix("http://")
            .or_else(|| origin.strip_prefix("https://"))
        else {
            return false;
        };
        if !origin_authority.eq_ignore_ascii_case(host) {
            return false;
        }
    }
    true
}

#[derive(Deserialize)]
pub struct ExchangeQuery {
    token: Option<String>,
}

/// `GET /auth?token=…`：令牌正确就写入 Cookie 并跳回首页，这样令牌不会留在地址栏里。
pub async fn exchange_token(
    State(state): State<AppState>,
    Query(query): Query<ExchangeQuery>,
    headers: HeaderMap,
) -> Response {
    match query.token {
        Some(token) if tokens_match(&token, &state.token) => {
            let name = cookie_name(&headers);
            let cookie = format!("{name}={token}; Path=/; HttpOnly; SameSite=Strict");
            (
                StatusCode::SEE_OTHER,
                [(SET_COOKIE, cookie), (LOCATION, "/".to_string())],
            )
                .into_response()
        }
        _ => (
            StatusCode::UNAUTHORIZED,
            Html(
                "<!doctype html><meta charset=\"utf-8\"><title>Myriad</title>\
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
    let name = cookie_name(headers);
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(cookie, _)| *cookie == name)
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
    fn the_cookie_is_named_after_the_port_the_browser_sees() {
        let mut headers = HeaderMap::new();
        headers.insert(HOST, HeaderValue::from_static("127.0.0.1:50123"));
        headers.insert(
            COOKIE,
            HeaderValue::from_static("sa_token_4517=local; sa_token_50123=remote"),
        );
        assert_eq!(cookie_name(&headers), "sa_token_50123");
        assert_eq!(presented_token(&headers), Some("remote"));
        headers.insert(HOST, HeaderValue::from_static("[::1]:4517"));
        assert_eq!(presented_token(&headers), Some("local"));
        headers.insert(HOST, HeaderValue::from_static("example.com"));
        assert_eq!(cookie_name(&headers), "sa_token");
    }

    #[test]
    fn compares_whole_token() {
        assert!(tokens_match("abc", "abc"));
        assert!(!tokens_match("abd", "abc"));
        assert!(!tokens_match("ab", "abc"));
        assert!(!tokens_match("", "abc"));
    }
}
