// @formal evidence session_event_batch.processed_requires_commit_observation_activity_and_dispatch_settlement
// @formal evidence session_run.stale_work_epoch_cannot_observe_authoritative_thread_commit
//! The worker-facing dispatch transport over its real HTTP surface: a database-less
//! worker's `enqueue` → `claim` → `renew` → `settle` round-trip drives the same
//! process-shared dispatch store the co-located pool drains. Its own test binary: it
//! installs a one-shot injected dispatch store, so it must not share a process with
//! other dispatch tests.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

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
use awaken_dispatch::{
    DispatchSettlementError, DispatchSettlementObserver, MemoryDispatchStore, RunClaim,
    RunDispatch, StreamEventRequest, StreamObservationRequest, WorkerIdentity,
};
use awaken_dispatch_http::{WorkerDispatchService, dispatch_transport_router_with_service};
use awaken_lease_testkit::ManualClock;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_store_inmem::MemoryStreamSink;
use awaken_worker_transport_security::{FixedWorkerLeasePolicy, HeaderWorkerAuthenticator};
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
                max_steps: Some(8),
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

#[derive(Default)]
struct RecordingSessionCoordination {
    finalized: Mutex<Vec<awaken_session_execution_contract::SessionAgentBoundaryCommand>>,
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionAgentCoordination for RecordingSessionCoordination {
    // Fixture rule P1: the transport fixture observes settlement without admitting activity.
    // Prepared preparation refuses without changing any recorded effect.
    async fn prepare_prepared_session_run_activity(
        &self,
        _session: &awaken_session_types::SessionId,
        _agent: &str,
        _run: &awaken_agent_contract::agent::run::Id,
        _mode: awaken_session_execution_contract::SessionRunActivityAdmissionMode,
    ) -> Result<Option<awaken_session_types::SessionRevision>, awaken_session_types::RunError> {
        Err(awaken_session_types::RunError::unavailable(
            "the transport fixture observes settlement without admitting activity",
        ))
    }

    // Fixture rule P1: the transport fixture observes settlement without admitting activity.
    // Prepared admission refuses without changing any recorded effect.
    async fn admit_prepared_session_run_activity(
        &self,
        _session: &awaken_session_types::SessionId,
        _run: &awaken_agent_contract::agent::run::Id,
        _mode: awaken_session_execution_contract::SessionRunActivityAdmissionMode,
        _preparation: awaken_session_execution_contract::PreparedSessionRunActivity,
    ) -> Result<
        awaken_session_execution_contract::SessionRunActivityAdmission,
        awaken_session_types::RunError,
    > {
        Err(awaken_session_types::RunError::unavailable(
            "the transport fixture observes settlement without admitting activity",
        ))
    }

    async fn admit_session_run_activity(
        &self,
        _session_id: &awaken_session_types::SessionId,
        _agent_id: &str,
        _run_id: &RunId,
        _mode: awaken_session_execution_contract::SessionRunActivityAdmissionMode,
    ) -> Result<
        awaken_session_execution_contract::SessionRunActivityAdmission,
        awaken_session_types::RunError,
    > {
        Err(awaken_session_types::RunError::bad_request(
            "settlement fixture does not admit Session activity",
        ))
    }

    async fn admit_session_model_request(
        &self,
        _session_id: &awaken_session_types::SessionId,
        _thread_id: &ThreadId,
        _run_id: &RunId,
    ) -> Result<bool, awaken_session_types::RunError> {
        Err(awaken_session_types::RunError::bad_request(
            "settlement fixture does not admit model requests",
        ))
    }

    async fn list_session_agents(
        &self,
        _session_id: &awaken_session_types::SessionId,
    ) -> Result<
        Vec<awaken_session_execution_contract::SessionAgentRosterEntry>,
        awaken_session_types::RunError,
    > {
        Ok(Vec::new())
    }

    async fn send_session_agent_message(
        &self,
        _command: awaken_session_execution_contract::SessionAgentMessageCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionAgentMessageReceipt,
        awaken_session_types::RunError,
    > {
        Err(awaken_session_types::RunError::bad_request("unused"))
    }

    async fn settle_session_agent_boundary(
        &self,
        _command: awaken_session_execution_contract::SessionAgentBoundaryCommand,
    ) -> Result<(), awaken_session_types::RunError> {
        Ok(())
    }

    async fn finalize_session_agent_boundary(
        &self,
        command: awaken_session_execution_contract::SessionAgentBoundaryCommand,
    ) -> Result<(), awaken_session_types::RunError> {
        self.finalized.lock().unwrap().push(command);
        Ok(())
    }

    async fn interrupt_session_thread(
        &self,
        _session_id: &awaken_session_types::SessionId,
        _child_thread_id: &ThreadId,
    ) -> Result<(), awaken_session_types::RunError> {
        Ok(())
    }

    async fn reply_session_thread_tool(
        &self,
        _command: awaken_session_execution_contract::SessionThreadToolReplyCommand,
    ) -> Result<(), awaken_session_types::RunError> {
        Ok(())
    }
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
    acquisitions: AtomicUsize,
    epoch: AtomicU64,
    unavailable: AtomicBool,
    mode: Mutex<WorkMode>,
    foreign_session: Mutex<Option<String>>,
}

#[derive(Clone, Copy, Default)]
enum WorkMode {
    #[default]
    Owned,
    Retired,
    Foreign,
    ReleaseRace,
    ReleaseResponseLost,
}

#[async_trait::async_trait]
impl awaken_session_application_contract::SessionWorkerLeaseAuthority for RecordingSessionWork {
    async fn acquire_session_work(
        &self,
        session_id: &awaken_session_types::SessionId,
        worker_owner: &str,
        now_ms: u64,
        _acquisition: awaken_session_application_contract::SessionWorkAcquisition,
    ) -> Result<
        awaken_session_application_contract::SessionWorkOwnership,
        awaken_session_types::RunError,
    > {
        self.acquisitions.fetch_add(1, Ordering::SeqCst);
        if self.unavailable.load(Ordering::SeqCst) {
            return Err(awaken_session_types::RunError::unavailable(
                "injected Work authority outage",
            ));
        }
        let mode = *self.mode.lock().unwrap();
        let foreign = matches!(mode, WorkMode::Foreign)
            || self.foreign_session.lock().unwrap().as_deref() == Some(session_id.as_str());
        if matches!(mode, WorkMode::Retired) {
            return Ok(awaken_session_application_contract::SessionWorkOwnership::Unowned);
        }
        Ok(
            awaken_session_application_contract::SessionWorkOwnership::Leased(
                awaken_session_types::SessionWorkLease::from_persisted(
                    format!("work-{session_id}"),
                    "env",
                    session_id.clone(),
                    if foreign {
                        "other-worker"
                    } else {
                        worker_owner
                    },
                    self.epoch.load(Ordering::SeqCst).max(1),
                    now_ms + 60_000,
                )
                .map(awaken_session_application_contract::SessionWorkGrant::from_transfer)
                .expect("nonzero epoch"),
            ),
        )
    }

    async fn release_session_work(
        &self,
        _lease: &awaken_session_application_contract::SessionWorkGrant,
    ) -> Result<bool, awaken_session_types::RunError> {
        self.releases.fetch_add(1, Ordering::SeqCst);
        let mut mode = self.mode.lock().unwrap();
        match *mode {
            WorkMode::ReleaseRace => Ok(false),
            WorkMode::ReleaseResponseLost => {
                *mode = WorkMode::Retired;
                Err(awaken_session_types::RunError::unavailable(
                    "release committed but its response was lost",
                ))
            }
            WorkMode::Foreign => panic!("cannot release another Work owner"),
            _ => Ok(true),
        }
    }

    async fn release_worker_session_work(
        &self,
        _worker_owner: &str,
    ) -> Result<usize, awaken_session_types::RunError> {
        Ok(0)
    }
}

#[tokio::test]
async fn foreign_session_work_defers_only_its_dispatch_and_leaves_other_sessions_runnable() {
    // Cause/effect graph: C1 the earliest Session Run is claimed but its Work
    // lease belongs to another Worker; C2 a later independent Session is
    // runnable; C3 the store clock is before/at the exact retry deadline and
    // Work ownership becomes available; C4 the Work authority itself fails.
    // Effects: E1 C1 returns no claim and
    // retains the first Run without a physical attempt or retry-budget cost;
    // E2 C1+C2 admits the second Run without an HTTP conflict; E3 C3 rejects
    // early re-entry and then admits the original Run once due; E4 C4 returns
    // unavailable and retains the row. Decision table:
    // R1 C1 -> E1; R2 C1+C2 -> E2; R3 C1+C3(before) -> no claim;
    // R4 C1+C3(due)+available Work -> E3; R5 C4 -> E4.
    let clock = Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let store = Arc::new(MemoryDispatchStore::new(clock.clone()));
    for (run, session) in [("busy-run", "busy-session"), ("ready-run", "ready-session")] {
        store
            .enqueue(
                RunDispatch::new(activation(run, session))
                    .for_session(ThreadId(session.into()))
                    .with_session_activity_epoch(1),
            )
            .await
            .expect("enqueue independent Session Run");
    }
    let work = Arc::new(RecordingSessionWork::default());
    *work.foreign_session.lock().unwrap() = Some("busy-session".into());
    let router = dispatch_transport_router_with_service(Arc::new(
        WorkerDispatchService::new(
            store.clone(),
            Arc::new(HeaderWorkerAuthenticator),
            Arc::new(ManualClock::new(0)),
            Arc::new(FixedWorkerLeasePolicy::new(30_000)),
        )
        .with_session_work_authority(work.clone()),
    ));

    let (status, first) = post(&router, "worker-1", "/v1/worker/dispatch/claim", json!({})).await;
    assert_eq!(status, StatusCode::OK, "R1/E1: {first}");
    assert!(
        first["claimed"].is_null(),
        "R1/E1 no foreign Work authority"
    );
    let rows = store.list_dispatches().await.unwrap();
    let blocked = rows.iter().find(|row| row.run_id.0 == "busy-run").unwrap();
    assert_eq!(
        blocked.state,
        awaken_dispatch::DispatchState::Pending,
        "R1/E1"
    );
    assert_eq!(blocked.attempt_count, 0, "R1/E1 no physical retry cost");
    assert!(!blocked.physical_attempt_active, "R1/E1 no execution");

    let (status, second) = post(&router, "worker-1", "/v1/worker/dispatch/claim", json!({})).await;
    assert_eq!(status, StatusCode::OK, "R2/E2: {second}");
    assert_eq!(
        second["claimed"]["request"]["activation"]["run_id"], "ready-run",
        "R2/E2"
    );
    assert!(
        second["claimed"]["session_work_authority"].is_object(),
        "R2/E2 exact Work receipt"
    );

    clock.set(4_999);
    let (status, early) = post(&router, "worker-1", "/v1/worker/dispatch/claim", json!({})).await;
    assert_eq!(status, StatusCode::OK, "R3: {early}");
    assert!(early["claimed"].is_null(), "R3 blocked Run is not yet due");

    *work.foreign_session.lock().unwrap() = None;
    clock.set(5_000);
    let (status, due) = post(&router, "worker-1", "/v1/worker/dispatch/claim", json!({})).await;
    assert_eq!(status, StatusCode::OK, "R4/E3: {due}");
    assert_eq!(
        due["claimed"]["request"]["activation"]["run_id"], "busy-run",
        "R4/E3"
    );
    assert!(
        due["claimed"]["session_work_authority"].is_object(),
        "R4/E3 exact Work receipt"
    );

    store
        .enqueue(
            RunDispatch::new(activation("outage-run", "outage-session"))
                .for_session(ThreadId("outage-session".into()))
                .with_session_activity_epoch(1),
        )
        .await
        .expect("enqueue outage probe");
    work.unavailable.store(true, Ordering::SeqCst);
    let (status, outage) = post(&router, "worker-1", "/v1/worker/dispatch/claim", json!({})).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "R5/E4: {outage}");
    let rows = store.list_dispatches().await.unwrap();
    let outage_row = rows
        .iter()
        .find(|row| row.run_id.0 == "outage-run")
        .unwrap();
    assert_eq!(
        outage_row.state,
        awaken_dispatch::DispatchState::Pending,
        "R5/E4"
    );
    assert!(!outage_row.physical_attempt_active, "R5/E4");
}

fn terminal_snapshot(thread_id: &ThreadId, run_id: &RunId, state: RunState) -> RunRecoverySnapshot {
    RunRecoverySnapshot {
        commit_timestamps: Default::default(),
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

#[derive(Clone, Copy)]
enum TerminalThread {
    Child,
    Primary,
    ImplicitPrimary,
}

async fn claimed_terminal_dispatch(
    tag: &str,
    placement: TerminalThread,
) -> (Arc<MemoryDispatchStore>, RunClaim, ThreadId, ThreadId) {
    let run_id = RunId(format!("terminal-run-{tag}"));
    let thread_id = ThreadId(format!("terminal-child-{tag}"));
    let child = matches!(placement, TerminalThread::Child);
    let session_thread_id = if child {
        ThreadId(format!("terminal-parent-{tag}"))
    } else {
        thread_id.clone()
    };
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let store = Arc::new(MemoryDispatchStore::new(clock.clock()));
    let mut request = RunDispatch::new(activation(&run_id.0, &thread_id.0))
        .with_traceparent(Some(TERMINAL_TRACEPARENT.to_string()));
    if !matches!(placement, TerminalThread::ImplicitPrimary) {
        request = request.for_session(session_thread_id.clone());
    }
    if !child {
        // Settlement starts after the root's activity receipt. Unreceipted
        // Session admission belongs to the reservation conformance suite.
        request = request.with_session_activity_epoch(1);
    }
    store.enqueue(request).await.unwrap();
    let claimed = store
        .claim("terminal-worker", 30_000, &Default::default())
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

#[tokio::test]
async fn applied_remote_dispatch_invokes_one_exact_post_settlement_callback() {
    // Cause/effect decision table: R1 exact root claim + committed Ended Run +
    // non-zero Session activity + Applied Dispatch => one callback carrying
    // those exact coordinates; R2 retry after the Dispatch row is gone =>
    // settled=false and no second callback. Invalid claim/state combinations
    // are covered by the surrounding transport table; the contract Kani proof
    // exhausts Event receipt/activity combinations.
    // R3: the transport's established primary fallback omits the optional
    // affinity field; its guarded root still selects the exact Session. Cross
    // explicit/implicit primary with R1 Applied and R2 retired-claim replay.
    for placement in [TerminalThread::Primary, TerminalThread::ImplicitPrimary] {
        let (store, claim, session_id, thread_id) =
            claimed_terminal_dispatch("post-dispatch-session", placement).await;
        let recovery = terminal_recovery(
            &session_id,
            &thread_id,
            &claim.run_id,
            Ok(terminal_snapshot(
                &thread_id,
                &claim.run_id,
                RunState::Ended(EndCause::NaturalEnd),
            )),
        );
        let coordination = Arc::new(RecordingSessionCoordination::default());
        let router = dispatch_transport_router_with_service(Arc::new(
            WorkerDispatchService::new(
                store,
                Arc::new(HeaderWorkerAuthenticator),
                Arc::new(ManualClock::new(0)),
                Arc::new(FixedWorkerLeasePolicy::new(30_000)),
            )
            .with_recovery_source(recovery)
            .with_session_coordination(coordination.clone()),
        ));

        let (status, applied) = settle_terminal(&router, &claim).await;
        assert_eq!(status, StatusCode::OK, "R1: {applied}");
        assert_eq!(applied["settled"], true, "R1");
        {
            // Inspect R1 under a lexical lock scope; replay must never carry a
            // synchronous fixture guard into the next awaited transport operation.
            let finalized = coordination.finalized.lock().unwrap();
            assert_eq!(finalized.len(), 1, "R1");
            assert_eq!(finalized[0].session_id, session_id.0, "R1 exact Session");
            assert_eq!(finalized[0].source_thread_id, thread_id, "R1 exact Thread");
            assert_eq!(finalized[0].source_run_id, claim.run_id, "R1 exact Run");
            assert_eq!(finalized[0].session_activity_epoch, 1, "R1 exact epoch");
        }

        let (status, replay) = settle_terminal(&router, &claim).await;
        assert_eq!(status, StatusCode::OK, "R2: {replay}");
        assert_eq!(replay["settled"], false, "R2");
        assert_eq!(coordination.finalized.lock().unwrap().len(), 1, "R2");
    }
}

#[tokio::test]
async fn session_recovery_reads_committed_truth_only_under_the_exact_work_epoch() {
    // Cause/effect graph: C1 the Dispatch claim is exact; C2 the receipt names
    // the claimed Session and authenticated incarnation; C3 Work id,
    // Environment, and epoch equal the canonical Work observation; C4 the Work
    // authority is available. E1 only C1+C2+C3+C4 may read and return committed
    // Thread truth; E2 every missing, mismatched, stale, or unavailable Work
    // observation rejects before RunRecoverySource is called.
    //
    // | Rule | C1 | C2 | C3 | C4 | Effect |
    // |---|---|---|---|---|---|
    // | W1 exact | yes | yes | yes | yes | E1 one read |
    // | W2 same-owner ABA | yes | yes | stale epoch | yes | E2 zero reads |
    // | W3 wrong Work/Environment | yes | yes | no | yes | E2 zero reads |
    // | W4 wrong Session/incarnation | yes | no | n/a | yes | E2 zero reads |
    // | W5 missing receipt | yes | no | n/a | yes | E2 zero reads |
    // | W6 authority outage | yes | yes | unknown | no | E2 zero reads |
    // Constraint: the receipt comes from claim admission and recovery may never
    // self-sign the latest owner/epoch. The read-count assertion makes the
    // validation-before-Thread-read ordering observable.
    let store = Arc::new(MemoryDispatchStore::new(awaken_time::system_clock()));
    let run_id = RunId("work-epoch-recovery-run".into());
    let session_id = ThreadId("work-epoch-session".into());
    let request = RunDispatch::new(activation(&run_id.0, &session_id.0))
        .for_session(session_id.clone())
        .with_session_activity_epoch(1);
    store.enqueue(request).await.expect("enqueue Session Run");
    let recovery = terminal_recovery(
        &session_id,
        &session_id,
        &run_id,
        Ok(terminal_snapshot(
            &session_id,
            &run_id,
            RunState::Ended(EndCause::NaturalEnd),
        )),
    );
    let work = Arc::new(RecordingSessionWork::default());
    let router = dispatch_transport_router_with_service(Arc::new(
        WorkerDispatchService::new(
            store,
            Arc::new(HeaderWorkerAuthenticator),
            Arc::new(ManualClock::new(0)),
            Arc::new(FixedWorkerLeasePolicy::new(30_000)),
        )
        .with_recovery_source(recovery.clone())
        .with_session_work_authority(work.clone()),
    ));

    let (status, body) = post(
        &router,
        "work-epoch-worker",
        "/v1/worker/dispatch/claim",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "claim: {body}");
    let claimed: awaken_dispatch::Claimed =
        serde_json::from_value(body["claimed"].clone()).expect("claimed response");
    let claim = RunClaim::from(&claimed.lease);
    let receipt = claimed
        .session_work_authority
        .clone()
        .expect("claim edge issues exact Session Work receipt");
    let exact_request = json!({
        "claim": claim,
        "session_work_authority": receipt,
    });

    // W0: a missing/zero Dispatch epoch fails the canonical wire decoder;
    // the rejection precedes any Work lookup or committed Thread read. Positive
    // exact grants continue into W1 below; the ABA mismatch still uses W2.
    for epoch in [None, Some(0)] {
        let mut invalid = exact_request.clone();
        invalid["claim"].as_object_mut().unwrap().remove("epoch");
        if let Some(epoch) = epoch {
            invalid["claim"]["epoch"] = epoch.into();
        }
        let (status, _) = post(
            &router,
            "work-epoch-worker",
            "/v1/worker/recovery/snapshot",
            invalid,
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "W0 {epoch:?}");
        assert!(
            recovery.observed.lock().unwrap().is_empty(),
            "W0 zero reads"
        );
    }

    let (status, _) = post(
        &router,
        "work-epoch-worker",
        "/v1/worker/recovery/snapshot",
        exact_request.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "W1");
    assert_eq!(recovery.observed.lock().unwrap().len(), 1, "W1");

    work.epoch.store(2, Ordering::SeqCst);
    recovery.observed.lock().unwrap().clear();
    let (status, _) = post(
        &router,
        "work-epoch-worker",
        "/v1/worker/recovery/snapshot",
        exact_request.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "W2 same-owner ABA");
    assert!(recovery.observed.lock().unwrap().is_empty(), "W2");

    work.epoch.store(1, Ordering::SeqCst);
    for (tag, field, value) in [
        ("W3-work", "work_id", json!("other-work")),
        ("W3-environment", "environment_id", json!("other-env")),
        ("W4-session", "session_id", json!("other-session")),
        ("W4-owner", "worker_owner", json!("other-incarnation")),
    ] {
        let mut request = exact_request.clone();
        let receipt = request["session_work_authority"]
            .as_object_mut()
            .expect("receipt object");
        if field == "work_id" || field == "environment_id" {
            receipt["authority"]
                .as_object_mut()
                .expect("authority object")
                .insert(field.into(), value);
        } else {
            receipt.insert(field.into(), value);
        }
        let (status, _) = post(
            &router,
            "work-epoch-worker",
            "/v1/worker/recovery/snapshot",
            request,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{tag}");
        assert!(recovery.observed.lock().unwrap().is_empty(), "{tag}");
    }

    let (status, _) = post(
        &router,
        "work-epoch-worker",
        "/v1/worker/recovery/snapshot",
        json!({ "claim": claim }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "W5");
    assert!(recovery.observed.lock().unwrap().is_empty(), "W5");

    work.unavailable.store(true, Ordering::SeqCst);
    let (status, _) = post(
        &router,
        "work-epoch-worker",
        "/v1/worker/recovery/snapshot",
        exact_request,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "W6");
    assert!(recovery.observed.lock().unwrap().is_empty(), "W6");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_db_less_worker_claims_renews_and_settles_over_http() {
    // The store is on wall time on purpose, and the manual clock below is not a
    // mirror of it. Every fenced outcome this test asserts is decided by the
    // claim *epoch*, not by a deadline: the stale-owner settles it rejects carry
    // a non-current epoch, and the 30s lease never lapses inside a test that
    // runs in milliseconds. The manual clock drives the service's lease policy,
    // which is the injected `WallClock`'s job. Handing the store a second, mirrored clock —
    // as `signed_worker_transport_http.rs` legitimately does, because there the
    // store does judge a deadline — would add machinery here that decides
    // nothing.
    let mem = Arc::new(MemoryDispatchStore::new(awaken_time::system_clock()));
    let clock = Arc::new(ManualClock::new(0));
    let router = dispatch_transport_router_with_service(Arc::new(WorkerDispatchService::new(
        mem as Arc<dyn WorkerDispatchAuthority>,
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

    // Causes: H0 epoch missing/zero crossed with Renew/Settle entrance.
    // Effect: invalid JSON is rejected before a typed authority reaches the
    // service. The subsequent exact renewal/settlement and empty final claim
    // verify that rejected input neither retires nor replaces the live grant.
    for (path, key, body) in [
        (
            "/v1/worker/dispatch/renew",
            "lease_epoch",
            json!({"run_id": "run-A"}),
        ),
        (
            "/v1/worker/dispatch/settle",
            "epoch",
            json!({"run_id": "run-A", "outcome": "Done"}),
        ),
    ] {
        for epoch in [None, Some(0)] {
            let mut invalid = body.clone();
            if let Some(epoch) = epoch {
                invalid[key] = epoch.into();
            }
            let (status, _) = post(&router, "worker-1", path, invalid).await;
            assert_eq!(
                status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "H0 {path} {epoch:?}"
            );
        }
    }

    // Renewal transport rule H1. Causes: the authenticated Worker still owns the
    // exact run+epoch returned by claim. Effect: Control derives owner/time/lease
    // policy, renews that exact claim, and returns `current`. The complementary missing-
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
    assert_eq!(
        v["outcome"], "current",
        "the owner renews its live lease: {v}"
    );

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
async fn occupied_physical_attempt_blocks_terminal_observers_and_work_release() {
    // Cause/effect table R2: committed terminal + exact current claim + an
    // occupied physical slot => zero observer/recovery/Work effects and no
    // settlement. R5: exact finish ACK => ordinary observer retry then settle.
    // Root/child both use the same HTTP authority; a child's logical outcome
    // must not unlock parent activity before the physical execution quiesces.
    for child in [false, true] {
        let (store, claim, session, thread) = claimed_terminal_dispatch(
            "occupied",
            if child {
                TerminalThread::Child
            } else {
                TerminalThread::Primary
            },
        )
        .await;
        store.begin_attempt(&claim, None).await.unwrap();
        let observer = Arc::new(FailFirstTerminalSettlement::default());
        let work = Arc::new(RecordingSessionWork::default());
        let recovery = terminal_recovery(
            &session,
            &thread,
            &claim.run_id,
            Ok(terminal_snapshot(
                &thread,
                &claim.run_id,
                RunState::Ended(EndCause::Indeterminate),
            )),
        );
        let router = dispatch_transport_router_with_service(Arc::new(
            WorkerDispatchService::new(
                store.clone(),
                Arc::new(HeaderWorkerAuthenticator),
                Arc::new(ManualClock::new(0)),
                Arc::new(FixedWorkerLeasePolicy::new(30_000)),
            )
            .with_recovery_source(recovery.clone())
            .with_terminal_observer(observer.clone())
            .with_session_work_authority(work.clone()),
        ));
        for _ in 0..2 {
            let (status, value) = settle_terminal(&router, &claim).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(value["settled"], false, "R2 pending physical confirmation");
        }
        assert_eq!(observer.attempts.load(Ordering::SeqCst), 0);
        assert_eq!(work.acquisitions.load(Ordering::SeqCst), 0);
        assert_eq!(work.releases.load(Ordering::SeqCst), 0);
        assert!(recovery.observed.lock().unwrap().is_empty());
        assert_eq!(store.list_dispatches().await.unwrap().len(), 1);
        store.finish_attempt(&claim, None).await.unwrap();
        assert_eq!(
            settle_terminal(&router, &claim).await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        let (status, value) = settle_terminal(&router, &claim).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            value["settled"], true,
            "R5 resumes original settlement path"
        );
        assert_eq!(observer.successes.load(Ordering::SeqCst), 1);
        assert_eq!(work.releases.load(Ordering::SeqCst), usize::from(!child));
    }
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
        claimed_terminal_dispatch("observer-retry", TerminalThread::Child).await;
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
            Arc::new(ManualClock::new(0)),
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
        awaken_dispatch::DispatchState::Leased,
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
        let (store, claim, session_thread_id, thread_id) =
            claimed_terminal_dispatch(tag, TerminalThread::Child).await;
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
            Arc::new(ManualClock::new(0)),
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
            awaken_dispatch::DispatchState::Leased,
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

#[tokio::test]
async fn committed_root_done_settles_after_work_retirement_without_reopening_execution() {
    // Causes: C1 current authenticated Dispatch claim; C2 matching committed
    // Ended/Running/Awaiting Run; C3 Done/Awaiting request; C4 Work owned,
    // retired, foreign, or lost between acquisition and release; C5 terminal
    // observation configured. Effects: E1 close Done exactly once; E2 release
    // only an owned Work; E3 retain dispatch for missing/nonterminal proof;
    // E4 stale epoch/foreign caller cause no recovery, Work, or observer effects.
    // Constraint: Work retirement remains the execution fence. Only C1+C2=Ended
    // +C3=Done may finish an already committed Run after that fence disappears.
    // | Rule | Run | request | Work | observer | status | release | outcome |
    // | S1 | Cancelled | Done | retired | yes | 200 | 0 | E1 |
    // | S2 | NaturalEnd | Done | foreign | yes | 200 | 0 | E1,E2 |
    // | S3 | NaturalEnd | Done | release race | yes | 200 | 1 | E1,E2 |
    // | S4 | NaturalEnd | Done | owned | yes | 200 | 1 | E1,E2 |
    // | S5 | Running | Done | retired | yes | 400 | 0 | E3 |
    // | S6 | Awaiting | Done | retired | yes | 400 | 0 | E3 |
    // | S7 | Awaiting | Awaiting | retired | yes | 400 | 0 | E3 |
    // | S8 | Ended | Done | retired | absent | 400 | 0 | E3 |
    // | S9 | Ended | Done | owned | yes, stale claim/caller | 200 | 0 | E4 |
    // Existing child R1-R6 cover mismatched/missing/unavailable recovery and
    // observer failure; child settlement never releases borrowed parent Work.
    let rows = [
        (
            "S1",
            RunState::Ended(EndCause::Cancelled),
            "Done",
            WorkMode::Retired,
            true,
            true,
            0,
        ),
        (
            "S2",
            RunState::Ended(EndCause::NaturalEnd),
            "Done",
            WorkMode::Foreign,
            true,
            true,
            0,
        ),
        (
            "S3",
            RunState::Ended(EndCause::NaturalEnd),
            "Done",
            WorkMode::ReleaseRace,
            true,
            true,
            1,
        ),
        (
            "S4",
            RunState::Ended(EndCause::NaturalEnd),
            "Done",
            WorkMode::Owned,
            true,
            true,
            1,
        ),
        (
            "S5",
            RunState::Running,
            "Done",
            WorkMode::Retired,
            true,
            false,
            0,
        ),
        (
            "S6",
            RunState::Awaiting,
            "Done",
            WorkMode::Retired,
            true,
            false,
            0,
        ),
        (
            "S7",
            RunState::Awaiting,
            "Awaiting",
            WorkMode::Retired,
            true,
            false,
            0,
        ),
        (
            "S8",
            RunState::Ended(EndCause::NaturalEnd),
            "Done",
            WorkMode::Retired,
            false,
            false,
            0,
        ),
    ];
    for (tag, state, outcome, mode, observe, accepted, releases) in rows {
        let (store, claim, session_id, thread_id) =
            claimed_terminal_dispatch(tag, TerminalThread::Primary).await;
        let recovery = terminal_recovery(
            &session_id,
            &thread_id,
            &claim.run_id,
            Ok(terminal_snapshot(&thread_id, &claim.run_id, state)),
        );
        let observer = Arc::new(FailFirstTerminalSettlement {
            attempts: AtomicUsize::new(1),
            ..Default::default()
        });
        let work = Arc::new(RecordingSessionWork {
            mode: Mutex::new(mode),
            ..Default::default()
        });
        let mut service = WorkerDispatchService::new(
            store.clone(),
            Arc::new(HeaderWorkerAuthenticator),
            Arc::new(ManualClock::new(0)),
            Arc::new(FixedWorkerLeasePolicy::new(30_000)),
        )
        .with_recovery_source(recovery.clone())
        .with_session_work_authority(work.clone());
        if observe {
            service = service.with_terminal_observer(observer.clone());
        }
        let router = dispatch_transport_router_with_service(Arc::new(service));
        for (worker, epoch) in [
            ("other-worker", claim.epoch),
            ("terminal-worker", claim.epoch + 1),
        ] {
            let (status, value) = post(
                &router,
                worker,
                "/v1/worker/dispatch/settle",
                json!({"run_id":claim.run_id.0,"epoch":epoch,"outcome":outcome,"consumed":[]}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{tag}/S9: {value}");
            assert_eq!(value["settled"], false, "{tag}/S9");
        }
        assert!(recovery.observed.lock().unwrap().is_empty(), "{tag}/S9");
        assert_eq!(work.acquisitions.load(Ordering::SeqCst), 0, "{tag}/S9");
        assert_eq!(observer.successes.load(Ordering::SeqCst), 0, "{tag}/S9");
        let (status, value) = post(
            &router,
            "terminal-worker",
            "/v1/worker/dispatch/settle",
            json!({"run_id":claim.run_id.0,"epoch":claim.epoch,"outcome":outcome,"consumed":[]}),
        )
        .await;
        assert_eq!(
            status,
            if accepted {
                StatusCode::OK
            } else {
                StatusCode::BAD_REQUEST
            },
            "{tag}: {value}"
        );
        assert_eq!(work.releases.load(Ordering::SeqCst), releases, "{tag}/E2");
        assert_eq!(
            store.list_dispatches().await.unwrap().is_empty(),
            accepted,
            "{tag}/E1,E3"
        );
        if accepted {
            assert_eq!(value["settled"], true, "{tag}/E1");
            let (_, replay) = settle_terminal(&router, &claim).await;
            assert_eq!(
                replay["settled"], false,
                "{tag}/replay has no current claim"
            );
            assert_eq!(
                work.releases.load(Ordering::SeqCst),
                releases,
                "{tag}/replay"
            );
            assert_eq!(observer.successes.load(Ordering::SeqCst), 1, "{tag}/replay");
        }
    }
}

#[tokio::test]
async fn work_release_response_loss_retries_done_from_the_same_committed_run() {
    // S10: current claim + Ended -> Work release commits but response fails ->
    // HTTP 500 retains Dispatch. S11: same claim retries + Work now retired ->
    // re-observe the same committed Run, settle Done, no second Work release.
    // No new execution, Work revival, or fabricated Runtime terminal is allowed.
    let (store, claim, session_id, thread_id) =
        claimed_terminal_dispatch("release-loss", TerminalThread::Primary).await;
    let recovery = terminal_recovery(
        &session_id,
        &thread_id,
        &claim.run_id,
        Ok(terminal_snapshot(
            &thread_id,
            &claim.run_id,
            RunState::Ended(EndCause::Cancelled),
        )),
    );
    let work = Arc::new(RecordingSessionWork {
        mode: Mutex::new(WorkMode::ReleaseResponseLost),
        ..Default::default()
    });
    let observer = Arc::new(FailFirstTerminalSettlement {
        attempts: AtomicUsize::new(1),
        ..Default::default()
    });
    let router = dispatch_transport_router_with_service(Arc::new(
        WorkerDispatchService::new(
            store.clone(),
            Arc::new(HeaderWorkerAuthenticator),
            Arc::new(ManualClock::new(0)),
            Arc::new(FixedWorkerLeasePolicy::new(30_000)),
        )
        .with_recovery_source(recovery.clone())
        .with_terminal_observer(observer)
        .with_session_work_authority(work.clone()),
    ));
    assert_eq!(
        settle_terminal(&router, &claim).await.0,
        StatusCode::INTERNAL_SERVER_ERROR,
        "S10"
    );
    assert_eq!(
        store.list_dispatches().await.unwrap().len(),
        1,
        "S10 retry evidence"
    );
    let (status, value) = settle_terminal(&router, &claim).await;
    assert_eq!(status, StatusCode::OK, "S11: {value}");
    assert_eq!(value["settled"], true, "S11");
    assert!(store.list_dispatches().await.unwrap().is_empty(), "S11");
    assert_eq!(
        work.releases.load(Ordering::SeqCst),
        1,
        "S10/S11 one physical release"
    );
    assert_eq!(
        recovery.observed.lock().unwrap().as_slice(),
        [
            (session_id.clone(), thread_id.clone(), claim.run_id.clone()),
            (session_id, thread_id, claim.run_id)
        ],
        "S10/S11 exact recovery coordinates"
    );
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
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let mem = Arc::new(MemoryDispatchStore::new(clock.clock()));
    let stream = Arc::new(MemoryStreamSink::new());
    let router = dispatch_transport_router_with_service(Arc::new(
        WorkerDispatchService::new(
            mem.clone() as Arc<dyn WorkerDispatchAuthority>,
            Arc::new(HeaderWorkerAuthenticator),
            Arc::new(ManualClock::new(0)),
            Arc::new(FixedWorkerLeasePolicy::new(30_000)),
        )
        .with_stream_sink(stream.clone()),
    ));
    mem.enqueue(RunDispatch::new(activation("run-live", "thread-live")))
        .await
        .unwrap();
    let claimed = mem
        .claim("worker-live", 30_000, &Default::default())
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

use awaken_dispatch_contract::{
    DispatchClaims as _, DispatchEnqueue as _, DispatchExecution as _, DispatchReads as _,
};

use awaken_dispatch_http::WorkerDispatchAuthority;

#[path = "dispatch_transport_http/terminal_work_ack.rs"]
mod terminal_work_ack;
