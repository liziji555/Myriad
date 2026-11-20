use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../web/dist"]
struct Frontend;

const NOT_BUILT: &str = "<!doctype html><meta charset=\"utf-8\"><title>SuperAgent</title>\
<p>前端还没有构建。请在仓库根目录运行 <code>pnpm --dir web install</code> 和 \
<code>pnpm --dir web build</code>，再重新编译。</p>";

/// 这个可执行文件里是否带有前端页面。
pub fn frontend_is_embedded() -> bool {
    Frontend::get("index.html").is_some()
}

pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if let Some(file) = Frontend::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        // 带哈希的构建产物可以永久缓存；其余每次都向服务器确认。
        let cache = if path.starts_with("assets/") {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };
        return (
            [
                (CONTENT_TYPE, mime.as_ref().to_string()),
                (CACHE_CONTROL, cache.to_string()),
            ],
            file.data,
        )
            .into_response();
    }
    // 缺失的构建产物必须是 404；否则浏览器会把首页当成脚本来解析。
    if path.starts_with("assets/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    // 单页应用：其余未知路径交给前端路由。
    match Frontend::get("index.html") {
        Some(index) => ([(CACHE_CONTROL, "no-cache")], Html(index.data)).into_response(),
        None => (StatusCode::SERVICE_UNAVAILABLE, Html(NOT_BUILT)).into_response(),
    }
}
