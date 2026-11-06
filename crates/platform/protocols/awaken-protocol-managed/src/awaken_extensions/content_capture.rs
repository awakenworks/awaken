//! Read-only Awaken extension for a Session's content-capture decision
//! (ADR-0050 D8).
//!
//! `GET /v1/awaken/sessions/{id}/content-capture` answers
//! `{ requested, ceiling, consent, effective, reason }`: the decision a Run of
//! that Session makes now. It lives beside the Managed Session rather than as
//! a field of it, because the Managed Session object carries only the official
//! SDK fields.

use axum::Router;
use axum::handler::Handler;
use axum::routing::get;

/// Mount the read-only content-capture projection under Awaken's extension
/// namespace.
pub fn session_content_capture_router<H, T, S>(handler: H) -> Router<S>
where
    H: Handler<T, S>,
    T: 'static,
    S: Clone + Send + Sync + 'static,
{
    Router::new().route("/v1/awaken/sessions/{id}/content-capture", get(handler))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn content_capture_route_inventory_has_one_get_only_owner() {
        // Cause/effect decision table: I1 exact path+GET reaches the injected
        // reader; I2 the same path+POST is 405; I3 a lookalike path is 404.
        // Rules R1=I1=>200 with the path id, R2=I2=>405, R3=I3=>404.
        use axum::body::Body;
        use axum::extract::Path;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt as _;

        async fn read(Path(session_id): Path<String>) -> String {
            session_id
        }
        let app = session_content_capture_router(read);
        let call = |method: &str, uri: &str| {
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap()
        };
        let ok = app
            .clone()
            .oneshot(call("GET", "/v1/awaken/sessions/sesn-1/content-capture"))
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK, "R1");
        let body = axum::body::to_bytes(ok.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"sesn-1", "R1");
        let post = app
            .clone()
            .oneshot(call("POST", "/v1/awaken/sessions/sesn-1/content-capture"))
            .await
            .unwrap();
        assert_eq!(post.status(), StatusCode::METHOD_NOT_ALLOWED, "R2");
        let lookalike = app
            .oneshot(call("GET", "/v1/awaken/sessions/sesn-1/content_capture"))
            .await
            .unwrap();
        assert_eq!(lookalike.status(), StatusCode::NOT_FOUND, "R3");
    }
}
