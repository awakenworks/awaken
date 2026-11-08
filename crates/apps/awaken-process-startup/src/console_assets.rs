//! Embedded Management console mounted over the canonical API router.

use awaken_api_contract::{SUITE_NAVIGATION_PATH, SuiteNavigation};
use awaken_ui_artifact::CONSOLE_ARTIFACT_MANIFEST_PATH;
use axum::body::Body;
use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
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
        .route("/w/{*path}", get(workspace_index))
        .route("/assets/{*path}", get(asset))
        .route(CONSOLE_ARTIFACT_MANIFEST_PATH, get(console_artifact))
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

async fn workspace_index(Path(path): Path<String>, headers: HeaderMap) -> Response {
    // `/w/{workspace}` is a browser location, never an SDK base URL. Returning
    // HTML for a nested API request lets SDK pagination report a false empty
    // success. Keep browser documents while refusing API-shaped or JSON calls.
    let nested_api = path
        .split_once('/')
        .is_some_and(|(_, tail)| tail == "v1" || tail.starts_with("v1/"));
    let json_request = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(',').any(|part| {
                let media = part.trim().split(';').next().unwrap_or_default();
                media == "application/json" || media.ends_with("+json")
            })
        });
    if nested_api || json_request || headers.contains_key("anthropic-version") {
        return (
            StatusCode::NOT_FOUND,
            Json(awaken_protocol_managed::types::ErrorResponse::new(
                "not_found_error",
                "SDK requests must use the service origin as base URL",
            )),
        )
            .into_response();
    }
    index().await
}

async fn asset(Path(path): Path<String>) -> Response {
    response(&format!("assets/{path}"), true)
}

async fn console_artifact() -> Response {
    response(
        CONSOLE_ARTIFACT_MANIFEST_PATH.trim_start_matches('/'),
        false,
    )
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
    if path == CONSOLE_ARTIFACT_MANIFEST_PATH.trim_start_matches('/') {
        return "application/json";
    }
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

    #[tokio::test]
    async fn workspace_browser_routes_cannot_mask_sdk_api_mistakes() {
        // Causes: C1 browser vs nested API path; C2 absent/wildcard/HTML/JSON
        // Accept; C3 SDK version Header; C4 GET/HEAD vs write. Effects: only
        // browser documents are HTML 200; nested APIs/JSON/SDK are typed 404,
        // HEAD strips the body; writes remain 405; missing assets stay 404.
        // Decision rows independently vary C1/C2/C3 and cover C4's classes.
        for (path, accept, sdk, expected) in [
            ("/w/ws/overview", None, false, StatusCode::OK),
            ("/w/ws/overview", Some("*/*"), false, StatusCode::OK),
            ("/w/ws/overview", Some("text/html"), false, StatusCode::OK),
            (
                "/w/ws/overview",
                Some("application/json"),
                false,
                StatusCode::NOT_FOUND,
            ),
            (
                "/w/ws/overview",
                Some("application/problem+json; q=1"),
                false,
                StatusCode::NOT_FOUND,
            ),
            ("/w/ws/overview", None, true, StatusCode::NOT_FOUND),
            ("/w/ws/v1/agents", None, false, StatusCode::NOT_FOUND),
            (
                "/w/ws/v1/agents",
                Some("text/html"),
                false,
                StatusCode::NOT_FOUND,
            ),
        ] {
            for method in ["GET", "HEAD"] {
                let mut request = Request::builder().method(method).uri(path);
                if let Some(accept) = accept {
                    request = request.header(header::ACCEPT, accept);
                }
                if sdk {
                    request = request.header("anthropic-version", "2023-06-01");
                }
                let response = mount(Router::new())
                    .oneshot(request.body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    expected,
                    "{method}/{path}/{accept:?}/{sdk}"
                );
                let content_type = response.headers()[header::CONTENT_TYPE]
                    .to_str()
                    .unwrap()
                    .to_owned();
                let body = response.into_body().collect().await.unwrap().to_bytes();
                if expected == StatusCode::NOT_FOUND {
                    assert_eq!(content_type, "application/json");
                    if method == "GET" {
                        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
                        assert_eq!(value["type"], "error");
                        assert_eq!(value["error"]["type"], "not_found_error");
                    }
                } else {
                    assert_eq!(content_type, "text/html; charset=utf-8");
                }
                if method == "HEAD" {
                    assert!(body.is_empty());
                }
            }
        }
        for (method, path, expected) in [
            ("POST", "/w/ws/v1/agents", StatusCode::METHOD_NOT_ALLOWED),
            ("GET", "/assets/nonexistent.js", StatusCode::NOT_FOUND),
        ] {
            assert_eq!(
                mount(Router::new())
                    .oneshot(
                        Request::builder()
                            .method(method)
                            .uri(path)
                            .body(Body::empty())
                            .unwrap()
                    )
                    .await
                    .unwrap()
                    .status(),
                expected
            );
        }
    }

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
        let manifest = mount(Router::new())
            .oneshot(
                Request::get(CONSOLE_ARTIFACT_MANIFEST_PATH)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(manifest.status(), StatusCode::OK);
        assert_eq!(
            manifest.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
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
