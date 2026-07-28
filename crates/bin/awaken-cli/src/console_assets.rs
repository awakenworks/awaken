//! Embedded Management console mounted over the canonical API router.

use axum::Router;
use axum::body::Body;
use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::Response;
use axum::routing::get;

include!(concat!(env!("OUT_DIR"), "/embedded_console.rs"));

pub fn mount(app: Router) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/w/{*path}", get(index))
        .route("/assets/{*path}", get(asset))
        .fallback_service(app)
}

async fn index() -> Response {
    response("index.html", false)
}

async fn asset(Path(path): Path<String>) -> Response {
    response(&format!("assets/{path}"), true)
}

fn response(path: &str, immutable: bool) -> Response {
    let Some(bytes) = embedded_asset(path) else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .expect("valid not-found response");
    };
    let cache_control = if immutable {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type(path))
        .header(header::CACHE_CONTROL, cache_control)
        .body(Body::from(bytes))
        .expect("valid embedded asset response")
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, extension)| extension) {
        Some("css") => "text/css; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("html") => "text/html; charset=utf-8",
        Some("json") | Some("map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::put;
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    use super::*;

    /// Cause graph:
    /// embedded index/assets + canonical API fallback -> one hosted surface;
    /// asset misses remain 404 and cannot shadow API routes.
    ///
    /// Decision table:
    /// | request | embedded match | result |
    /// | `/` | index | SPA HTML |
    /// | `/assets/*` | yes | immutable asset |
    /// | `/v1/*` | no | canonical API result |
    #[tokio::test]
    async fn embedded_console_serves_the_spa_and_preserves_the_api() {
        let api = Router::new().route("/v1/probe", put(|| async { StatusCode::CREATED }));
        let app = mount(api);

        let index = app
            .clone()
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(index.status(), StatusCode::OK);
        assert_eq!(
            index.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        let body = index.into_body().collect().await.unwrap().to_bytes();
        assert!(
            body.windows(b"Awaken Console".len())
                .any(|window| window == b"Awaken Console")
        );

        let api = app
            .oneshot(Request::put("/v1/probe").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(api.status(), StatusCode::CREATED);
    }
}
