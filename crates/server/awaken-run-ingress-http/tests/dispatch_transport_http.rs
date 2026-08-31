//! The worker-facing dispatch transport over its real HTTP surface: a database-less
//! worker's `enqueue` → `claim` → `renew` → `settle` round-trip drives the same
//! process-shared dispatch store the co-located pool drains. Its own test binary: it
//! installs a one-shot injected dispatch store, so it must not share a process with
//! other dispatch tests.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Record as RunRecord, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::event::{AgentEvent, Delta, Fact};
use awaken_agent_contract::stream::event::{
    Event as StreamEvent, Observation as StreamObservation,
};
use awaken_agent_contract::thread::read::recovery::{
    RecoveryError, RunRecoverySnapshot, RunRecoverySource,
};
use awaken_run_ingress::{
    DispatchQueue, DispatchSettlementError, DispatchSettlementObserver, MemoryDispatchStore,
    RunClaim, RunDispatch, StreamEventRequest, StreamObservationRequest, WorkerIdentity,
};
use awaken_run_ingress_http::{WorkerDispatchService, dispatch_transport_router_with_service};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_store_inmem::MemoryStreamSink;
use awaken_worker_transport_security::{
    FixedWorkerLeasePolicy, HeaderWorkerAuthenticator, ManualWorkerClock,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tracing::Instrument;

const TERMINAL_TRACEPARENT: &str = "00-22222222222222222222222222222222-2222222222222222-01";

fn activation(run: &str, thread: &str) -> RunActivation {
    RunActivation::new(
        RunId(run.into()),
        ThreadId(thread.into()),
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snap".into()),
            metadata: Default::default(),
            root_agent_id: AgentId("agent".into()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: CatalogFingerprint("fp".into()),
                instructions: "be helpful".into(),
                max_steps: 8,
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding::new("prov", "model", "acp:test"),
                ),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint: CatalogFingerprint("fp".into()),
        },
        vec![Message::text(MessageId("u1".into()), Role::User, "go")],
    )
}

async fn post(router: &Router, worker: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-awaken-worker-id", worker)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = router.clone().oneshot(request).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

struct StaticRecovery {
    expected_session_thread_id: ThreadId,
    expected_thread_id: ThreadId,
    expected_run_id: RunId,
    result: Result<RunRecoverySnapshot, String>,
    observed: Mutex<Vec<(ThreadId, ThreadId, RunId)>>,
}

#[async_trait::async_trait]
impl RunRecoverySource for StaticRecovery {
    async fn recovery_snapshot(
        &self,
        _thread_id: &ThreadId,
        _claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        Err(RecoveryError::Rejected(
            "settlement bypassed physical Session recovery".into(),
        ))
    }

    async fn recovery_snapshot_in_session(
        &self,
        session_thread_id: &ThreadId,
        thread_id: &ThreadId,
        claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        self.observed.lock().unwrap().push((
            session_thread_id.clone(),
            thread_id.clone(),
            claimed_run_id.clone(),
        ));
        if session_thread_id != &self.expected_session_thread_id
            || thread_id != &self.expected_thread_id
            || claimed_run_id != &self.expected_run_id
        {
            return Err(RecoveryError::Rejected(
                "settlement recovery coordinates do not match the guarded dispatch".into(),
            ));
        }
        self.result.clone().map_err(RecoveryError::Rejected)
    }
}

#[derive(Default)]
struct FailFirstTerminalSettlement {
    attempts: AtomicUsize,
    successes: AtomicUsize,
    observed: Mutex<Vec<(ThreadId, ThreadId, RunId)>>,
    span_names: Mutex<Vec<Option<&'static str>>>,
}

#[async_trait::async_trait]
impl DispatchSettlementObserver for FailFirstTerminalSettlement {
    async fn before_settle(
        &self,
        dispatch: &RunDispatch,
        claim: &RunClaim,
        committed_state: &RunState,
        _cancellation_requested: bool,
    ) -> Result<(), DispatchSettlementError> {
        assert_eq!(dispatch.run_id(), &claim.run_id);
        assert!(matches!(committed_state, RunState::Ended(_)));
        self.observed.lock().unwrap().push((
            dispatch.session_thread_id().clone(),
            dispatch.thread_id().clone(),
            dispatch.run_id().clone(),
        ));
        self.span_names
            .lock()
            .unwrap()
            .push(tracing::Span::current().metadata().map(|meta| meta.name()));
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(DispatchSettlementError(
                "injected terminal publication failure".into(),
            ));
        }
        self.successes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Default)]
