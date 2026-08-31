//! Route-scoped browser CORS policy for the AI SDK application ingress.
//!
//! CORS is a deployment admission boundary, not an authentication mechanism.
//! The application-token guard remains authoritative for every actual request;
//! this policy only permits a browser at an explicitly configured origin to
//! present that credential to the AI SDK routes.

use std::collections::BTreeSet;
use std::time::Duration;

use axum::Router;
use axum::http::{HeaderName, HeaderValue, Method, header};
use tower_http::cors::{AllowOrigin, CorsLayer};

const MAX_BROWSER_ORIGINS: usize = 64;
const AI_SDK_STREAM_HEADER: HeaderName = HeaderName::from_static("x-vercel-ai-ui-message-stream");

/// Closed deployment policy for cross-origin AI SDK browser access.
///
/// An empty policy installs no CORS middleware, so browser preflight remains
/// rejected by ordinary routing. Values are canonical browser `Origin` header
/// values: no wildcard, path, credentials, query, or fragment is accepted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AiSdkBrowserCors {
    origins: Vec<String>,
}

impl AiSdkBrowserCors {
    pub fn try_from_origins(origins: Vec<String>) -> Result<Self, String> {
        if origins.len() > MAX_BROWSER_ORIGINS {
            return Err(format!(
                "ai_sdk_browser_origins accepts at most {MAX_BROWSER_ORIGINS} origins"
            ));
        }
        let mut canonical = Vec::with_capacity(origins.len());
        let mut unique = BTreeSet::new();
        for origin in origins {
            if origin.trim() != origin || origin.is_empty() {
                return Err(
                    "ai_sdk_browser_origins entries must be non-empty exact origins".to_owned(),
                );
            }
            let parsed = url::Url::parse(&origin)
                .map_err(|_| "ai_sdk_browser_origins entries must be absolute origins")?;
            let host = parsed
                .host_str()
                .ok_or("ai_sdk_browser_origins entries must contain a host")?;
            let loopback_http = parsed.scheme() == "http"
                && (host == "localhost"
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|address| address.is_loopback()));
            if parsed.scheme() != "https" && !loopback_http {
                return Err(
                    "ai_sdk_browser_origins entries must use HTTPS or loopback HTTP".to_owned(),
                );
            }
            if !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.query().is_some()
                || parsed.fragment().is_some()
            {
                return Err(
                    "ai_sdk_browser_origins entries must not contain credentials, query, or fragment"
                        .to_owned(),
                );
            }
            let exact = parsed.origin().ascii_serialization();
            if origin != exact {
                return Err(format!(
                    "ai_sdk_browser_origins entry {origin:?} must be the canonical origin {exact:?}"
                ));
            }
            if !unique.insert(exact.clone()) {
                return Err(format!(
                    "ai_sdk_browser_origins contains duplicate origin {exact:?}"
                ));
            }
            canonical.push(exact);
        }
        Ok(Self { origins: canonical })
    }

    pub fn origins(&self) -> &[String] {
        &self.origins
    }

    pub(crate) fn apply(&self, router: Router) -> Router {
        if self.origins.is_empty() {
            return router;
        }
        let origins = self
            .origins
            .iter()
            .map(|origin| HeaderValue::from_str(origin).expect("validated browser origin"))
            .collect::<Vec<_>>();
        router.layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::list(origins))
                .allow_methods([Method::GET, Method::HEAD, Method::POST])
                .allow_headers([header::ACCEPT, header::AUTHORIZATION, header::CONTENT_TYPE])
                .expose_headers([
                    header::CACHE_CONTROL,
                    header::CONTENT_TYPE,
                    AI_SDK_STREAM_HEADER,
                ])
                .max_age(Duration::from_secs(600)),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::get;
    use tower::ServiceExt as _;

    use super::*;

    async fn counted(calls: Arc<AtomicUsize>) -> impl IntoResponse {
        calls.fetch_add(1, Ordering::SeqCst);
        "ok"
    }

    fn request(method: Method, path: &str, origin: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .header(header::ORIGIN, origin)
            .body(Body::empty())
            .unwrap()
    }

    /// Test design — browser admission is the product of three independent facts:
    /// C1 an exact origin is configured, C2 the request is a preflight/actual AI SDK request,
    /// and C3 the sibling route is outside the AI SDK router. Effects: E1 a matching preflight
    /// is answered without entering the guarded handler; E2 actual responses expose only the
    /// protocol/cache headers; E3 empty/unlisted origins receive no authorization header; E4 a
    /// sibling protocol never inherits CORS. This proves default-deny, exact allowlisting,
    /// middleware ordering, and route isolation without weakening bearer authentication.
    #[tokio::test]
    async fn exact_origin_preflight_is_short_circuited_and_siblings_stay_closed() {
        let calls = Arc::new(AtomicUsize::new(0));
        let ai_calls = calls.clone();
        let ai = Router::new().route(
            "/v1/ai-sdk/threads/{thread}/messages",
            get(move || counted(ai_calls.clone())),
        );
        let policy =
            AiSdkBrowserCors::try_from_origins(vec!["https://workspace.example".to_owned()])
                .unwrap();
        let app = policy
            .apply(ai)
            .merge(Router::new().route("/v1/ag-ui", get(|| async { "ag" })));

        let preflight = Request::builder()
            .method(Method::OPTIONS)
            .uri("/v1/ai-sdk/threads/t/messages")
            .header(header::ORIGIN, "https://workspace.example")
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
            .header(
                header::ACCESS_CONTROL_REQUEST_HEADERS,
                "authorization,content-type",
            )
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(preflight).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "C1+C2=>E1");
        assert_eq!(
            response.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            Some(&HeaderValue::from_static("https://workspace.example")),
            "C1+C2=>E1"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "C1+C2=>E1");

        let actual = app
            .clone()
            .oneshot(request(
                Method::GET,
                "/v1/ai-sdk/threads/t/messages",
                "https://workspace.example",
            ))
            .await
            .unwrap();
        assert_eq!(actual.status(), StatusCode::OK, "C1+C2=>E2");
        assert_eq!(
            actual.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            Some(&HeaderValue::from_static("https://workspace.example")),
            "C1+C2=>E2"
        );
        let exposed = actual
            .headers()
            .get(header::ACCESS_CONTROL_EXPOSE_HEADERS)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(exposed.contains("cache-control"), "C1+C2=>E2");
        assert!(
            exposed.contains("x-vercel-ai-ui-message-stream"),
            "C1+C2=>E2"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "C1+C2=>E2");

        let unlisted = app
            .clone()
            .oneshot(request(
                Method::GET,
                "/v1/ai-sdk/threads/t/messages",
                "https://unlisted.example",
            ))
            .await
            .unwrap();
        assert!(
            unlisted
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none(),
            "!C1+C2=>E3"
        );
        let sibling = app
            .oneshot(request(
                Method::GET,
                "/v1/ag-ui",
                "https://workspace.example",
            ))
            .await
            .unwrap();
        assert!(
            sibling
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none(),
            "C1+C3=>E4"
        );
    }

    #[test]
    fn policy_rejects_ambient_or_non_origin_configuration() {
        // Test design: every value that broadens authority (wildcard/non-TLS host/path/credentials),
        // creates ambiguous equality (non-canonical/duplicate), or exceeds the fixed resource bound
        // is rejected at configuration time; exact HTTPS and loopback development origins survive.
        for invalid in [
            vec!["*".to_owned()],
            vec!["http://workspace.example".to_owned()],
            vec!["https://workspace.example/path".to_owned()],
            vec!["https://user:secret@workspace.example".to_owned()],
            vec!["https://workspace.example/".to_owned()],
            vec![
                "https://workspace.example".to_owned(),
                "https://workspace.example".to_owned(),
            ],
        ] {
            assert!(AiSdkBrowserCors::try_from_origins(invalid).is_err());
        }
        assert!(
            AiSdkBrowserCors::try_from_origins(vec!["https://workspace.example".to_owned()])
                .is_ok()
        );
        assert!(
            AiSdkBrowserCors::try_from_origins(vec!["http://127.0.0.1:4173".to_owned()]).is_ok()
        );
        assert!(
            AiSdkBrowserCors::try_from_origins(
                (0..=MAX_BROWSER_ORIGINS)
                    .map(|index| format!("https://{index}.workspace.example"))
                    .collect()
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn empty_policy_leaves_preflight_closed() {
        // Test design: omitted deployment configuration is not an implicit wildcard. The ordinary
        // method router returns 405, emits no CORS authority, and never invokes the actual handler.
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = calls.clone();
        let app = AiSdkBrowserCors::default().apply(Router::new().route(
            "/v1/ai-sdk/threads/{thread}/messages",
            get(move || counted(handler_calls.clone())),
        ));
        let response = app
            .oneshot(request(
                Method::OPTIONS,
                "/v1/ai-sdk/threads/t/messages",
                "https://workspace.example",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
