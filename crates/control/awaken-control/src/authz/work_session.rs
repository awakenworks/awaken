//! Consumption of the complete proof stamped by the outer Work capability edge.
//!
//! This module owns no token validation or route policy. It only prevents the
//! local and Cloud management guards from reinterpreting an already-validated
//! Work bearer as a generic management credential.

use axum::extract::Request;

/// True only for the complete proof the outer WorkQueue capability edge stamps.
pub(super) fn has_work_session_access(req: &Request) -> bool {
    req.extensions()
        .get::<awaken_session_contract::work_queue::WorkSessionAccess>()
        .is_some()
        && req
            .extensions()
            .get::<awaken_tenancy::WorkspaceScope>()
            .and_then(awaken_tenancy::WorkspaceScope::non_empty)
            .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz::{management_guard, tests::fresh_iam};
    use axum::Router;
    use axum::body::Body;
    use axum::http::StatusCode;
    use tower::ServiceExt as _;

    #[tokio::test(flavor = "multi_thread")]
    async fn only_a_complete_outer_work_proof_bypasses_management_token_authentication() {
        // Cause/effect graph: C1 the outer Work guard stamped both current
        // WorkSessionAccess and non-empty WorkspaceScope; C2 only one marker is
        // present; C3 bearer is not a management token. Effects: E1 C1 proceeds
        // to the already-classified handler; E2 C2/C3 remain 401. Constraints:
        // K1 HTTP input cannot forge request extensions; K2 the outer guard is
        // the only producer and rejects routes outside the leased capability.
        // Decision rows WB1=C1+C3->E1, WB2=!C1+C3->E2.
        let (_dir, iam) = fresh_iam();
        let app = Router::new()
            .route(
                "/v1/sessions/sesn_1",
                axum::routing::get(|| async { StatusCode::OK }),
            )
            .layer(axum::middleware::from_fn_with_state(iam, management_guard));

        let make_request = |work: bool, workspace: bool| {
            let mut request = Request::builder()
                .uri("/v1/sessions/sesn_1")
                .header("authorization", "Bearer sk-ant-req-not-management")
                .body(Body::empty())
                .unwrap();
            if work {
                request.extensions_mut().insert(
                    awaken_session_contract::work_queue::WorkSessionAccess {
                        work_id: "work_1".into(),
                        environment_id: "env_1".into(),
                        session_id: "sesn_1".into(),
                        lease_owner: "owner".into(),
                        lease_epoch: 1,
                        expires_at_unix_ms: 10,
                    },
                );
            }
            if workspace {
                request
                    .extensions_mut()
                    .insert(awaken_tenancy::WorkspaceScope("ws_1".into()));
            }
            request
        };

        assert_eq!(
            app.clone()
                .oneshot(make_request(true, true))
                .await
                .unwrap()
                .status(),
            StatusCode::OK,
            "WB1"
        );
        for request in [make_request(true, false), make_request(false, true)] {
            assert_eq!(
                app.clone().oneshot(request).await.unwrap().status(),
                StatusCode::UNAUTHORIZED,
                "WB2"
            );
        }
    }
}
