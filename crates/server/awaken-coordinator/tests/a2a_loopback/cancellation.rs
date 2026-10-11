//! Production credential admission and A2A transport composed with durable control.
use super::authenticated::AuthenticatedPeer;
use super::*;
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_lease_testkit::ManualClock;

use awaken_dispatch::{DEFAULT_LEASE_MS, DispatchWorker, MemoryDispatchStore, RunDispatch};
use awaken_runtime::Runtime;
use awaken_runtime_contract::activation::RunActivation;
use awaken_store_inmem::MemoryCommitCoordinator;
use std::sync::atomic::{AtomicBool, Ordering};

const TASK: &str = "cancel-task";

#[derive(Clone, Copy, Debug)]
enum Case {
    Terminal,
    Revoked,
    StillWaiting,
    Expired,
}

fn task(terminal: bool) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "kind": "task", "id": TASK, "contextId": "cancel-thread",
        "status": {"state": if terminal { "canceled" } else { "input-required" }},
    }))
}

fn peer_router(case: Case, clock: Arc<ManualClock>) -> Router {
    use awaken_protocol_a2a::client::{MESSAGE_SEND_PATH, task_cancel_path, task_path};
    let stopped = Arc::new(AtomicBool::new(false));
    let observed = stopped.clone();
    Router::new()
        .route(
            MESSAGE_SEND_PATH,
            axum::routing::post(|| async { task(false) }),
        )
        .route(
            &task_path(TASK),
            get(move || {
                let stopped = observed.clone();
                let clock = clock.clone();
                async move {
                    // R4 expires the real dispatch claim at the HTTP observation
                    // boundary, before the production driver's next authority check.
                    if matches!(case, Case::Expired) {
                        clock.advance(DEFAULT_LEASE_MS + 1);
                    }
                    task(stopped.load(Ordering::SeqCst))
                }
            }),
        )
        .route(
            &task_cancel_path(TASK),
            axum::routing::post(move || {
                let stopped = stopped.clone();
                async move {
                    if matches!(case, Case::Terminal) {
                        stopped.store(true, Ordering::SeqCst);
                    }
                    StatusCode::ACCEPTED
                }
            }),
        )
}

fn worker(
    peer: &AuthenticatedPeer,
    store: Arc<MemoryDispatchStore>,
    commit: Arc<MemoryCommitCoordinator>,
    owner: &str,
) -> DispatchWorker<MemoryDispatchStore> {
    let installation = awaken_coordinator::a2a_attempt_executor(Some(peer.materializer()));
    let worker = DispatchWorker::new(Arc::new(Runtime::new()), store, commit, owner)
        .with_local_credential_capabilities(installation.credential_realization);
    worker.install_attempt_executor(installation.executor);
    worker
}

