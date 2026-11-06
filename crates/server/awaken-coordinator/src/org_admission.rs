// @formal surface obligation=managed_rate_limit.organization_buckets_never_over_admit_under_monotonic_time
// @formal reason: Mounts the access edge's Org bucket decision on the AI SDK, AG-UI and A2A entries with
// @formal reason: each protocol's own encoding; it owns no bucket state, clock or admission decision.
//! Org admission on the runtime protocol entries this composition root mounts:
//! the access edge's bucket decision with each protocol's own classification
//! and encoding. The Managed entries are admitted where Control's management
//! edge mounts them (`awaken_control::protect_management_router`).

use std::sync::Arc;

use awaken_authz_enforce::{OrgAdmissionEdge, OrgAdmissionEncoding, admit_by_org_bucket};
use awaken_tenancy::org_admission::OrgRequestAdmission;
use axum::Router;

pub(crate) const AI_SDK: OrgAdmissionEncoding =
    OrgAdmissionEncoding::plain_text(awaken_protocol_ai_sdk::org_admission::org_admission_class);

pub(crate) const AG_UI: OrgAdmissionEncoding =
    OrgAdmissionEncoding::plain_text(awaken_protocol_ag_ui::org_admission::org_admission_class);

pub(crate) const A2A: OrgAdmissionEncoding = OrgAdmissionEncoding {
    classify: awaken_protocol_a2a::org_admission::org_admission_class,
    scope_missing: awaken_protocol_a2a::org_admission::org_admission_scope_missing,
    unavailable: awaken_protocol_a2a::org_admission::org_admission_unavailable,
    rejected: awaken_protocol_a2a::org_admission::org_admission_rejected,
    annotate: None,
};

