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
) -> Router {
    flat = flat.merge(mcp_export);
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
                        // Heartbeat is Worker authority, while publication refresh is
                        // an after-commit projection. Never hold the heartbeat response
                        // open on that projection: a slow model/catalog read would let
                        // the just-committed Worker lease expire before the Worker can
                        // observe its receipt, fencing every subsequent dispatch.
                        //
                        // The shared gate still coalesces concurrent heartbeats and
                        // advances its fingerprint only after a successful refresh, so
                        // a transient failure is retried by a later heartbeat.
                        tokio::spawn(async move {
                            let _ = worker_observation_gate.reconcile(reconciler.as_ref()).await;
                        });
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
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::post;
    use tower::ServiceExt;

    #[derive(Default)]
    struct RecordingReconciler {
        fixed: AtomicUsize,
        all: AtomicUsize,
    }

    struct BlockingReconciler {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl awaken_config_service::PublicationBindingReconciler for BlockingReconciler {
        async fn reconcile(&self) -> Result<usize, String> {
            unreachable!("the Worker event uses the all-policy operation")
        }

        async fn reconcile_all(&self) -> Result<usize, String> {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(1)
        }
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
        );
        for (rule, path) in [("H1", "/v1/worker/heartbeat"), ("H2", "/unrelated")] {
            let response = app
                .clone()
                .oneshot(Request::post(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{rule}");
        }
        tokio::time::timeout(Duration::from_millis(250), async {
            while reconciler.all.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("H1 schedules the after-commit reconciliation");
        assert_eq!(reconciler.all.load(Ordering::SeqCst), 1, "H1+H2");
        assert_eq!(reconciler.fixed.load(Ordering::SeqCst), 0, "H2");
    }
    #[tokio::test]
    async fn blocked_projection_never_blocks_the_committed_worker_heartbeat_receipt() {
        // Cause/effect graph:
        // C1 the registry accepts the heartbeat; C2 the derived publication
        // refresh blocks. E1 the HTTP heartbeat receipt still returns promptly;
        // E2 refresh remains in flight and can complete later. Coupling C1 to C2
        // makes the Worker's lease expire even though authority was committed.
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let app = finish(
            Router::new().route("/v1/worker/heartbeat", post(|| async { StatusCode::OK })),
            Router::new(),
            Some(Arc::new(BlockingReconciler {
                entered: entered.clone(),
                release: release.clone(),
            })),
            worker_observations(),
            "platform".into(),
        );

        let response = tokio::time::timeout(
            Duration::from_millis(250),
            app.oneshot(
                Request::post("/v1/worker/heartbeat")
                    .body(Body::empty())
                    .unwrap(),
            ),
        )
        .await
        .expect("E1 heartbeat response is independent of projection")
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "E1");
        tokio::time::timeout(Duration::from_millis(250), entered.notified())
            .await
            .expect("E2 projection continues asynchronously");
        release.notify_waiters();
    }
}