async fn cancellation_case(case: Case) {
    let clock = Arc::new(ManualClock::new(0));
    let peer = AuthenticatedPeer::start(peer_router(case, clock.clone())).await;
    let activation = RunActivation::new(
        RunId("cancel-run".into()),
        ThreadId("cancel-thread".into()),
        ExecutableAgentSnapshot::builder("remote")
            .resolved_model(
                ResolvedModelCandidate::try_remote(
                    ModelBinding::new("remote", "", format!("a2a:{}", peer.endpoint)),
                    awaken_tenancy::ScopeId::from("default"),
                    Some(peer.access.clone()),
                    peer.security_fingerprint.clone(),
                )
                .unwrap(),
            )
            .build(),
        vec![user("wait for input")],
    );
    let run = activation.run_id.clone();
    let mut dispatch = RunDispatch::new(activation);
    dispatch.inference_plaintext_holder = Some(awaken_runtime_contract::PlaintextHolder::new(
        awaken_runtime_contract::PlaintextBoundary::Worker,
        awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
    ));
    // R4 expires the claim by advancing this clock at the HTTP boundary, and the
    // dispatch port now reads "now" from the store it was built with.
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    store.enqueue(dispatch).await.unwrap();
    let first = worker(&peer, store.clone(), commit.clone(), "first-worker");
    assert_eq!(
        first.tick(clock.clone()).await.unwrap(),
        Some((run.clone(), RunState::Awaiting))
    );
    assert!(store.load_physical_attempt(&run).await.unwrap().is_none());
    assert_eq!(
        peer.requests.lock().unwrap().as_slice(),
        &[(
            "POST".into(),
            awaken_protocol_a2a::client::MESSAGE_SEND_PATH.into(),
            true,
        )]
    );
    drop(first);
    peer.requests.lock().unwrap().clear();
    if matches!(case, Case::Revoked) {
        peer.revoke().await;
    }
    store
        .cancel(&run)
        .await
        .unwrap()
        .expect("durable cancel intent");
    let restarted = worker(&peer, store.clone(), commit.clone(), "replacement-worker");
    let result = restarted.tick(clock).await;
    let requests = peer.requests.lock().unwrap().clone();
    let expected = match case {
        Case::Revoked => vec![],
        Case::Expired => vec![("GET", awaken_protocol_a2a::client::task_path(TASK))],
        Case::Terminal | Case::StillWaiting => vec![
            ("GET", awaken_protocol_a2a::client::task_path(TASK)),
            ("POST", awaken_protocol_a2a::client::task_cancel_path(TASK)),
            ("GET", awaken_protocol_a2a::client::task_path(TASK)),
        ],
    }
    .into_iter()
    .map(|(method, path)| (method.to_string(), path, true))
    .collect::<Vec<_>>();
    assert_eq!(
        requests, expected,
        "{case:?}: exact authenticated I/O, no resend or anonymous retry"
    );
    if matches!(case, Case::Terminal) {
        assert_eq!(
            result.unwrap(),
            Some((run.clone(), RunState::Ended(EndCause::Cancelled)))
        );
        assert!(store.list_dispatches().await.unwrap().is_empty());
        assert!(store.load_physical_attempt(&run).await.unwrap().is_none());
        assert_eq!(
            commit.run_state(&run),
            Some(RunState::Ended(EndCause::Cancelled))
        );
    } else {
        assert!(
            result.is_err(),
            "{case:?}: insufficient evidence must surface failure: {result:?}"
        );
        let rows = store.list_dispatches().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].run_id, run);
        assert!(
            rows[0].cancellation_requested && rows[0].physical_attempt_active,
            "{case:?}"
        );
        assert_eq!(commit.run_state(&run), Some(RunState::Awaiting));
    }
}

#[test]
fn authenticated_cancellation_recovery_obeys_claim_and_terminal_evidence() {
    // Causes: C1 exact origin credential remains valid/revoked after Awaiting;
    // C2 independent GET observes terminal/still-waiting after accepted cancel;
    // C3 current claim remains live/expires during first GET; C4 Worker is rebuilt.
    // Effects: E1 exact authenticated GET/POST/GET, Cancelled commit and slot release;
    // E2 no task I/O; E3 one GET but no POST after expiry; E4 error, retained intent
    // and physical slot, unchanged Awaiting truth. Graph: C1 && C3 && C2 => E1;
    // !C1 => E2+E4; C1 && !C3 => E3+E4; C1 && C3 && !C2 => E4.
    // | Rule | credential | claim at GET | observed after cancel | effects |
    // | R1 | valid   | live    | terminal | E1 |
    // | R2 | revoked | live    | unreached | E2+E4 |
    // | R3 | valid   | live    | waiting | authenticated GET/POST/GET + E4 |
    // | R4 | valid   | expired | unreached | E3+E4 |
    // Constraint: every row has C4 and uses production claim admission, pinned
    // materialization, HTTP transport, A2A adapter, terminal writer and ACK owner.
    // The first Worker commits the real Accepted task; no test fabricates its
    // reference or cancellation binding. Queue/commit stores are the in-memory
    // conformance adapters: rebuilding proves composition, not a process crash
    // or PostgreSQL durability. The peer's truthful terminal response is the TCB.
    run_composed_async_test(|| async {
        for case in [
            Case::Terminal,
            Case::Revoked,
            Case::StillWaiting,
            Case::Expired,
        ] {
            cancellation_case(case).await;
        }
    });
}

use awaken_dispatch::{
    DispatchAdministration as _, DispatchControlReads as _, DispatchEnqueue as _,
    DispatchReads as _,
};