/// Admit `router`'s metered requests by Org bucket. Mount it inside the edge
/// that stamps the trusted Workspace scope. `None` is a deterministic scenario
/// host that mounts no limiter.
pub(crate) fn admitted(
    router: Router,
    admission: Option<&Arc<dyn OrgRequestAdmission>>,
    encoding: OrgAdmissionEncoding,
) -> Router {
    match admission {
        Some(admission) => router.layer(axum::middleware::from_fn_with_state(
            OrgAdmissionEdge::new(admission.clone(), encoding),
            admit_by_org_bucket,
        )),
        None => router,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use awaken_tenancy::org_admission::{
        OrgAdmissionDecision, OrgAdmissionRequest, OrgAdmissionUnavailable, OrgOperation,
    };
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request, StatusCode, header};
    use axum::routing::{get, post};
    use tower::ServiceExt;

    use super::*;

    struct FixedAdmission {
        calls: Arc<AtomicUsize>,
        operation: OrgOperation,
        allowed: Option<bool>,
        retry_after: Option<u64>,
    }

    #[async_trait::async_trait]
    impl OrgRequestAdmission for FixedAdmission {
        async fn admit(
            &self,
            request: OrgAdmissionRequest,
        ) -> Result<OrgAdmissionDecision, OrgAdmissionUnavailable> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(request.workspace_id, "ws");
            assert_eq!(request.operation, self.operation);
            let Some(allowed) = self.allowed else {
                return Err(OrgAdmissionUnavailable {
                    message: "down".into(),
                });
            };
            Ok(OrgAdmissionDecision {
                allowed,
                limit: 3,
                remaining: 0,
                retry_after: self.retry_after,
                reset_after: 60,
                reset_at_unix_ms: 0,
            })
        }
    }

    async fn assert_plain_text_protocol(
        encoding: OrgAdmissionEncoding,
        create_path: &'static str,
        read_path: &'static str,
    ) {
        // Causes: C1 the ORIGINAL protocol classifier meters Create/Read;
        // C2 trusted Workspace scope; C3 limiter answers; C4 allowed decision;
        // C5 rejected decision has Retry-After. Effects include exact wire bytes
        // and whether either the limiter or the real mounted handler runs.
        // | Rule | C1        | C2 | C3 | C4 | C5       | Effect |
        // | W0   | unmetered | -  | -  | -  | -        | handler only, unchanged response |
        // | W1   | Create    | no | -  | -  | -        | 503 missing-scope text, neither call |
        // | W2   | Read      | yes| no | -  | -        | 503 exact unavailable text, no handler |
        // | W3   | Create/Read|yes| yes| no| some/none | representative rejection of each route |
        // | W4   | Read      | yes| yes| yes| -        | limiter + handler, unchanged response |
        // The two retained named regressions exercise DIFFERENT real classifiers
        // through Coordinator's actual middleware assembly, not copied encoders.
        // The codec owner's E3 table owns the complete operation/header product;
        // this integration only crosses route/wiring and handler-dispatch axes.
        for (rule, operation, scoped, allowed, retry_after) in [
            ("W0", None, false, None, None),
            ("W1", Some(OrgOperation::Create), false, None, None),
            ("W2", Some(OrgOperation::Read), true, None, None),
            (
                "W3 create retry",
                Some(OrgOperation::Create),
                true,
                Some(false),
                Some(20),
            ),
            (
                "W3 read no retry",
                Some(OrgOperation::Read),
                true,
                Some(false),
                None,
            ),
            ("W4 read", Some(OrgOperation::Read), true, Some(true), None),
        ] {
            let admission_calls = Arc::new(AtomicUsize::new(0));
            let handler_calls = Arc::new(AtomicUsize::new(0));
            let handler = {
                let calls = handler_calls.clone();
                move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async { (StatusCode::ACCEPTED, "handler") }
                }
            };
            let router = Router::new()
                .route(create_path, post(handler.clone()))
                .route(read_path, get(handler.clone()))
                .route("/free", post(handler));
            let limiter: Arc<dyn OrgRequestAdmission> = Arc::new(FixedAdmission {
                calls: admission_calls.clone(),
                operation: operation.unwrap_or(OrgOperation::Create),
                allowed,
                retry_after,
            });
            let mut app = admitted(router, Some(&limiter), encoding);
            if scoped {
                app = app.layer(axum::Extension(awaken_tenancy::WorkspaceScope("ws".into())));
            }
            let (method, path) = match operation {
                None => (Method::POST, "/free"),
                Some(OrgOperation::Create) => (Method::POST, create_path),
                Some(OrgOperation::Read) => (Method::GET, read_path),
            };
            let response = app
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let runs_handler = operation.is_none() || allowed == Some(true);
            let (status, body) = if runs_handler {
                (StatusCode::ACCEPTED, "handler")
            } else if !scoped {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "request has no trusted Workspace scope",
                )
            } else if allowed.is_none() {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Org request limiter is unavailable: down",
                )
            } else if operation == Some(OrgOperation::Create) {
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    "Org Create request limit exceeded",
                )
            } else {
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    "Org Read request limit exceeded",
                )
            };
            assert_eq!(response.status(), status, "{rule}");
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "text/plain; charset=utf-8",
                "{rule}"
            );
            assert_eq!(
                response
                    .headers()
                    .get(header::RETRY_AFTER)
                    .map(|v| v.to_str().unwrap()),
                retry_after.map(|_| "20"),
                "{rule}"
            );
            assert_eq!(
                to_bytes(response.into_body(), 1024).await.unwrap().as_ref(),
                body.as_bytes(),
                "{rule}"
            );
            assert_eq!(
                admission_calls.load(Ordering::SeqCst),
                usize::from(operation.is_some() && scoped),
                "{rule}"
            );
            assert_eq!(
                handler_calls.load(Ordering::SeqCst),
                usize::from(runs_handler),
                "{rule}"
            );
        }
    }

    mod ai_sdk {
        #[tokio::test]
        async fn a_refused_request_keeps_the_adapter_plain_text_error_shape() {
            super::assert_plain_text_protocol(
                super::AI_SDK,
                "/v1/ai-sdk/chat",
                "/v1/ai-sdk/threads/t/messages",
            )
            .await;
        }
    }

    mod ag_ui {
        #[tokio::test]
        async fn a_refused_request_keeps_the_adapter_plain_text_error_shape() {
            super::assert_plain_text_protocol(
                super::AG_UI,
                "/v1/ag-ui",
                "/v1/ag-ui/threads/t/messages",
            )
            .await;
        }
    }
}
