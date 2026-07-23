//! Web console HTTP adapter.
//!
//! The outer command locates or builds the console distribution. This server
//! adapter owns HTTP static-file delivery and preserves the API router as the
//! fallback for every route outside the console surface.

use std::path::Path;

use axum::Router;
use tower_http::services::{ServeDir, ServeFile};

/// Mount a built console distribution in front of an existing API router.
pub fn mount(app: Router, dist: &Path) -> Router {
    let index = dist.join("index.html");
    Router::new()
        .route_service("/", ServeFile::new(index.clone()))
        .route_service("/w/{*path}", ServeFile::new(index))
        .nest_service("/assets", ServeDir::new(dist.join("assets")))
        .fallback_service(app)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use axum::routing::put;
    use tower::ServiceExt as _;

    use super::*;

    #[tokio::test]
    async fn mounting_the_console_preserves_api_fallback_routing() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("assets")).unwrap();
        std::fs::write(temp.path().join("index.html"), "console").unwrap();
        let api = Router::new().route("/v1/probe", put(|| async { StatusCode::CREATED }));
        let response = mount(api, temp.path())
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri("/v1/probe")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
    }
}
