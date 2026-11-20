use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use sa_engine::EngineError;
use sa_protocol::ApiError;

#[derive(Debug)]
pub struct AppError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl AppError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "需要访问令牌：请用 `sa serve` 打印的链接打开",
        )
    }

    /// 请求本身不合格：JSON 或查询参数解析不了。
    pub fn bad_request(err: impl std::fmt::Display) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "bad_request",
            format!("请求格式不对：{err}"),
        )
    }

    pub fn internal(err: impl std::fmt::Display) -> Self {
        tracing::error!("内部错误：{err}");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "服务内部出错，详情见服务端日志",
        )
    }
}

impl From<EngineError> for AppError {
    fn from(err: EngineError) -> Self {
        match err {
            EngineError::Invalid(message) => Self::new(StatusCode::BAD_REQUEST, "invalid", message),
            EngineError::Store(e) => Self::internal(e),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = ApiError {
            code: self.code.to_string(),
            message: self.message,
        };
        (self.status, Json(body)).into_response()
    }
}