struct RecordingSessionWork {
    releases: AtomicUsize,
}

#[async_trait::async_trait]
impl awaken_session_contract::work_queue::SessionWorkLeaseAuthority for RecordingSessionWork {
    async fn acquire_session_work(
        &self,
        session_id: &str,
        worker_owner: &str,
        now_ms: u64,
        _acquisition: awaken_session_contract::work_queue::SessionWorkAcquisition,
    ) -> Result<
        awaken_session_contract::work_queue::SessionWorkOwnership,
        awaken_session_contract::work_queue::WorkQueueError,
    > {
        Ok(
            awaken_session_contract::work_queue::SessionWorkOwnership::Leased(
                awaken_session_contract::work_queue::SessionWorkLease {
                    work_id: format!("work-{session_id}"),
                    environment_id: "env".into(),
                    session_id: session_id.into(),
                    owner: worker_owner.into(),
                    epoch: 1,
                    expires_at_unix_ms: now_ms + 60_000,
                },
            ),
        )
    }

    async fn release_session_work(
        &self,
        _lease: &awaken_session_contract::work_queue::SessionWorkLease,
    ) -> Result<bool, awaken_session_contract::work_queue::WorkQueueError> {
        self.releases.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    }

    async fn release_worker_session_work(
        &self,
        _worker_owner: &str,
    ) -> Result<usize, awaken_session_contract::work_queue::WorkQueueError> {
        Ok(0)
    }
}

fn terminal_snapshot(thread_id: &ThreadId, run_id: &RunId, state: RunState) -> RunRecoverySnapshot {
    RunRecoverySnapshot {
        thread_id: thread_id.clone(),
        claimed_run_id: run_id.clone(),
        runs: vec![RunRecord {
            id: run_id.clone(),
            thread_id: thread_id.clone(),
            state,
        }],
        latest_run_id: Some(run_id.clone()),
        messages: Vec::new(),
        message_commit_cursors: Vec::new(),
        state: Vec::new(),
        state_commit_cursors: Vec::new(),
        events: Vec::new(),
        resume_tickets: Vec::new(),
        thread_version: 1,
        store_cursor: 1,
        next_commit_ordinal: 1,
    }
}

async fn claimed_terminal_dispatch(
    tag: &str,
) -> (Arc<MemoryDispatchStore>, RunClaim, ThreadId, ThreadId) {
    let run_id = RunId(format!("terminal-run-{tag}"));
    let thread_id = ThreadId(format!("terminal-child-{tag}"));
    let session_thread_id = ThreadId(format!("terminal-parent-{tag}"));
    let store = Arc::new(MemoryDispatchStore::new());
    store
        .enqueue(
            RunDispatch::new(activation(&run_id.0, &thread_id.0))
                .for_session(session_thread_id.clone())
                .with_traceparent(Some(TERMINAL_TRACEPARENT.to_string())),
        )
        .await
        .unwrap();
    let claimed = store
        .claim("terminal-worker", 30_000, 0, &Default::default())
        .await
        .unwrap()
        .expect("terminal fixture claim");
    (
        store,
        RunClaim::from(&claimed.lease),
        session_thread_id,
        thread_id,
    )
}

