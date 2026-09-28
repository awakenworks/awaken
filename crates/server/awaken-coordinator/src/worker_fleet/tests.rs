use async_trait::async_trait;
use awaken_worker_registry::{
    RegisteredWorker, RegistryError, RegistryMutation, WorkerDirectory, WorkerHeartbeat,
    WorkerIdentity, WorkerObservationSource, WorkerRegistration,
};
use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

use super::*;

/// A directory that answers `list` from a fixed script and refuses every
/// mutation. Cause/effect guard: each readiness rule supplies a list result;
/// any register, heartbeat, drain, quiesce, deregister, expire, or process-exit
/// effect panics the test, while the exact read projection returns its expected
/// readiness/error response. The three route cases below cover empty, failed,
/// and capability-filtered reads through this same mutation fence.
struct ScriptedDirectory(Result<Vec<RegisteredWorker>, RegistryError>);

#[async_trait]
impl WorkerObservationSource for ScriptedDirectory {
    async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError> {
        match &self.0 {
            Ok(workers) => Ok(workers.clone()),
            Err(error) => Err(RegistryError::Persistence(error.to_string())),
        }
    }
}

#[async_trait]
impl WorkerDirectory for ScriptedDirectory {
    async fn register(
        &self,
        _registration: WorkerRegistration,
        _now_ms: u64,
        _ttl_ms: u64,
    ) -> Result<RegisteredWorker, RegistryError> {
        unreachable!("a readiness read never registers a Worker")
    }

    async fn heartbeat(
        &self,
        _identity: &WorkerIdentity,
        _heartbeat: WorkerHeartbeat,
        _now_ms: u64,
        _ttl_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        unreachable!("a readiness read never heartbeats")
    }

    async fn begin_drain(
        &self,
        _identity: &WorkerIdentity,
        _deadline_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        unreachable!("a readiness read never drains")
    }

    async fn mark_quiesced(
        &self,
        _identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        unreachable!("a readiness read never quiesces")
    }

    async fn deregister(
        &self,
        _identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        unreachable!("a readiness read never deregisters")
    }

    async fn current(&self, _worker_id: &str) -> Result<Option<RegisteredWorker>, RegistryError> {
        unreachable!("a readiness read uses the whole directory")
    }

    async fn expire_incarnation(
        &self,
        _identity: &WorkerIdentity,
        _now_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        unreachable!("a readiness read never expires an incarnation")
    }

    async fn record_process_exit(
        &self,
        _identity: &WorkerIdentity,
        _receipt: awaken_dispatch::WorkerProcessExit,
    ) -> Result<RegistryMutation, RegistryError> {
        unreachable!("a readiness read never records a process exit")
    }
}

async fn call(directory: ScriptedDirectory, uri: &str) -> (StatusCode, serde_json::Value) {
    let response = router(Arc::new(directory), awaken_lease_contract::system_clock())
        .oneshot(
            Request::builder()
                .uri(uri)
                .body(Body::empty())
                .expect("a well-formed test request"),
        )
        .await
        .expect("the router always answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("a bounded test body");
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

#[tokio::test]
async fn an_empty_fleet_answers_not_ready_rather_than_failing() {
    // The product plane must be able to distinguish "no Worker yet" from "the
    // fleet cannot be read". An empty directory is the former.
    let (status, body) = call(
        ScriptedDirectory(Ok(Vec::new())),
        "/v1/workers/readiness?capabilities=coding",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ready"], serde_json::json!(false));
    assert_eq!(body["total_workers"], serde_json::json!(0));
    assert_eq!(
        body["missing_capabilities"],
        serde_json::json!(["coding"]),
        "an empty fleet offers nothing, so every requirement is missing"
    );
}

#[tokio::test]
async fn an_unreadable_directory_is_a_retryable_dependency_failure() {
    // Answering `200 {"ready": false, "total_workers": 0}` here would let a
    // caller conclude the fleet was deleted.
    let (status, body) = call(
        ScriptedDirectory(Err(RegistryError::Persistence("offline".into()))),
        "/v1/workers/readiness",
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["status"], serde_json::json!(503));
    assert!(
        body["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("worker directory is unavailable")),
        "the operator-facing detail names the dependency, got {body}"
    );
}

#[tokio::test]
async fn an_absent_capability_list_asks_the_weaker_question() {
    let (status, body) = call(ScriptedDirectory(Ok(Vec::new())), "/v1/workers/readiness").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["missing_capabilities"],
        serde_json::json!([]),
        "no capability was required, so none can be missing"
    );
}

#[test]
fn the_wire_form_is_split_without_rejecting_a_caller_s_empty_tail() {
    assert_eq!(parse_capabilities(None), Vec::<String>::new());
    assert_eq!(parse_capabilities(Some("")), Vec::<String>::new());
    assert_eq!(
        parse_capabilities(Some("coding, gpu ,,")),
        vec!["coding".to_owned(), "gpu".to_owned()]
    );
}
