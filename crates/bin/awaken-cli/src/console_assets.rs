//! Embedded Management console mounted over the canonical API router.

use awaken_api_contract::{SUITE_NAVIGATION_PATH, SuiteNavigation};
use axum::body::Body;
use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};

include!(concat!(env!("OUT_DIR"), "/embedded_console.rs"));

pub fn mount(app: Router) -> Router {
    mount_with_navigation(app, SuiteNavigation::default())
}

/// Mount the embedded console with its optional deployment-owned suite exit.
pub fn mount_with_navigation(app: Router, navigation: SuiteNavigation) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/w/{*path}", get(index))
        .route("/assets/{*path}", get(asset))
        .route(
            SUITE_NAVIGATION_PATH,
            get(move || {
                let navigation = navigation.clone();
                async move { Json(navigation) }
            }),
        )
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
    /// | suite navigation | JSON projection | exact inert deployment value |
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
            body.windows(b"Awaken Agents".len())
                .any(|window| window == b"Awaken Agents"),
            "the embedded index must be the canonical web/index.html product shell"
        );

        let api = app
            .oneshot(Request::put("/v1/probe").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(api.status(), StatusCode::CREATED);
    }

    /// Cause graph: configured hub -> exact projection; absent hub -> explicit
    /// standalone projection; either state -> canonical API remains unshadowed.
    ///
    /// Decision table:
    /// | rule | hub | effect |
    /// | R1 | configured | exact JSON URL |
    /// | R2 | absent | `hub_url: null` |
    #[tokio::test]
    async fn suite_navigation_projects_only_the_trusted_deployment_value() {
        for (navigation, expected, rule) in [
            (
                SuiteNavigation {
                    hub_url: Some("https://cloud.example/products".to_owned()),
                },
                serde_json::json!({"hub_url":"https://cloud.example/products"}),
                "R1",
            ),
            (
                SuiteNavigation::default(),
                serde_json::json!({"hub_url":null}),
                "R2",
            ),
        ] {
            let response = mount_with_navigation(Router::new(), navigation)
                .oneshot(
                    Request::get(SUITE_NAVIGATION_PATH)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{rule}");
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
                expected,
                "{rule}"
            );
        }
    }
}
