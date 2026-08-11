//! Last-mile process startup for authority-change reconciliation and
//! workspace-addressed routing.

use std::sync::Arc;

use axum::Router;

pub(crate) fn finish(
    mut flat: Router,
    mcp_export: Router,
    reconciler: Option<Arc<dyn awaken_config_service::PublicationBindingReconciler>>,
    worker_observations: Arc<dyn awaken_coordinator::WorkerObservationSource>,
    platform_workspace: String,
    managed_rate_limiter: Arc<awaken_protocol_managed::ManagedRateLimiter>,
) -> Router {
    flat = flat.merge(mcp_export);
    // One startup serves one resolved Organization. Install one shared
    // limiter before workspace-path dispatch so flat and rewritten Workspace
    // routes draw from the same organization buckets.
    flat = flat.layer(axum::middleware::from_fn_with_state(
        managed_rate_limiter,
        awaken_protocol_managed::enforce_managed_rate_limit,
    ));
    if let Some(reconciler) = reconciler {
        let worker_observation_gate = Arc::new(
            crate::observation_reconcile::WorkerObservationReconcileGate::new(worker_observations),
        );
        let reconcile_on_authority_change = axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let reconciler = reconciler.clone();
                let worker_observation_gate = worker_observation_gate.clone();
                async move {
                    let method = req.method().clone();
                    let path = req.uri().path().to_string();
                    let is_write =
                        method == axum::http::Method::POST || method == axum::http::Method::PUT;
                    let model_authority_changed = is_write
                        && (path.contains("/config/provider-connections")
                            || path.contains("/config/model-attributes")
                            || path.contains("/config/inference-profiles/")
                            || path.contains("/config/brokered-models/refresh"));
                    let worker_observations_may_have_changed =
                        method == axum::http::Method::POST && path == "/v1/worker/heartbeat";
                    let response = next.run(req).await;
                    if response.status().is_success() && model_authority_changed {
                        let _ = reconciler.reconcile().await;
                    }
                    if response.status().is_success() && worker_observations_may_have_changed {
                        // The gate advances only after success; heartbeat is the
                        // bounded retry clock for a transient reconciliation failure.
                        let _ = worker_observation_gate.reconcile(reconciler.as_ref()).await;
                    }
                    response
                }
            },
        );
        flat = flat.layer(reconcile_on_authority_change);
    }
    let flat =
        awaken_coordinator::workspace_path::with_platform_workspace(flat, platform_workspace);
    awaken_coordinator::workspace_path::with_workspace_path_addressing(flat)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::post;
    use tower::ServiceExt;

    #[derive(Default)]
    struct RecordingReconciler {
        fixed: AtomicUsize,
        all: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl awaken_config_service::PublicationBindingReconciler for RecordingReconciler {
        async fn reconcile(&self) -> Result<usize, String> {
            self.fixed.fetch_add(1, Ordering::SeqCst);
            Ok(1)
        }

        async fn reconcile_all(&self) -> Result<usize, String> {
            self.all.fetch_add(1, Ordering::SeqCst);
            Ok(1)
        }
    }

    fn worker_observations() -> Arc<dyn awaken_coordinator::WorkerObservationSource> {
        awaken_coordinator::test_worker_directory()
    }

    #[tokio::test]
    async fn successful_worker_heartbeat_enters_the_all_scope_reconcile_gate() {
        // Cause/effect decision table:
        // H1 successful POST heartbeat -> all-scope reconcile;
        // H2 non-heartbeat write -> no Worker reconcile.
        // Coalescing, changed fingerprints and failure retry are owned by the
        // gate test; model-authority writes use the fixed-set branch.
        let reconciler = Arc::new(RecordingReconciler::default());
        let app = finish(
            Router::new()
                .route("/v1/worker/heartbeat", post(|| async { StatusCode::OK }))
                .route("/unrelated", post(|| async { StatusCode::OK })),
            Router::new(),
            Some(reconciler.clone()),
            worker_observations(),
            "platform".into(),
            Arc::new(awaken_protocol_managed::ManagedRateLimiter::for_organization("org_test")),
        );
        for (rule, path) in [("H1", "/v1/worker/heartbeat"), ("H2", "/unrelated")] {
            let response = app
                .clone()
                .oneshot(Request::post(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{rule}");
        }
        assert_eq!(reconciler.all.load(Ordering::SeqCst), 1, "H1+H2");
        assert_eq!(reconciler.fixed.load(Ordering::SeqCst), 0, "H2");
    }

    #[tokio::test]
    async fn flat_and_workspace_paths_share_one_organization_create_bucket() {
        // Organization/Workspace cause graph:
        // O1 flat Managed create and O2 workspace-addressed Managed create both
        // enter the same post-rewrite flat router; Organization is fixed by the
        // startup, while Workspace is only a resource scope. Therefore the
        // first two mixed creates consume an explicit two-token test bucket and
        // O3 create three is one 429 — no per-Workspace bucket and no double
        // charge during the rewrite. The deliberately slow refill also makes
        // this an execution-speed-independent regression test.
        let app = finish(
            Router::new().route("/v1/sessions", post(|| async { StatusCode::OK })),
            Router::new(),
            Some(Arc::new(RecordingReconciler::default())),
            worker_observations(),
            "platform".into(),
            Arc::new(awaken_protocol_managed::ManagedRateLimiter::with_limits(
                "org_shared",
                awaken_protocol_managed::ManagedRateLimits {
                    create_per_minute: 2,
                    read_per_minute: 2,
                },
            )),
        );
        for ordinal in 0..2 {
            let path = if ordinal % 2 == 0 {
                "/v1/sessions"
            } else {
                "/v1/workspaces/workspace_b/sessions"
            };
            let response = app
                .clone()
                .oneshot(Request::post(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "O1/O2 #{ordinal}");
        }
        let rejected = app
            .oneshot(
                Request::post("/v1/workspaces/workspace_c/sessions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS, "O3");
        assert!(rejected.headers().contains_key("retry-after"), "O3");
    }
}
