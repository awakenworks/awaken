//! T33-C03 (cross-adapter consistency), HTTP Worker row.
//!
//! The production Native `Runtime` runs Worker-side behind the production
//! `ClaimedCommitCoordinator` + `RecoveryProjection`, whose every step commit
//! crosses the real `/v1/worker/commit-claimed` HTTP boundary
//! (`RemoteClaimedRunCommit` -> axum `claimed_commit_router` ->
//! `ClaimedCommitService` -> the Control-side commit authority). The injected
//! upstream fault is the shared fixture of this scenario (see
//! `awaken-runtime/tests/cross_adapter_fault_local.rs`):
//!
//! | fault | upstream answer                                     | authority                        | uncertain | retry hint      |
//! | F1    | 529 Overloaded, Retry-After 7s, proved undispatched | `Error(Inference{"overloaded"})` | no        | retryable class |
//! | F2    | the same 529 without dispatch evidence (Unknown)    | `Indeterminate`                  | yes       | never repeated  |
//!
//! Oracles: O0 the Control-side committed terminal keeps the authority,
//! uncertainty and retry hint the Worker produced; O1 it is never a success.
//! Negative control (executed): a Worker transport mutant that rewrites the
//! terminal to `NaturalEnd` (and forges the receipt hash so the Worker does
//! not notice) is driven through the same HTTP chain; the same checker must
//! reject the resulting Control-side truth.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::Error as CommitError;
use awaken_agent_contract::thread::commit::operation::{CommitReceipt, commit_payload_hash};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::recovery::RunRecoverySource;
use awaken_coordinator_client::RemoteClaimedRunCommit;
use awaken_dispatch::{
    ClaimedCommitCommand, ClaimedCommitCoordinator, ClaimedCommitService, ClaimedRunCommit,
    MemoryDispatchStore, RecoveryProjection, RegisteredWorker, RegistryError, RegistryMutation,
    RunClaim, RunDispatch, WorkerDirectory, WorkerHeartbeat, WorkerIdentity, WorkerManifest,
    WorkerObservationSource, WorkerRegistration, WorkerSnapshot, WorkerState,
};
use awaken_dispatch_contract::{DispatchClaims as _, DispatchEnqueue as _};
use awaken_dispatch_http::{ClaimedCommitHttpService, claimed_commit_router};
use awaken_dispatch_testkit::ConformanceClock as _;
use awaken_runtime::{RetryPolicy, Runtime};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{ChatRequest, ChatResponse, Error as LlmError, LlmExecutor};
use awaken_runtime_contract::model_operation::{ModelExecutionEvidence, NoDispatchProof};
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_store_inmem::MemoryCommitCoordinator;
use awaken_worker_transport_security::HeaderWorkerAuthenticator;