fn terminal_recovery(
    session_thread_id: &ThreadId,
    thread_id: &ThreadId,
    run_id: &RunId,
    result: Result<RunRecoverySnapshot, String>,
) -> Arc<StaticRecovery> {
    Arc::new(StaticRecovery {
        expected_session_thread_id: session_thread_id.clone(),
        expected_thread_id: thread_id.clone(),
        expected_run_id: run_id.clone(),
        result,
        observed: Mutex::new(Vec::new()),
    })
}

async fn settle_terminal(router: &Router, claim: &RunClaim) -> (StatusCode, Value) {
    post(
        router,
        "terminal-worker",
        "/v1/worker/dispatch/settle",
        json!({
            "run_id": claim.run_id.0,
            "epoch": claim.epoch,
            "outcome": "Done",
            "consumed": []
        }),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_db_less_worker_claims_renews_and_settles_over_http() {
    let mem = Arc::new(MemoryDispatchStore::new());
    let clock = Arc::new(ManualWorkerClock::new(0));
    let router = dispatch_transport_router_with_service(Arc::new(WorkerDispatchService::new(
        mem as Arc<dyn DispatchQueue>,
        Arc::new(HeaderWorkerAuthenticator),
        clock.clone(),
        Arc::new(FixedWorkerLeasePolicy::new(30_000)),
    )));

    let unauthorized = Request::builder()
        .method("POST")
        .uri("/v1/worker/dispatch/claim")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let unauthorized = router.clone().oneshot(unauthorized).await.unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    // enqueue a run over the transport.
    let request = serde_json::to_value(RunDispatch::new(activation("run-A", "t1"))).unwrap();
    let (s, _) = post(
        &router,
        "worker-1",
        "/v1/worker/dispatch/enqueue",
        json!({ "request": request }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // claim it: the self-contained request comes back with a lease.
    let (s, v) = post(&router, "worker-1", "/v1/worker/dispatch/claim", json!({})).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        v["claimed"]["request"]["activation"]["run_id"], "run-A",
        "claim returns the enqueued run over the wire: {v}"
    );
    assert_eq!(
        v["claimed"]["lease"]["owner"], "worker-1",
        "the lease is owned: {v}"
    );
    // Capture the fence epoch the claim assigned — the settle must carry it.
    let epoch = v["claimed"]["lease"]["epoch"]
        .as_u64()
        .expect("claim returns a lease epoch");
    assert_eq!(epoch, 1, "the first claim assigns fence epoch 1: {v}");

    // Renewal transport rule H1. Causes: the authenticated Worker still owns the
    // exact run+epoch returned by claim. Effect: Control derives owner/time/lease
    // policy, renews that exact claim, and returns true. The complementary missing-
    // epoch and wrong-epoch rules live in the contract and transport-client tests.
    clock.set(1_000);
    let (s, v) = post(
        &router,
        "worker-1",
        "/v1/worker/dispatch/renew",
        json!({ "run_id": "run-A", "lease_epoch": epoch }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["renewed"], true, "the owner renews its live lease: {v}");

    // a claim by a second worker finds nothing runnable (single owner per run).
    let (_, v) = post(&router, "worker-2", "/v1/worker/dispatch/claim", json!({})).await;
    assert!(
        v["claimed"].is_null(),
        "a leased run is not double-claimed: {v}"
    );

    // a settle carrying a non-current epoch is fenced over the wire — nothing
    // changes (a stale owner past its lease cannot settle behind a reclaimer).
    let (s, v) = post(
        &router,
        "worker-1",
        "/v1/worker/dispatch/settle",
        json!({ "run_id": "run-A", "epoch": 99, "outcome": "Done", "consumed": [] }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        v["settled"], false,
        "a non-current-epoch settle is fenced, not applied: {v}"
    );

    let (s, v) = post(
        &router,
        "worker-2",
        "/v1/worker/dispatch/settle",
        json!({ "run_id": "run-A", "epoch": epoch, "outcome": "Done", "consumed": [] }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        v["settled"], false,
        "an authenticated non-owner cannot settle a current epoch: {v}"
    );

    // settle Done under the current epoch: the dispatch is finished and removed.
    let (s, v) = post(
        &router,
        "worker-1",
        "/v1/worker/dispatch/settle",
        json!({ "run_id": "run-A", "epoch": epoch, "outcome": "Done", "consumed": [] }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["settled"], true, "the run settles Done: {v}");

    // nothing left to claim.
    let (_, v) = post(&router, "worker-1", "/v1/worker/dispatch/claim", json!({})).await;
    assert!(v["claimed"].is_null(), "a settled run is gone: {v}");
}

#[tokio::test(flavor = "multi_thread")]
async fn terminal_observer_failure_withholds_remote_child_done_and_parent_work_release() {
    // Test design.
    // C: C1 one exact child claim has distinct parent physical Session and child
    // logical Thread coordinates plus matching committed Ended truth; C2 its
    // first terminal publication fails; C3 the same live epoch retries and the
    // publication succeeds; C4 the settle HTTP request has unrelated ambient
    // context while the guarded RunDispatch retains its admitted traceparent.
    // E: E1 C2 returns 503 while the row stays Leased, publishes nothing, and
    // does not release parent Session Work; E2 C3 publishes once and alone
    // settles Done, while the child continues to borrow rather than release the
    // parent's Work lease; E3 every recovery and observer call preserves the
    // parent/logical/Run tuple; E4 both attempts run under the Coordinator's
    // explicit settlement continuation, not the ambient transport span.
    // K: physical/logical recovery, state validation, and observer delivery all
    // occur under the exact guard; its persisted traceparent is the sole
    // retry-stable causal carrier, while trace metadata never authorizes state;
    // only observer success permits guard drop and queue settlement, and only a
    // root Run may release Session Work.
    // D: R1=(C1,C2)=>503+Leased+0 releases+0 successes+E3;
    // R2=(C1,C3,C4)=>200+Done+0 releases+1 success+E3+E4.
    awaken_observability::init(&awaken_observability::ObservabilityConfig::default());
    let (store, claim, session_thread_id, thread_id) =
        claimed_terminal_dispatch("observer-retry").await;
    let run_id = claim.run_id.clone();
    let observer = Arc::new(FailFirstTerminalSettlement::default());
    let work = Arc::new(RecordingSessionWork::default());
    let recovery = terminal_recovery(
        &session_thread_id,
        &thread_id,
        &run_id,
        Ok(terminal_snapshot(
            &thread_id,
            &run_id,
            RunState::Ended(EndCause::NaturalEnd),
        )),
    );
    let router = dispatch_transport_router_with_service(Arc::new(
        WorkerDispatchService::new(
            store.clone(),
            Arc::new(HeaderWorkerAuthenticator),
            Arc::new(ManualWorkerClock::new(0)),
            Arc::new(FixedWorkerLeasePolicy::new(30_000)),
        )
        .with_recovery_source(recovery.clone())
        .with_terminal_observer(observer.clone())
        .with_session_work_authority(work.clone()),
    ));
    let settle = || {
        settle_terminal(&router, &claim).instrument(tracing::info_span!("settle.request.ambient"))
    };

    let (status, _) = settle().await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "R1/E1");
    let rows = store.list_dispatches().await.unwrap();
    assert_eq!(rows.len(), 1, "R1/E1 retains replay evidence");
    assert_eq!(
        rows[0].state,
        awaken_run_ingress::DispatchState::Leased,
        "R1/E1"
    );
    assert_eq!(work.releases.load(Ordering::SeqCst), 0, "R1/E1");
    assert_eq!(observer.attempts.load(Ordering::SeqCst), 1, "R1/E1");
    assert_eq!(observer.successes.load(Ordering::SeqCst), 0, "R1/E1");

    let (status, value) = settle().await;
    assert_eq!(status, StatusCode::OK, "R2/E2: {value}");
    assert_eq!(value["settled"], true, "R2/E2: {value}");
    assert!(store.list_dispatches().await.unwrap().is_empty(), "R2/E2");
    assert_eq!(work.releases.load(Ordering::SeqCst), 0, "R2/E2");
    assert_eq!(observer.attempts.load(Ordering::SeqCst), 2, "R2/E2");
    assert_eq!(observer.successes.load(Ordering::SeqCst), 1, "R2/E2");
    let expected_coordinates = (session_thread_id.clone(), thread_id.clone(), run_id.clone());
    assert_eq!(
        recovery.observed.lock().unwrap().as_slice(),
        [expected_coordinates.clone(), expected_coordinates.clone()],
        "R1-R2/E3 recovery preserves parent physical and child logical coordinates"
    );
    assert_eq!(
        observer.observed.lock().unwrap().as_slice(),
        [expected_coordinates.clone(), expected_coordinates],
        "R1-R2/E3 observer receives the same guarded coordinates"
    );
    assert_eq!(
        observer.span_names.lock().unwrap().as_slice(),
        [
            Some("dispatch.settlement.observe"),
            Some("dispatch.settlement.observe")
        ],
        "R1-R2/E4 guarded dispatch provenance overrides ambient settle transport"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_done_requires_one_exact_committed_terminal_recovery_prefix() {
    // Test design.
    // C: C1 the exact guarded claim has no recovery source; C2 recovery returns
    // an outage; C3 recovery returns a snapshot whose logical coordinate differs
    // from the guarded child; C4 recovery returns the exact coordinates but no
    // matching committed Run; C5 the exact Run is committed Awaiting; C6 the
    // exact Run is committed Running.
    // E: E1 C1 fails 500; E2 C2 fails 503; E3 C3-C6 fail 400; E4 every rule
    // retains the Leased dispatch and invokes neither the terminal observer nor
    // parent Session Work release; E5 configured sources receive the exact
    // parent-physical/child-logical/Run tuple once.
    // K: Done is derived only from one claim-guarded recovery prefix. Missing,
    // unavailable, mismatched, absent, Awaiting, or Running evidence cannot
    // degrade to a best-effort Done settlement or a Work side effect.
    // D:
    // | Rule | Recovery source/result | Committed state | Status | Effects |
    // | R1 | missing | - | 500 | E1,E4 |
    // | R2 | unavailable | - | 503 | E2,E4,E5 |
    // | R3 | mismatched logical Thread | Ended | 400 | E3-E5 |
    // | R4 | exact coordinates, no matching Run | - | 400 | E3-E5 |
    // | R5 | exact | Awaiting | 400 | E3-E5 |
    // | R6 | exact | Running | 400 | E3-E5 |
    enum RecoveryRule {
        Missing,
        Unavailable,
        Mismatched,
        NoMatchingRun,
        Nonterminal(RunState),
    }

    let rules = [
        (
            "missing",
            RecoveryRule::Missing,
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (
            "unavailable",
            RecoveryRule::Unavailable,
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (
            "mismatched",
            RecoveryRule::Mismatched,
            StatusCode::BAD_REQUEST,
        ),
        (
            "no-matching-run",
            RecoveryRule::NoMatchingRun,
            StatusCode::BAD_REQUEST,
        ),
        (
            "awaiting",
            RecoveryRule::Nonterminal(RunState::Awaiting),
            StatusCode::BAD_REQUEST,
        ),
        (
            "running",
            RecoveryRule::Nonterminal(RunState::Running),
            StatusCode::BAD_REQUEST,
        ),
    ];

    for (tag, rule, expected_status) in rules {
        let (store, claim, session_thread_id, thread_id) = claimed_terminal_dispatch(tag).await;
        let run_id = claim.run_id.clone();
        let observer = Arc::new(FailFirstTerminalSettlement::default());
        let work = Arc::new(RecordingSessionWork::default());
        let recovery = match rule {
            RecoveryRule::Missing => None,
            RecoveryRule::Unavailable => Some(terminal_recovery(
                &session_thread_id,
                &thread_id,
                &run_id,
                Err("injected recovery outage".into()),
            )),
            RecoveryRule::Mismatched => Some(terminal_recovery(
                &session_thread_id,
                &thread_id,
                &run_id,
                Ok(terminal_snapshot(
                    &ThreadId(format!("foreign-{tag}")),
                    &run_id,
                    RunState::Ended(EndCause::NaturalEnd),
                )),
            )),
            RecoveryRule::NoMatchingRun => {
                let mut snapshot =
                    terminal_snapshot(&thread_id, &run_id, RunState::Ended(EndCause::NaturalEnd));
                snapshot.runs.clear();
                Some(terminal_recovery(
                    &session_thread_id,
                    &thread_id,
                    &run_id,
                    Ok(snapshot),
                ))
            }
            RecoveryRule::Nonterminal(state) => Some(terminal_recovery(
                &session_thread_id,
                &thread_id,
                &run_id,
                Ok(terminal_snapshot(&thread_id, &run_id, state)),
            )),
        };
        let mut service = WorkerDispatchService::new(
            store.clone(),
            Arc::new(HeaderWorkerAuthenticator),
            Arc::new(ManualWorkerClock::new(0)),
            Arc::new(FixedWorkerLeasePolicy::new(30_000)),
        )
        .with_terminal_observer(observer.clone())
        .with_session_work_authority(work.clone());
        if let Some(recovery) = recovery.as_ref() {
            service = service.with_recovery_source(recovery.clone());
        }
        let router = dispatch_transport_router_with_service(Arc::new(service));

        let (status, value) = settle_terminal(&router, &claim).await;
        assert_eq!(status, expected_status, "{tag}: {value}");
        let rows = store.list_dispatches().await.unwrap();
        assert_eq!(rows.len(), 1, "{tag}/E4 retains retry evidence");
        assert_eq!(
            rows[0].state,
            awaken_run_ingress::DispatchState::Leased,
            "{tag}/E4"
        );
        assert_eq!(observer.attempts.load(Ordering::SeqCst), 0, "{tag}/E4");
        assert_eq!(work.releases.load(Ordering::SeqCst), 0, "{tag}/E4");
        if let Some(recovery) = recovery {
            assert_eq!(
                recovery.observed.lock().unwrap().as_slice(),
                [(session_thread_id, thread_id, run_id)],
                "{tag}/E5"
            );
        }
    }
}

/// Cause/effect graph and decision table for the cross-process live relay.
/// Causes: C1 authenticated Worker owns the claim; C2 epoch is current; C3 event
/// Run equals claim Run; C4 event is classified live; C5 an optional coordinate
/// names the committed Thread, a foreign Thread, or is absent for a legacy
/// adapter. Effects: E1 forward once to the existing Coordinator StreamSink; E2
/// return accepted=false without a forward for a stale epoch; E3 reject
/// malformed/unauthorized observations; E4 retain an exact coordinate, while an
/// absent coordinate remains foreground-only and is never guessed. Constraint:
/// no row mutates messages, Run state, or settlement.
/// Decision rule: R1-R6 cover the valid relay, stale claim, mismatched Run,
/// non-live Fact, foreign coordinate, and legacy absent-coordinate partitions.
///
/// | Rule | C1 | C2 | C3 | C4 | C5 | Effect |
/// |---|---|---|---|---|---|---|
/// | R1 | T | T | T | T | exact | E1+E4 |
/// | R2 | T | F | T | T | exact | E2 |
/// | R3 | T | T | F | T | exact | E3 |
/// | R4 | T | T | T | F | absent | E3 |
/// | R5 | T | T | T | T | foreign | E3 |
/// | R6 | T | T | T | T | absent | E1+E4 |
#[tokio::test(flavor = "multi_thread")]
async fn worker_stream_transport_forwards_only_live_events_from_the_current_claim() {
    let mem = Arc::new(MemoryDispatchStore::new());
    let stream = Arc::new(MemoryStreamSink::new());
    let router = dispatch_transport_router_with_service(Arc::new(
        WorkerDispatchService::new(
            mem.clone() as Arc<dyn DispatchQueue>,
            Arc::new(HeaderWorkerAuthenticator),
            Arc::new(ManualWorkerClock::new(0)),
            Arc::new(FixedWorkerLeasePolicy::new(30_000)),
        )
        .with_stream_sink(stream.clone()),
    ));
    mem.enqueue(RunDispatch::new(activation("run-live", "thread-live")))
        .await
        .unwrap();
    let claimed = mem
        .claim("worker-live", 30_000, 0, &Default::default())
        .await
        .unwrap()
        .expect("claim");
    let claim = RunClaim::from(&claimed.lease);
    let identity = WorkerIdentity::new("worker-live", "boot-live", 1);
    let event = StreamObservation::assistant_delta(
        claim.run_id.clone(),
        awaken_agent_contract::agent::thread::Id("thread-live".into()),
        0,
        0,
        AgentEvent::Delta(Delta::TextDelta { delta: "x".into() }),
    );
    let request = |claim: RunClaim, observation: StreamObservation| {
        serde_json::to_value(StreamObservationRequest {
            request: StreamEventRequest {
                claim,
                identity: identity.clone(),
                event: observation.event,
            },
            assistant_response: observation.assistant_response,
        })
        .unwrap()
    };

    let (status, value) = post(
        &router,
        "worker-live",
        "/v1/worker/dispatch/stream",
        request(claim.clone(), event.clone()),
    )
    .await;
    assert_eq!(
        (status, value["accepted"].as_bool()),
        (StatusCode::OK, Some(true)),
        "R1"
    );
    assert_eq!(stream.events().len(), 1, "R1 forwards once");
    assert_eq!(
        stream.observations()[0]
            .assistant_response
            .as_ref()
            .map(|coordinate| coordinate.thread_id.0.as_str()),
        Some("thread-live"),
        "R1/E4"
    );

    let (status, value) = post(
        &router,
        "worker-live",
        "/v1/worker/dispatch/stream",
        request(
            RunClaim {
                epoch: claim.epoch + 1,
                ..claim.clone()
            },
            event.clone(),
        ),
    )
    .await;
    assert_eq!(
        (status, value["accepted"].as_bool()),
        (StatusCode::OK, Some(false)),
        "R2"
    );

    let (status, _) = post(
        &router,
        "worker-live",
        "/v1/worker/dispatch/stream",
        request(
            claim.clone(),
            StreamObservation {
                event: StreamEvent {
                    run_id: RunId("other".into()),
                    ..event.event.clone()
                },
                ..event.clone()
            },
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "R3");

    let (status, _) = post(
        &router,
        "worker-live",
        "/v1/worker/dispatch/stream",
        request(
            claim.clone(),
            StreamObservation::assistant_delta(
                claim.run_id.clone(),
                awaken_agent_contract::agent::thread::Id("foreign-thread".into()),
                0,
                0,
                AgentEvent::Delta(Delta::TextDelta { delta: "x".into() }),
            ),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "R5");

    let (status, _) = post(
        &router,
        "worker-live",
        "/v1/worker/dispatch/stream",
        request(
            claim.clone(),
            StreamObservation::from(StreamEvent {
                run_id: RunId("run-live".into()),
                kind: AgentEvent::Fact(Fact::RunFinished { exhausted: false }),
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "R4");

    let legacy = serde_json::to_value(StreamEventRequest {
        claim,
        identity,
        event: StreamEvent {
            run_id: RunId("run-live".into()),
            kind: AgentEvent::Delta(Delta::TextDelta {
                delta: "legacy".into(),
            }),
        },
    })
    .unwrap();
    let (status, value) = post(&router, "worker-live", "/v1/worker/dispatch/stream", legacy).await;
    assert_eq!(
        (status, value["accepted"].as_bool()),
        (StatusCode::OK, Some(true)),
        "R6"
    );
    assert_eq!(stream.events().len(), 2, "R2-R5 never forward; R6 does");
    assert!(
        stream.observations()[1].assistant_response.is_none(),
        "R6/E4 legacy context stays absent"
    );
}
