//! Registry-incarnation fixtures and terminal-cleanup admission tests.
use super::*;

#[async_trait::async_trait]
impl awaken_worker_contract::WorkerObservationSource for TestWorkerDirectory {
    async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError> {
        Ok(self.0.lock().unwrap().clone().into_iter().collect())
    }
}

#[async_trait::async_trait]
impl awaken_worker_contract::WorkerCurrentSource for TestWorkerDirectory {
    async fn current(&self, worker_id: &str) -> Result<Option<RegisteredWorker>, RegistryError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .clone()
            .filter(|record| record.snapshot.identity.worker_id == worker_id))
    }
}

#[async_trait::async_trait]
impl awaken_worker_contract::WorkerIncarnationSource for TestWorkerDirectory {
    async fn incarnation(
        &self,
        identity: &awaken_worker_contract::WorkerIdentity,
    ) -> Result<
        Option<awaken_worker_contract::RegisteredWorker>,
        awaken_worker_contract::RegistryError,
    > {
        awaken_worker_contract::read_current_incarnation(self, identity).await
    }
}

/// Cause/effect design: C1 the cleanup claim route has authenticated Worker-id
/// evidence but no Coordinator Worker Directory; C2 the request supplies a
/// syntactically complete identity, target, and cursor protocol. Effect E1: fail closed before
/// Session Control, because C1 cannot prove the current registry incarnation.
/// Decision rule D1: C1+C2 => HTTP 500 and zero claim calls. A local header is
/// deliberately insufficient incarnation authority; no compatibility bypass is
/// permitted for this recovery-only claim. Constraint/Invariant: cleanup claims
/// require the Coordinator's current registry incarnation, never header identity.
#[tokio::test]
async fn terminal_cleanup_claim_requires_current_registry_incarnation() {
    // The dispatch port reads "now" from its store; mirror the service clock.
    let dispatch_clock = Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let dispatch = Arc::new(MemoryDispatchStore::new(dispatch_clock.clone()));
    let session_control = Arc::new(RecordingSessionControl::default());
    *session_control.projection.lock().unwrap() = Some(frozen_projection());
    let service = WorkerDispatchService::local(dispatch, awaken_time::system_clock())
        .with_session_control(session_control.clone());
    let router = dispatch_transport_router_with_service(Arc::new(service));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let identity = WorkerIdentity::new("local-worker", "local-incarnation", 1);
    let response = reqwest::Client::new()
        .post(format!(
            "http://{address}/v1/worker/session/cleanup/claim-next"
        ))
        .header("x-awaken-worker-id", "local-worker")
        .json(&serde_json::json!({
            "identity": identity,
            "target": awaken_session_realization_contract::SessionRealizationTarget {
                owner: "local-worker".into(),
                runtime_incarnation: identity.lease_owner(),
                lease_expires_at_unix_ms: u64::MAX,
                reassign_existing_lease: false,
            },
            "sweep_cursor_version": awaken_session_realization_contract::TERMINAL_CLEANUP_SWEEP_CURSOR_VERSION,
            "after_session_id": null,
        }))
        .send()
        .await
        .expect("D1 request reaches the authenticated route");

    assert_eq!(
        response.status(),
        reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        "D1/E1"
    );
    assert!(
        session_control.cleanup_claims.lock().unwrap().is_empty(),
        "D1/E1"
    );
}