const UPSTREAM_DETAIL: &str = "upstream 529 overloaded_error (retry-after 7s)";
const RETRY_AFTER: Duration = Duration::from_secs(7);
const RETRYABLE_CODES: [&str; 4] = ["provider_error", "rate_limited", "overloaded", "timeout"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InjectedFault {
    Overloaded,
    UnknownOutcome,
}

impl InjectedFault {
    const ALL: [Self; 2] = [Self::Overloaded, Self::UnknownOutcome];

    fn label(self) -> &'static str {
        match self {
            Self::Overloaded => "overloaded",
            Self::UnknownOutcome => "unknown",
        }
    }

    fn upstream_error(self) -> LlmError {
        let overloaded = LlmError::Overloaded {
            message: UPSTREAM_DETAIL.to_string(),
            retry_after: Some(RETRY_AFTER),
        };
        match self {
            Self::Overloaded => overloaded.with_execution_evidence(
                ModelExecutionEvidence::NotDispatched(NoDispatchProof::request_projection()),
            ),
            Self::UnknownOutcome => overloaded,
        }
    }

    fn expected(self) -> Classification {
        match self {
            Self::Overloaded => Classification {
                authority: "overloaded".to_string(),
                uncertain: false,
                retryable: true,
                success: false,
            },
            Self::UnknownOutcome => Classification {
                authority: "indeterminate".to_string(),
                uncertain: true,
                retryable: false,
                success: false,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Classification {
    authority: String,
    uncertain: bool,
    retryable: bool,
    success: bool,
}

fn classify(state: &RunState) -> Classification {
    match state {
        RunState::Ended(EndCause::Error(failure)) => Classification {
            authority: failure.code().to_string(),
            uncertain: false,
            retryable: RETRYABLE_CODES.contains(&failure.code()),
            success: false,
        },
        RunState::Ended(EndCause::Indeterminate) => Classification {
            authority: "indeterminate".to_string(),
            uncertain: true,
            retryable: false,
            success: false,
        },
        RunState::Ended(EndCause::NaturalEnd) => Classification {
            authority: "natural_end".to_string(),
            uncertain: false,
            retryable: false,
            success: true,
        },
        other => Classification {
            authority: format!("{other:?}"),
            uncertain: false,
            retryable: false,
            success: false,
        },
    }
}

fn check(
    adapter: &str,
    expected: &Classification,
    observed: &Classification,
) -> Result<(), String> {
    if observed.success {
        return Err(format!(
            "O1 {adapter}: the injected fault surfaced as success: {observed:?}"
        ));
    }
    if observed != expected {
        return Err(format!(
            "O0 {adapter}: expected {expected:?}, observed {observed:?}"
        ));
    }
    Ok(())
}

struct FaultyUpstream {
    fault: InjectedFault,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl LlmExecutor for FaultyUpstream {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(self.fault.upstream_error())
    }
}

/// Negative-control Worker transport: a lossy adapter that rewrites the
/// terminal to success on the wire and forges the receipt hash so the Worker's
/// own projection does not notice the rewrite.
struct DowngradingTransport {
    inner: RemoteClaimedRunCommit,
}

#[async_trait::async_trait]
impl ClaimedRunCommit for DowngradingTransport {
    async fn commit(
        &self,
        claim: &RunClaim,
        commit: ThreadCommit,
    ) -> Result<CommitRecord, CommitError> {
        self.inner.commit(claim, commit).await
    }

    async fn commit_operation(
        &self,
        mut command: ClaimedCommitCommand,
    ) -> Result<CommitReceipt, CommitError> {
        if !matches!(command.operation.commit.run_state(), RunState::Ended(_)) {
            return self.inner.commit_operation(command).await;
        }
        let original_hash = command.operation.payload_hash.clone();
        let run_id = command.operation.commit.run_id().clone();
        command.operation.commit.run = RunDisposition::ended(run_id, EndCause::NaturalEnd);
        command.operation.payload_hash = commit_payload_hash(&command.operation.commit)
            .map_err(|error| CommitError::Rejected(error.to_string()))?;
        let receipt = self.inner.commit_operation(command).await?;
        Ok(CommitReceipt {
            payload_hash: original_hash,
            ..receipt
        })
    }
}

struct CurrentWorkerDirectory(RegisteredWorker);

#[async_trait::async_trait]
impl WorkerObservationSource for CurrentWorkerDirectory {
    async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError> {
        Ok(vec![self.0.clone()])
    }
}

#[async_trait::async_trait]
impl awaken_worker_contract::WorkerCurrentSource for CurrentWorkerDirectory {
    async fn current(&self, worker_id: &str) -> Result<Option<RegisteredWorker>, RegistryError> {
        Ok((worker_id == self.0.snapshot.identity.worker_id).then(|| self.0.clone()))
    }
}

#[async_trait::async_trait]
impl awaken_worker_contract::WorkerIncarnationSource for CurrentWorkerDirectory {
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

#[async_trait::async_trait]
impl WorkerDirectory for CurrentWorkerDirectory {
    async fn lock_incarnation(
        &self,
        _identity: &WorkerIdentity,
    ) -> Result<Option<awaken_worker_contract::WorkerIncarnationGuard>, RegistryError> {
        Err(RegistryError::Persistence(
            "fixture forbids physical admission".into(),
        ))
    }

    async fn register(
        &self,
        _registration: WorkerRegistration,
        _now_ms: u64,
        _ttl_ms: u64,
    ) -> Result<RegisteredWorker, RegistryError> {
        Ok(self.0.clone())
    }

    async fn heartbeat(
        &self,
        _identity: &WorkerIdentity,
        _heartbeat: WorkerHeartbeat,
        _now_ms: u64,
        _ttl_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        Ok(RegistryMutation::NotFound)
    }

    async fn begin_drain(
        &self,
        _identity: &WorkerIdentity,
        _deadline_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        Ok(RegistryMutation::NotFound)
    }

    async fn mark_quiesced(
        &self,
        _identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        Ok(RegistryMutation::NotFound)
    }

    async fn deregister(
        &self,
        _identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        Ok(RegistryMutation::NotFound)
    }

    async fn expire_incarnation(
        &self,
        _identity: &WorkerIdentity,
        _now_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        Ok(RegistryMutation::InvalidTransition)
    }

    async fn record_process_exit(
        &self,
        _identity: &WorkerIdentity,
        _receipt: awaken_dispatch::WorkerProcessExit,
    ) -> Result<RegistryMutation, RegistryError> {
        Err(RegistryError::Persistence(
            "process-exit persistence is outside this read-oriented fixture".into(),
        ))
    }
}

fn registered(identity: &WorkerIdentity) -> RegisteredWorker {
    let manifest = WorkerManifest::default();
    RegisteredWorker {
        snapshot: WorkerSnapshot {
            identity: identity.clone(),
            state: WorkerState::Ready,
            capability_fingerprint: manifest.fingerprint().unwrap(),
            manifest,
            in_flight: 0,
            warm_environment_shapes: Default::default(),
            credential_observations: Default::default(),
            acp_capability_observations: Default::default(),
            expires_at_ms: u64::MAX,
        },
        heartbeat_sequence: 0,
        observation_sequence: 0,
        registered_at_ms: 0,
        heartbeat_at_ms: 0,
        drain_deadline_ms: None,
        process_exit: None,
        create_ledger: None,
        source_retention: Default::default(),
    }
}

fn activation(run: &str, thread: &str) -> RunActivation {
    let fingerprint = CatalogFingerprint("t33-c03-http-worker".into());
    RunActivation::new(
        RunId(run.into()),
        ThreadId(thread.into()),
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("t33-c03-snapshot".into()),
            metadata: Default::default(),
            root_agent_id: AgentId("t33-c03-agent".into()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: fingerprint.clone(),
                instructions: String::new(),
                max_steps: Some(4),
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding::parse("provider", "model", "local").expect("valid test binding"),
                ),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint,
        },
        vec![Message {
            id: MessageId(format!("{run}-user")),
            role: Role::User,
            content: vec![ContentBlock::text("go")],
        }],
    )
}

/// What one Worker attempt left behind on both sides of the HTTP boundary.
struct WorkerRun {
    /// The terminal the Worker-side runtime returned.
    returned: RunState,
    /// The terminal the Control-side commit authority holds.
    control: RunState,
    /// Committed terminal facts on the Control side.
    control_terminals: usize,
    /// Upstream invocations.
    calls: usize,
}

/// Claim the Run on the Control side, run the production runtime Worker-side
/// with the production claimed-commit coordinator over real HTTP, and read the
/// Control-side truth back.
async fn run_on_http_worker(fault: InjectedFault, downgrade: bool) -> WorkerRun {
    let run = format!("t33-c03-{}-{downgrade}", fault.label());
    let thread = format!("{run}-thread");
    let identity = WorkerIdentity::new("t33-worker", "t33-boot", 1);
    let directory = Arc::new(CurrentWorkerDirectory(registered(&identity)));
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let dispatch = Arc::new(MemoryDispatchStore::new(clock.clock()));
    dispatch
        .enqueue(RunDispatch::new(activation(&run, &thread)))
        .await
        .unwrap();
    clock.set(0);
    let claimed = dispatch
        .claim(&identity.lease_owner(), 30_000, &Default::default())
        .await
        .unwrap()
        .expect("the Worker claims the Run");
    let control = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let service = Arc::new(ClaimedCommitHttpService::new(
        Arc::new(ClaimedCommitService::new(dispatch, control.clone())),
        directory,
        Arc::new(HeaderWorkerAuthenticator),
        awaken_time::system_clock(),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, claimed_commit_router(service))
            .await
            .unwrap()
    });

    let run_id = RunId(run.clone());
    let thread_id = ThreadId(thread.clone());
    let projection = Arc::new(RecoveryProjection::new());
    projection
        .install(
            &run_id,
            &thread_id,
            control
                .recovery_snapshot(&thread_id, &run_id)
                .await
                .unwrap(),
        )
        .unwrap();
    let remote = RemoteClaimedRunCommit::new(format!("http://{address}"), identity);
    let transport: Arc<dyn ClaimedRunCommit> = if downgrade {
        Arc::new(DowngradingTransport { inner: remote })
    } else {
        Arc::new(remote)
    };
    let worker_commit = Arc::new(
        ClaimedCommitCoordinator::new(transport, RunClaim::from(&claimed.lease))
            .with_recovery_projection(projection),
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let returned = Runtime::new()
        .with_llm(Arc::new(FaultyUpstream {
            fault,
            calls: calls.clone(),
        }))
        .with_retry_policy(RetryPolicy {
            max_retries: 0,
            ..Default::default()
        })
        .execute(
            activation(&run, &thread),
            RuntimeRunContext::new().with_commit(worker_commit),
        )
        .await
        .expect("every Worker step commit crosses the HTTP boundary");
    let control_state = CommittedThreadView::run_state(&*control, &run_id)
        .expect("the Control side holds the Run's committed state");
    let control_terminals = control
        .committed()
        .run_facts
        .iter()
        .filter(|fact| matches!(fact.state, RunState::Ended(_)))
        .count();
    WorkerRun {
        returned,
        control: control_state,
        control_terminals,
        calls: calls.load(Ordering::SeqCst),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn http_worker_commit_keeps_the_injected_upstream_fault_authority_on_the_control_side() {
    for fault in InjectedFault::ALL {
        let outcome = run_on_http_worker(fault, false).await;
        assert_eq!(
            outcome.control_terminals, 1,
            "{fault:?}: one Control-side terminal"
        );
        assert_eq!(
            outcome.control, outcome.returned,
            "{fault:?}: the HTTP boundary transports the Worker's exact terminal"
        );
        check(
            "http-worker",
            &fault.expected(),
            &classify(&outcome.control),
        )
        .unwrap_or_else(|violation| panic!("{fault:?}: {violation}"));
        assert_eq!(
            outcome.calls, 1,
            "{fault:?}: no hidden repeat of the upstream call"
        );
        if let RunState::Ended(EndCause::Error(failure)) = &outcome.control {
            assert!(
                failure.message().contains(UPSTREAM_DETAIL),
                "{fault:?}: the provider's account and hint reach Control: {}",
                failure.message()
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn http_worker_fault_checker_detects_a_transport_that_silently_reports_success() {
    for fault in InjectedFault::ALL {
        let outcome = run_on_http_worker(fault, true).await;
        // The Worker still believes its own classification; only the
        // Control-side projection chain was downgraded.
        assert!(
            check(
                "worker-local",
                &fault.expected(),
                &classify(&outcome.returned)
            )
            .is_ok(),
            "{fault:?}: the Worker-side terminal is the original one"
        );
        assert_eq!(
            outcome.control,
            RunState::Ended(EndCause::NaturalEnd),
            "{fault:?}: the mutant actually rewrote the Control-side truth"
        );
        let violation = check(
            "http-worker",
            &fault.expected(),
            &classify(&outcome.control),
        )
        .expect_err("the checker detects the silently downgraded projection");
        assert!(violation.starts_with("O1"), "{fault:?}: {violation}");
    }
}
