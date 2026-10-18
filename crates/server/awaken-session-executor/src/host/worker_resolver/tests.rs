// @formal evidence session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin
// @formal evidence session_resources.explicit_empty_manifest_revokes_runtime_projection
use super::test_support::{
    ToggleBindingSink, adoption_dependencies, adoption_dependencies_at, adoption_host,
    adoption_host_at, adoption_host_over, available_local_environment_binding, claim,
    complete_managed_test_host, deferred_environment, eager_environment, empty_frozen_projection,
    empty_frozen_projection_for_snapshot, frozen_projection_for_manifest, managed_test_host,
    prepare_deferred_session, test_activation, with_empty_session_resources,
};
use super::*;
use crate::host::dispatch_execution::adopt_bound_sandbox;
use crate::host::tests::ManagedSessionRootFixture;
use awaken_dispatch_contract as ingress;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::snapshot::{AgentId, ExecutableAgentSnapshotId};
use awaken_session_realization::peers::EnvironmentPeer as _;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[path = "tests/trusted_host_admission.rs"]
mod trusted_host_admission;

#[path = "tests/resources.rs"]
mod resources;

#[path = "tests/claimed_publication.rs"]
mod claimed_publication;

#[tokio::test]
async fn session_run_reservation_repair_skips_execution_realization() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Cause/effect graph: C0 the Session application supplies the current
    // immutable command fingerprint; C1 a complete self-affine Session Run
    // reservation expires; C2 the queue grants its dedicated recovery claim;
    // C3 no Session Environment has been realized. Effects: E1 the claim is
    // marked admission-only; E2 the canonical dispatch resolver selects the existing
    // Environment-free boundary Worker; E3 no Session Environment or Sandbox side effect is
    // created. Decision rule R1=C0+C1+C2+C3=>E1+E2+E3. Missing/unknown command
    // fingerprints remain owned by the queue conformance table.

    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let store = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
            .await
            .expect("dispatch store"),
    );
    let session_id = "reservation-repair";
    let run_id = "reservation-repair-run";
    let activation = test_activation(session_id, run_id);
    let command = awaken_session_execution_contract::AdmitSessionRun {
        session_id: session_id.into(),
        agent_id: activation.snapshot.root_agent_id.0.clone(),
        operation_id: "reservation-repair-operation".into(),
        run_id: activation.run_id.clone(),
        messages: activation.input.clone(),
        data_subject_id: None,
        traceparent: None,
        execution_requirements: Default::default(),
        replacement: Default::default(),
    };
    let request = ingress::RunDispatch::new(activation)
        .for_session(awaken_agent_contract::agent::thread::Id(session_id.into()))
        .with_session_command_fingerprint(command.command_fingerprint());
    assert_eq!(
        store
            .reserve_session_run(request, 1)
            .await
            .expect("R1 reserve"),
        ingress::SessionRunReservationOutcome::Reserved,
        "R1/C1"
    );
    // C1 is governed by the queue-owned clock. Wait past the relative TTL,
    // then sample the edge clock used by the existing execution-lease claim.
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let repair_now = awaken_time::system_clock().now().as_epoch_ms();
    clock.set(repair_now);
    let claimed = store
        .claim("repair-worker", 1_000, &Default::default())
        .await
        .expect("R1 recovery claim")
        .expect("R1 expired reservation is claimable");
    assert!(claimed.session_activity_admission_required, "R1/E1");

    let storage = tempfile::tempdir().expect("storage");
    let host = (adoption_dependencies_at(storage.path())
        .await
        .with_ephemeral_dispatch(store))
    .build()
    .expect("validated Host dependencies");
    crate::host::session_worker_resolver(&host.run_assembly, &host.dispatch)
        .expect("Host dispatch resolver")
        .worker_for_claimed(&claimed)
        .await
        .expect("R1/E2 environment-free worker");
    assert!(!has_environment(&host, session_id).await, "R1/E3");
}

#[tokio::test]
async fn ephemeral_parent_routes_a_coordinated_child_through_the_boundary_worker() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 the local Runtime authority is ephemeral; C2 the
    // parent Session therefore uses direct foreground delivery; C3 an ordinary
    // child Run is claimed from that same authority's dispatch queue with
    // parent affinity; C4 the co-located Session application has installed the
    // complete frozen parent projection before dispatch. Effects: E1 resolution reuses the parent commit,
    // Environment, and attempt context; E2 it constructs the existing child
    // boundary Worker without requiring a second per-Session durable ingress.
    // The durable-parent sibling paths are covered by cold-worker recovery.
    //
    // | Rule | Authority | Parent delivery | Claimed Thread | Effect |
    // |---|---|---|---|---|
    // | E1 | ephemeral | direct | child != parent, C4 | E1+E2 |
    use awaken_dispatch::WorkerResolver as _;

    let roots = ManagedSessionRootFixture::new().await;
    let host = roots
        .configure(adoption_dependencies().await)
        .build()
        .expect("E1 Host with the selected real Root sink");
    let managed = managed_test_host(host.clone());
    let parent = awaken_agent_contract::agent::thread::Id("ephemeral-parent".into());
    let child = awaken_agent_contract::agent::thread::Id("ephemeral-child".into());
    let run = awaken_agent_contract::agent::run::Id("ephemeral-child-run".into());
    let activation = test_activation(&child.0, &run.0);
    // C5 ephemeral foreground delivery does not remove Session authority:
    // the same real SQLite Root is assigned before physical parent opening.
    // Returned desired generation1, rather than the legacy declaration's0,
    // supplies the child's exact Resource envelope. No new child Root or
    // parent Runtime/plugin construction is authorized by boundary resolution.
    let mut initial = empty_frozen_projection_for_snapshot(
        host.run_assembly.local_workspace(),
        eager_environment(),
        &activation.snapshot,
    );
    initial.agent_publication = Some(activation.snapshot.clone());
    let projection = roots
        .install_projection(&managed, &parent.0, initial)
        .await
        .expect("E1 actual parent Root/current lease before I/O");
    let transition = projection
        .resource_transition(
            awaken_session_realization_contract::FrozenResourceTransitionUse::ApplyEffects,
        )
        .unwrap();
    let parent_ctx = host
        .run_assembly
        .ctx_for(&parent.0, None)
        .await
        .expect("E1 direct parent context");
    assert!(!parent_ctx.delivery.is_durable(), "E1 precondition");
    let store = host
        .run_assembly
        .authority
        .dispatch_binding()
        .map(|binding| binding.queue())
        .expect("ephemeral dispatch authority");
    store
        .enqueue(
            ingress::RunDispatch::new(activation)
                .for_session(parent.clone())
                .with_session_activity_epoch(7)
                .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
                    awaken_tenancy::ScopeId::from(projection.workspace_id.as_str()),
                ))
                .with_session_resources(
                    awaken_dispatch_execution::session_envelope::resource_envelope(
                        transition.desired(),
                    )
                    .unwrap(),
                ),
        )
        .await
        .expect("E1 child admission");
    let claimed = store
        .claim("ephemeral-worker", 1_000, &Default::default())
        .await
        .expect("E1 claim")
        .expect("E1 child work");
    let resolver = crate::host::session_worker_resolver(&host.run_assembly, &host.dispatch)
        .expect("Host dispatch resolver");

    resolver
        .worker_for_claimed(&claimed)
        .await
        .expect("E1/E2 child boundary resolution");
    assert!(
        host.run_assembly
            .installed_sessions
            .read_run(&parent.0, |state| state
                .context
                .as_ref()
                .is_some_and(|context| Arc::ptr_eq(context, &parent_ctx)))
            .expect("installed Sessions")
            .unwrap(),
        "E1/E2 boundary reuses the parent Runtime without rebuilding plugins"
    );
    assert!(
        host.run_assembly
            .installed_sessions
            .read(&child.0, |_| ())
            .expect("installed Sessions")
            .is_none(),
        "E1/E2 no child Session slot or Root is constructed"
    );
}

/// Durable lazy placement decision table. Brain resolution stays sandbox-free;
/// root persistence precedes the raw dispatch cache; both stale-cache rejection
/// and response loss retain the one hidden Candidate for the next owner.
///
/// | Rule | Root persist | Dispatch claim/cache | Effect |
/// | D1 | not attempted | current | sandbox-free Brain worker |
/// | D2 | committed | stale | reject exposure; retain Candidate |
/// | D3 | already committed | replacement current | reuse Candidate; publish exact identity |
/// | D4 | committed, response lost | current/cache empty | return error; retain Candidate |
/// | D5 | already committed | replacement current | one idempotent Store-read; publish |
///
/// D3/D5 perform no second provider create/adopt. Two sink entries in D4+D5
/// represent one attempted mutation plus its authoritative readback replay.
#[tokio::test]
async fn durable_deferred_sandbox_publication_decision_table() {
    let now = awaken_time::system_clock().now().as_epoch_ms();
    let storage = tempfile::tempdir().expect("isolated deferred Sandbox root");
    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let store = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
            .await
            .expect("dispatch store"),
    );
    let host = (adoption_dependencies_at(storage.path())
        .await
        .with_ephemeral_dispatch(store.clone()))
    .build()
    .expect("validated Host dependencies");
    let sink = Arc::new(ToggleBindingSink {
        fail: std::sync::atomic::AtomicBool::new(false),
        calls: AtomicUsize::new(0),
        binding: std::sync::Mutex::new(None),
    });
    let managed = prepare_deferred_session(host.clone(), "durable-lazy").await;
    managed
        .run_assembly
        .install_test_environment_binding_sink(sink.clone());
    let claimed = claim(&store, "durable-lazy", "run-lazy", "worker-a", now, &clock).await;
    let resolver = crate::host::session_worker_resolver(&host.run_assembly, &host.dispatch)
        .expect("Host dispatch resolver");
    resolver
        .worker_for_claimed(&claimed)
        .await
        .expect("D1 resolves Brain worker without Sandbox");
    assert!(!has_environment(&host, "durable-lazy").await, "D1");

    clock.set(now + 2_000);
    let replacement = store
        .claim("worker-b", 1_000, &Default::default())
        .await
        .expect("replacement claim")
        .expect("expired claim is recoverable");
    let deferred = host
        .run_assembly
        .installed_sessions
        .read_run("durable-lazy", |run| run.deferred_executor.clone())
        .expect("installed Sessions")
        .flatten()
        .expect("deferred executor");
    let error = deferred
        .invoke(&awaken_tool_contract::tool::ToolCall {
            call_id: "stale-read".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({"path": "missing"}),
        })
        .await
        .expect_err("D3 stale claim is fenced");
    assert!(
        error.to_string().contains("replacement claim"),
        "D3: {error}"
    );
    assert!(!has_environment(&host, "durable-lazy").await, "D2");

    resolver
        .worker_for_claimed(&replacement)
        .await
        .expect("D3 replacement repairs the root-first Candidate");
    assert!(
        has_environment(&host, "durable-lazy").await,
        "D3 exact Candidate published"
    );
    assert_eq!(sink.calls.load(Ordering::SeqCst), 2, "D2+D3");

    let loss_store = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
            .await
            .expect("loss dispatch store"),
    );
    let loss_host = (adoption_dependencies_at(storage.path().join("response-loss"))
        .await
        .with_ephemeral_dispatch(loss_store.clone()))
    .build()
    .expect("validated Host dependencies");
    let loss_sink = Arc::new(ToggleBindingSink {
        fail: std::sync::atomic::AtomicBool::new(false),
        calls: AtomicUsize::new(0),
        binding: std::sync::Mutex::new(None),
    });
    let loss_managed = prepare_deferred_session(loss_host.clone(), "durable-loss").await;
    loss_managed
        .run_assembly
        .install_test_environment_binding_sink(loss_sink.clone());
    let loss_now = awaken_time::system_clock().now().as_epoch_ms();
    let loss_claim = claim(
        &loss_store,
        "durable-loss",
        "run-durable-loss",
        "worker-loss-a",
        loss_now,
        &clock,
    )
    .await;
    let loss_resolver =
        crate::host::session_worker_resolver(&loss_host.run_assembly, &loss_host.dispatch)
            .expect("Host dispatch resolver");
    loss_resolver
        .worker_for_claimed(&loss_claim)
        .await
        .expect("D4 sandbox-free loss worker");
    loss_sink.fail.store(true, Ordering::SeqCst);
    let loss_deferred = loss_host
        .run_assembly
        .installed_sessions
        .read_run("durable-loss", |run| run.deferred_executor.clone())
        .expect("installed Sessions")
        .flatten()
        .expect("D4 deferred executor");
    let error = loss_deferred
        .invoke(&awaken_tool_contract::tool::ToolCall {
            call_id: "crash-gap-read".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({"path": "missing"}),
        })
        .await
        .expect_err("D4 Session binding response loss");
    assert!(
        error
            .to_string()
            .contains("injected Session binding failure"),
        "D4: {error}"
    );
    assert!(
        loss_host
            .run_assembly
            .resident_session_environment("durable-loss")
            .await
            .is_none(),
        "D4 hidden Candidate is not exposed"
    );
    clock.set(loss_now + 2_000);
    let loss_retried = loss_store
        .claim("worker-loss-b", 1_000, &Default::default())
        .await
        .expect("D5 retry claim")
        .expect("D5 response-loss Run is recoverable");
    assert!(loss_retried.sandbox.is_none(), "D4 cache remains empty");
    loss_sink.fail.store(false, Ordering::SeqCst);
    loss_resolver
        .worker_for_claimed(&loss_retried)
        .await
        .expect("D5 exact Candidate readback repair");
    assert_eq!(
        loss_sink.calls.load(Ordering::SeqCst),
        2,
        "D4 attempted the root mutation once; D5 uses one idempotent Store-read replay"
    );
    assert!(
        loss_host
            .run_assembly
            .resident_session_environment("durable-loss")
            .await
            .is_some(),
        "D5 exact Candidate published"
    );
}

struct RecordingSessionControl {
    resume_calls: Arc<AtomicUsize>,
    continuations:
        Arc<std::sync::Mutex<Vec<Option<crate::session_resource::SessionRealizationLease>>>>,
    phases: Arc<std::sync::Mutex<Vec<&'static str>>>,
    projection:
        Arc<std::sync::Mutex<Option<awaken_session_realization_contract::FrozenSessionProjection>>>,
    mcp_stage: Option<crate::session_mcp::StageMcpAttachment>,
    fail_begin: Arc<AtomicBool>,
}

#[derive(Default)]
struct RecordingMcpRealizer {
    calls: std::sync::Mutex<Vec<&'static str>>,
    fail_stage: std::sync::atomic::AtomicBool,
    required_physical_environment:
        std::sync::OnceLock<(std::sync::Weak<ExecutorComponents>, String)>,
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::McpAttachmentRealizer for RecordingMcpRealizer {
    async fn stage_mcp_attachment(
        &self,
        request: crate::session_mcp::StageMcpAttachment,
    ) -> Result<crate::session_mcp::McpRealizationReceipt, awaken_session_types::RunError> {
        self.calls.lock().unwrap().push("stage");
        if let Some((host, thread)) = self.required_physical_environment.get() {
            let physical_only = host.upgrade().is_some_and(|host| {
                host.run_assembly
                    .installed_sessions
                    .read(thread, |slot| {
                        slot.realization.environment().owner.is_resident()
                            && slot.run.context.is_none()
                    })
                    .expect("installed Sessions")
                    .unwrap_or(false)
            });
            if !physical_only {
                return Err(awaken_session_types::RunError::classified(
                    "test_session_physical_environment_missing",
                    "claimed MCP stage requires the physical Environment but no parent Runtime",
                ));
            }
        }
        if self.fail_stage.load(Ordering::SeqCst) {
            return Err(awaken_session_types::RunError::classified(
                "test_mcp_stage_failed",
                "test MCP stage failed",
            ));
        }
        Ok(crate::session_mcp::McpRealizationReceipt {
            receipt_fingerprint: request.fingerprint(),
            generation: request.generation,
            realization_id: request.realization_id,
            selected_plaintext_holder: request.selected_plaintext_holder,
            actual_realization_kind: None,
        })
    }

    async fn publish_mcp_generation(
        &self,
        _generation: crate::session_mcp::McpGenerationRef,
    ) -> Result<(), awaken_session_types::RunError> {
        self.calls.lock().unwrap().push("publish");
        Ok(())
    }

    async fn drain_mcp_generation(
        &self,
        _generation: crate::session_mcp::McpGenerationRef,
    ) -> Result<(), awaken_session_types::RunError> {
        self.calls.lock().unwrap().push("drain");
        Ok(())
    }
}

#[tokio::test]
async fn cold_claimed_stdio_mcp_opens_the_exact_agent_snapshot_before_staging() {
    /* Claimed sandbox-stdio recovery cause/effect decision table. Causes:
     * C1 a cold Worker has no process-local Agent publication; C2 the durable
     * Run carries its exact immutable snapshot and non-default backend
     * projection; C3 its Session runtime envelope contains a sandbox-stdio
     * MCP stage; C4 the Session has no resident Runtime/Environment; C5 its
     * complete baseline/model and empty Resource transition are installed but
     * retain no Agent publication. Effects:
     * E1 open only the physical Session Environment from the claimed snapshot
     * before MCP staging; E2 stage and publish the exact MCP generation; E3
     * build the root Runtime once after publication, never as stage substrate;
     * E4 never consult or synthesize a second publication source. Rule S1:
     * C1+C2+C3+C4=>E1+E2+E3+E4. The adjacent malformed-envelope and stale-
     * claim rules retain fail-closed coverage for invalid authority. */
    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let store = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
            .await
            .expect("dispatch store"),
    );
    let thread = "cold-stdio-recovery";
    let realizer = Arc::new(RecordingMcpRealizer::default());
    // S1 receiver obtains real Root/lease/first CAS without storing an Agent
    // publication. The claim remains the exact backend/stdio snapshot source;
    // the Queue receives the actual prepared Resource generation before claim.
    let root_fixture = ManagedSessionRootFixture::new().await;
    let host = root_fixture
        .configure(
            adoption_dependencies()
                .await
                .with_ephemeral_dispatch(store.clone())
                .with_session_execution(
                    crate::SessionExecutionDependencies::default()
                        .with_mcp_attachment_realizer(realizer.clone()),
                ),
        )
        .build()
        .unwrap();
    realizer
        .required_physical_environment
        .set((Arc::downgrade(&host), thread.into()))
        .unwrap();
    let managed = complete_managed_test_host(host.clone());
    let activation = test_activation(thread, "run-cold-stdio-recovery");
    let projection = root_fixture
        .install_projection(
            &managed,
            thread,
            empty_frozen_projection_for_snapshot(
                host.run_assembly.local_workspace(),
                deferred_environment(),
                &activation.snapshot,
            ),
        )
        .await
        .expect("S1 install complete cold Runtime coordinates");
    let transition = projection
        .resource_transition(
            awaken_session_realization_contract::FrozenResourceTransitionUse::ApplyEffects,
        )
        .expect("S1 exact Store-read initial Resource transition");
    let stage = crate::session_mcp::StageMcpAttachment {
        workspace_id: host.run_assembly.local_workspace().into(),
        generation: crate::test_mcp::mcp_fence(thread, "browser", 1, "worker-a", 1, u64::MAX),
        realization_id: "realize-browser-1".into(),
        stage_idempotency_key: "stage-browser-1".into(),
        name: "browser".into(),
        target: awaken_agent_contract::McpTarget::sandbox_stdio(
            "playwright-mcp",
            vec!["--headless".into()],
        )
        .expect("sandbox stdio target"),
        prompts_as_skills: false,
        credential: None,
        selected_plaintext_holder: None,
    };
    let runtime = ingress::SessionRuntimeEnvelope::new(
        awaken_session_configuration_contract::SessionRuntimeProjection {
            environment: deferred_environment(),
            tools: Some(Default::default()),
            mcp_stages: Some(vec![stage]),
        }
        .encode()
        .expect("runtime envelope"),
    );
    store
        .enqueue(
            ingress::RunDispatch::new(activation)
                .with_session_runtime(runtime)
                .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
                    awaken_tenancy::ScopeId::from(projection.workspace_id.as_str()),
                ))
                .with_session_resources(
                    awaken_dispatch_execution::session_envelope::resource_envelope(
                        transition.desired(),
                    )
                    .expect("S1 carry the Root's prepared Resource generation"),
                ),
        )
        .await
        .expect("enqueue");
    let claimed = store
        .claim("worker-a", 30_000, &Default::default())
        .await
        .expect("claim")
        .expect("claimed Run");

    crate::host::session_worker_resolver(&host.run_assembly, &host.dispatch)
        .expect("Host dispatch resolver")
        .worker_for_claimed(&claimed)
        .await
        .expect("S1 cold stdio recovery");

    assert_eq!(
        realizer.calls.lock().unwrap().as_slice(),
        ["stage", "publish"]
    );
    assert!(
        host.run_assembly
            .installed_sessions
            .read_run(thread, |run| run.context.is_some())
            .expect("installed Sessions")
            .unwrap_or(false),
        "S1/E3"
    );
}

#[async_trait::async_trait]
impl awaken_session_realization_contract::ClaimedSessionRealizationControl
    for RecordingSessionControl
{
    async fn resume_frozen(
        &self,
        claim: &ingress::RunClaim,
        _session_id: &str,
        _session_work_authority: Option<
            &awaken_session_application_contract::SessionWorkAuthorityReceipt,
        >,
        continuation: Option<&crate::session_resource::SessionRealizationLease>,
    ) -> Result<
        Option<awaken_session_realization_contract::SessionRealizationDirective>,
        awaken_session_realization_contract::ClaimedSessionControlError,
    > {
        self.resume_calls.fetch_add(1, Ordering::SeqCst);
        self.continuations
            .lock()
            .unwrap()
            .push(continuation.cloned());
        Ok(self.projection.lock().unwrap().clone().map(|projection| {
            awaken_session_realization_contract::SessionRealizationDirective {
                projection,
                authority: awaken_session_realization::test_support::test_lease_receipt(
                    continuation.cloned().unwrap_or_else(|| {
                        crate::session_resource::SessionRealizationLease {
                            owner: claim.owner.clone(),
                            runtime_incarnation: claim.owner.clone(),
                            epoch: claim.epoch,
                            expires_at_unix_ms: u64::MAX,
                        }
                    }),
                ),
                action: awaken_session_realization_contract::SessionRealizationAction::Complete,
            }
        }))
    }
}

#[async_trait::async_trait]
impl awaken_session_realization_contract::SessionRealizationControl for RecordingSessionControl {
    async fn begin_session_realization(
        &self,
        command: awaken_session_realization_contract::BeginSessionRealization,
    ) -> Result<
        awaken_session_realization_contract::SessionRealizationDirective,
        awaken_session_realization_contract::SessionRealizationControlFailure,
    > {
        self.phases.lock().unwrap().push("begin");
        if self.fail_begin.load(Ordering::SeqCst) {
            return Err(
                awaken_session_realization_contract::SessionRealizationControlFailure::NotReady,
            );
        }
        let projection = self.projection.lock().unwrap().clone().ok_or(
            awaken_session_realization_contract::SessionRealizationControlFailure::NotReady,
        )?;
        let mut stage = self.mcp_stage.clone().ok_or(
            awaken_session_realization_contract::SessionRealizationControlFailure::NotReady,
        )?;
        stage.generation = stage
            .generation
            .with_persisted(|w| {
                w.lease_expires_at_unix_ms = command.target.lease_expires_at_unix_ms
            })
            .unwrap();
        stage.stage_idempotency_key = format!("renew:{}", command.target.lease_expires_at_unix_ms);
        Ok(
            awaken_session_realization_contract::SessionRealizationDirective {
                projection,
                authority: awaken_session_realization::test_support::test_lease_receipt(
                    crate::session_resource::SessionRealizationLease {
                        owner: command.target.owner,
                        runtime_incarnation: command.target.runtime_incarnation,
                        epoch: 1,
                        expires_at_unix_ms: command.target.lease_expires_at_unix_ms,
                    },
                ),
                action: awaken_session_realization_contract::SessionRealizationAction::Stage {
                    prepare_session: false,
                    mcp_stages: vec![stage],
                },
            },
        )
    }

    async fn activate_session_realization(
        &self,
        command: awaken_session_realization_contract::ActivateSessionRealization,
    ) -> Result<
        awaken_session_realization_contract::SessionRealizationDirective,
        awaken_session_realization_contract::SessionRealizationControlFailure,
    > {
        self.phases.lock().unwrap().push("activate");
        Ok(
            awaken_session_realization_contract::SessionRealizationDirective {
                projection: self
                    .projection
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("frozen Session projection"),
                authority: awaken_session_realization::test_support::test_lease_receipt(
                    command.lease,
                ),
                action: awaken_session_realization_contract::SessionRealizationAction::Publish {
                    publish: command
                        .mcp_receipts
                        .into_iter()
                        .map(|receipt| receipt.generation)
                        .collect(),
                    drain: Vec::new(),
                },
            },
        )
    }

    async fn acknowledge_session_realization(
        &self,
        command: awaken_session_realization_contract::AcknowledgeSessionRealization,
    ) -> Result<
        awaken_session_realization_contract::SessionRealizationDirective,
        awaken_session_realization_contract::SessionRealizationControlFailure,
    > {
        self.phases.lock().unwrap().push("acknowledge");
        Ok(
            awaken_session_realization_contract::SessionRealizationDirective {
                projection: self
                    .projection
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("frozen Session projection"),
                authority: awaken_session_realization::test_support::test_lease_receipt(
                    command.lease,
                ),
                action: awaken_session_realization_contract::SessionRealizationAction::Complete,
            },
        )
    }

    async fn fail_session_realization(
        &self,
        _command: awaken_session_realization_contract::FailSessionRealization,
    ) -> Result<(), awaken_session_realization_contract::SessionRealizationControlFailure> {
        self.phases.lock().unwrap().push("fail");
        Ok(())
    }

    async fn renew_session_realization(
        &self,
        command: awaken_session_realization_contract::RenewSessionRealization,
    ) -> Result<
        awaken_session_realization_contract::SessionRealizationLeaseReceipt,
        awaken_session_realization_contract::SessionRealizationControlFailure,
    > {
        self.phases.lock().unwrap().push("renew");
        let mut lease = command.asserted_lease;
        if command.requested_expires_at_unix_ms < lease.expires_at_unix_ms {
            return Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership);
        }
        lease.expires_at_unix_ms = command.requested_expires_at_unix_ms;
        Ok(awaken_session_realization::test_support::test_lease_receipt(lease))
    }
}

fn ordinary_frozen_projection() -> awaken_session_realization_contract::FrozenSessionProjection {
    let holder = awaken_runtime_contract::PlaintextHolder::new(
        awaken_runtime_contract::PlaintextBoundary::Worker,
        "test.worker",
    );
    empty_frozen_projection(
        "workspace",
        awaken_session_configuration_contract::EnvironmentSnapshot {
            environment_id: "env".into(),
            revision: awaken_environment_contract::EnvironmentRevision(1),
            self_hosted: true,
            config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
                "env-fingerprint".into(),
            ),
            sandbox: Default::default(),
            sandbox_provisioning: Default::default(),
            idle_retention: Default::default(),
            packages: Default::default(),
            prepared_image: None,
            network: awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
            credential_realization: awaken_credential_contract::CredentialRealizationProfile {
                inference_holder: holder.clone(),
                mcp_holder: holder.clone(),
                resource_holder: holder,
            },
        },
    )
}

/// Cause/effect graph: C1 an authenticated Run claim is current; C2 its
/// Session is already frozen; C3 the Worker has the claimed Session-control
/// client; C4 the Worker has the canonical Environment-effect binding sink.
/// C1+C2+C3+C4 cause E1 resume the canonical realization, E2 install the exact
/// baseline and lease, and E3 durably create the Worker environment.
/// C3 absent is the co-located local-pool row, covered by the cold legacy test.
///
/// | Rule | Claim | Frozen | Control+binding | Effect |
/// |---|---|---|---|---|
/// | O1 | live | yes | present | resume and realize frozen projection |
/// | O2 | stale | any | present | reject before realization |
/// | O3 | live | no | present | fail closed |
/// | O4 | live | local pre-realized | absent | local-pool replay |
/// | O5 | live Run without Session pointer | n/a | present | bypass Session control |
#[tokio::test]
async fn claimed_worker_uses_only_frozen_session_realization() {
    let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let dispatch = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
            .await
            .expect("in-memory dispatch"),
    );
    let resume_calls = Arc::new(AtomicUsize::new(0));
    let continuations = Arc::new(std::sync::Mutex::new(Vec::new()));
    let projection = Arc::new(std::sync::Mutex::new(Some(ordinary_frozen_projection())));
    let host = (adoption_dependencies()
        .await
        .with_ephemeral_dispatch(dispatch.clone())
        .with_session_realization_control(Arc::new(RecordingSessionControl {
            resume_calls: resume_calls.clone(),
            continuations: continuations.clone(),
            phases: Arc::new(std::sync::Mutex::new(Vec::new())),
            projection: projection.clone(),
            mcp_stage: None,
            fail_begin: Arc::new(AtomicBool::new(false)),
        })))
    .build()
    .expect("validated Host dependencies");
    let managed = managed_test_host(host.clone());
    managed
        .run_assembly
        .install_test_environment_binding_sink(Arc::new(ToggleBindingSink {
            fail: AtomicBool::new(false),
            calls: AtomicUsize::new(0),
            binding: std::sync::Mutex::new(None),
        }));
    dispatch
        .enqueue(with_empty_session_resources(
            ingress::RunDispatch::new(test_activation("ordinary-remote", "run-ordinary-remote"))
                .for_session(awaken_agent_contract::agent::thread::Id(
                    "ordinary-remote".into(),
                ))
                .with_session_activity_epoch(1),
            "workspace",
        ))
        .await
        .expect("O1 enqueue");
    let claimed = dispatch
        .claim("worker-a", 30_000, &Default::default())
        .await
        .expect("O1 claim")
        .expect("O1 claimed Run");
    // CR1 new Run + predecessor live lease + two failed Control responses =>
    // neither request may relabel that lease as its continuation. CR2 successful
    // frozen publication binds claim and lease together; CR3 same-Run retry
    // then sends precisely that installed lease. This sequence catches the
    // early-claim-publication bug that the static 20-case selector cannot.
    let predecessor = ingress::RunClaim {
        run_id: awaken_agent_contract::agent::run::Id("previous-run".into()),
        owner: claimed.lease.owner().to_owned(),
        epoch: 0,
    };
    host.run_assembly
        .installed_sessions
        .install_with("ordinary-remote", |slot| {
            slot.run.dispatch_claim = Some(predecessor.clone());
            slot.realization.execution_mut().authority =
                crate::realization_state::SessionRealizationAuthority::Live(
                    awaken_session_realization::test_support::test_observed_lease(
                        crate::session_resource::SessionRealizationLease {
                            owner: claimed.lease.owner().to_owned(),
                            runtime_incarnation: claimed.lease.owner().to_owned(),
                            epoch: 0,
                            expires_at_unix_ms: u64::MAX,
                        },
                    ),
                );
        })
        .expect("installed Sessions");
    let frozen = projection.lock().unwrap().take();
    for _ in 0..2 {
        assert!(
            crate::host::session_worker_resolver(&host.run_assembly, &host.dispatch)
                .expect("Host dispatch resolver")
                .worker_for_claimed(&claimed)
                .await
                .is_err(),
            "CR1 unavailable projection"
        );
        assert_eq!(
            host.run_assembly
                .installed_sessions
                .read_run("ordinary-remote", |run| run.dispatch_claim.clone())
                .expect("installed Sessions")
                .flatten(),
            Some(predecessor.clone()),
            "CR1 failed request cannot publish its claim"
        );
    }
    assert_eq!(*continuations.lock().unwrap(), vec![None, None], "CR1");
    *projection.lock().unwrap() = frozen;
    crate::host::session_worker_resolver(&host.run_assembly, &host.dispatch)
        .expect("Host dispatch resolver")
        .worker_for_claimed(&claimed)
        .await
        .expect("O1 canonical realization");

    assert_eq!(resume_calls.load(Ordering::SeqCst), 3, "O1/E1+CR1");
    let installed = host
        .run_assembly
        .installed_sessions
        .read("ordinary-remote", |slot| {
            assert_eq!(
                slot.run.dispatch_claim,
                Some(ingress::RunClaim::from(&claimed.lease)),
                "CR2"
            );
            slot.realization.execution().authority.lease().cloned()
        })
        .expect("installed Sessions")
        .flatten()
        .expect("CR2 installed lease");
    crate::host::session_worker_resolver(&host.run_assembly, &host.dispatch)
        .expect("Host dispatch resolver")
        .worker_for_claimed(&claimed)
        .await
        .expect("CR3 same-Run retry");
    assert_eq!(
        *continuations.lock().unwrap(),
        vec![None, None, None, Some(installed)],
        "CR1-CR3"
    );
    assert!(has_environment(&host, "ordinary-remote").await, "O1/E3");
    assert!(
        host.run_assembly
            .installed_sessions
            .read("ordinary-remote", |slot| {
                slot.inputs.baseline.is_some()
                    && slot
                        .realization
                        .execution()
                        .authority
                        .lease()
                        .is_some_and(|lease| {
                            lease.owner == claimed.lease.owner()
                                && lease.epoch == claimed.lease.epoch()
                        })
            })
            .expect("installed Sessions")
            .unwrap_or(false),
        "O1/E1-E2"
    );

    let ordinary_activation = test_activation("ordinary-run", "run-without-session");
    awaken_session_execution_contract::RealizationRuntime::install_session_projection(
        &*managed.run_assembly,
        "ordinary-run",
        empty_frozen_projection_for_snapshot(
            host.run_assembly.local_workspace(),
            deferred_environment(),
            &ordinary_activation.snapshot,
        ),
        awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("O5 install complete ordinary Runtime projection");
    dispatch
        .enqueue(with_empty_session_resources(
            ingress::RunDispatch::new(ordinary_activation),
            host.run_assembly.local_workspace(),
        ))
        .await
        .expect("O5 enqueue");
    let claimed = dispatch
        .claim("worker-a", 30_000, &Default::default())
        .await
        .expect("O5 claim")
        .expect("O5 claimed Run");
    crate::host::session_worker_resolver(&host.run_assembly, &host.dispatch)
        .expect("Host dispatch resolver")
        .worker_for_claimed(&claimed)
        .await
        .expect("O5 ordinary Run bypasses Session realization");
    assert_eq!(resume_calls.load(Ordering::SeqCst), 4, "O5");
}

#[tokio::test]
async fn claimed_resource_conflict_is_terminal_before_realization() {
    // Cause/effect graph: C1 the Run claim is live; C2 its immutable Resource
    // envelope differs from Control's desired Workspace/revision/bindings.
    // C1+C2 => E1 typed terminal resolution, E2 no Worker realization effects.
    // Retrying cannot rewrite the carried envelope: do not silently install new
    // resources or relinquish forever. Decision table R1/R2/R3 isolates each
    // differing coordinate. Exact equality is covered by the projection table;
    // transient Control failures retain the contract-disposition table's retry.
    for rule in ["workspace", "revision", "bindings"] {
        let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
        let dispatch = Arc::new(
            awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
                .await
                .unwrap(),
        );
        let mut projection = ordinary_frozen_projection();
        match rule {
            "workspace" => {
                projection.workspace_id = "another-workspace".into();
                if let Some(previous) = projection.previous_resource_manifest.as_mut() {
                    previous.workspace_id = projection.workspace_id.clone();
                }
            }
            "revision" => projection.resource_revision += 1,
            "bindings" => {
                projection.resources = crate::session_resource::ResolvedSessionResources::try_new(
                    Vec::new(),
                    vec![crate::session_resource::ResolvedSkillBinding {
                        kind: awaken_agent_contract::AgentSkillKind::Custom,
                        skill_id: "replacement-skill".into(),
                        version: 1,
                        bundle_sha256: "sha256-replacement".into(),
                    }],
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        let resume_calls = Arc::new(AtomicUsize::new(0));
        let phases = Arc::new(std::sync::Mutex::new(Vec::new()));
        let host = (adoption_dependencies()
            .await
            .with_ephemeral_dispatch(dispatch.clone())
            .with_session_realization_control(Arc::new(RecordingSessionControl {
                resume_calls: resume_calls.clone(),
                continuations: Arc::new(std::sync::Mutex::new(Vec::new())),
                phases: phases.clone(),
                projection: Arc::new(std::sync::Mutex::new(Some(projection))),
                mcp_stage: None,
                fail_begin: Arc::new(AtomicBool::new(false)),
            })))
        .build()
        .expect("validated Host dependencies");
        dispatch
            .enqueue(with_empty_session_resources(
                ingress::RunDispatch::new(test_activation("conflict", "conflict-run"))
                    .for_session(awaken_agent_contract::agent::thread::Id("conflict".into()))
                    .with_session_activity_epoch(1),
                "workspace",
            ))
            .await
            .unwrap();
        let claimed = dispatch
            .claim("worker-a", 30_000, &Default::default())
            .await
            .unwrap()
            .unwrap();
        let result = crate::host::session_worker_resolver(&host.run_assembly, &host.dispatch)
            .expect("Host dispatch resolver")
            .worker_for_claimed(&claimed)
            .await;
        let error = match result {
            Ok(_) => panic!("{rule}: incompatible resources must not execute"),
            Err(error) => error,
        };
        assert!(error.is_terminal_resolution(), "{rule}/E1: {error}");
        assert!(
            error
                .to_string()
                .contains("conflicts with the claimed resource snapshot")
        );
        assert_eq!(
            resume_calls.load(Ordering::SeqCst),
            1,
            "{rule}: reached Control"
        );
        assert!(
            phases.lock().unwrap().is_empty(),
            "{rule}/E2: no realization phase"
        );
        assert!(!has_environment(&host, "conflict").await, "{rule}/E2");
    }
}

#[tokio::test]
async fn resource_manifest_must_match_the_durable_execution_scope() {
    // Test design. Causes: C1 a claim's execution scope is workspace-b while
    // its frozen Resource manifest names workspace-a. Effects: E1 resolution
    // fails before sandbox creation; E2 no mismatched manifest is installed.
    // Constraint/Invariant: durable execution scope owns Resource tenancy.
    // Decision rule: exercise the unequal-scope partition and require E1/E2.
    let storage = tempfile::tempdir().expect("storage");
    let host = adoption_host_at(storage.path()).await;
    let run = RunId("run-scope-mismatch".to_string());
    let request = ingress::RunDispatch::new(test_activation("thread-scope-mismatch", &run.0))
        .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
            awaken_tenancy::ScopeId::from("workspace-b"),
        ))
        .with_session_resources(ingress::SessionResourceEnvelope::new(
            "workspace-a",
            r#"{"inputs":[],"skills":[]}"#,
        ));
    let claimed = ingress::Claimed {
        request,
        lease: ingress::DispatchLease::from_persisted(run, "worker-a".to_string(), 100, 1).unwrap(),
        session_work_authority: None,
        credential_bindings: Vec::new(),
        credential_deliveries: Vec::new(),
        cancellation_requested: false,
        pending: Vec::new(),
        recovered: false,
        session_activity_admission_required: false,
        sandbox: None,
        assignment: None,
    };
    let resolver = crate::host::session_worker_resolver(&host.run_assembly, &host.dispatch)
        .expect("Host dispatch resolver");

    let error = match resolver.worker_for_claimed(&claimed).await {
        Ok(_) => panic!("scope mismatch must fail closed"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("outside its execution scope"));
    assert!(
        !has_environment(&host, "thread-scope-mismatch").await,
        "scope rejection happens before sandbox creation"
    );
}

#[tokio::test]
async fn malformed_resource_manifest_is_rejected_before_sandbox_creation() {
    // Test design. Causes: C1 a scope-matched claim carries malformed manifest
    // JSON. Effects: E1 resolution rejects it; E2 sandbox creation and Resource
    // projection remain untouched. Constraint/Invariant: manifest validation
    // precedes every live effect. Decision rule: exercise malformed input and
    // require fail-closed zero creation.
    let storage = tempfile::tempdir().expect("storage");
    let host = adoption_host_at(storage.path()).await;
    let run = RunId("run-malformed-resources".to_string());
    let request = ingress::RunDispatch::new(test_activation("thread-malformed-resources", &run.0))
        .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
            awaken_tenancy::ScopeId::from("workspace-a"),
        ))
        .with_session_resources(ingress::SessionResourceEnvelope::new(
            "workspace-a",
            "not-json",
        ));
    let claimed = ingress::Claimed {
        request,
        lease: ingress::DispatchLease::from_persisted(run, "worker-a".to_string(), 100, 1).unwrap(),
        session_work_authority: None,
        credential_bindings: Vec::new(),
        credential_deliveries: Vec::new(),
        cancellation_requested: false,
        pending: Vec::new(),
        recovered: false,
        session_activity_admission_required: false,
        sandbox: None,
        assignment: None,
    };
    let resolver = crate::host::session_worker_resolver(&host.run_assembly, &host.dispatch)
        .expect("Host dispatch resolver");

    let error = match resolver.worker_for_claimed(&claimed).await {
        Ok(_) => panic!("malformed manifest must fail closed"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("invalid Session resource manifest")
    );
    assert!(!has_environment(&host, "thread-malformed-resources").await);
}

#[tokio::test]
async fn desired_only_dispatch_reuses_the_exact_projected_transition() {
    // Producer/consumer cause/effect table: C1 the complete-projection producer
    // has installed an exact rev1→rev1 or rev1→rev2 transition; C2 active is
    // absent or equals the transition's previous generation; C3 the carried
    // desired-only manifest is exact or differs only by revision. Effects: E1
    // the consumer reuses the existing transition without publishing desired
    // as active; E2 a mismatch rejects before changing any correlated Resource
    // projection.
    //
    // | Rule | C1 | C2 | C3 | Effect |
    // | R1 | rev1→rev1 | absent | exact rev1 | E1, active absent |
    // | R2 | rev1→rev2 | active rev1 | exact rev2 | E1, active rev1 |
    // | R3 | rev1→rev1 | absent | rev2 | E2 |
    let host = adoption_host().await;
    let managed = managed_test_host(host.clone());
    let revision_one = crate::session_resource::SessionResourceManifest::at_revision(
        "workspace-a",
        1,
        crate::session_resource::ResolvedSessionResources::default(),
    );
    let revision_two = crate::session_resource::SessionResourceManifest::at_revision(
        "workspace-a",
        2,
        crate::session_resource::ResolvedSessionResources::default(),
    );

    for thread in ["exact-cold-transition", "mismatched-cold-transition"] {
        let mut projection = frozen_projection_for_manifest(deferred_environment(), &revision_one);
        projection.previous_resource_manifest = Some(revision_one.clone());
        awaken_session_execution_contract::RealizationRuntime::install_session_projection(
            &*managed.run_assembly,
            thread,
            projection,
            awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch,
        )
        .await
        .unwrap_or_else(|error| panic!("{thread}: complete projection producer: {error}"));
        assert!(
            active_manifest(&host, thread).is_none(),
            "{thread}: C2 active truth remains absent"
        );
    }
    let active_thread = "active-previous-transition";
    host.run_assembly
        .installed_sessions
        .install(active_thread)
        .expect("installed Sessions");
    host.realization_executor
        .staging()
        .register_thread_resource_manifest(active_thread, revision_one.clone())
        .unwrap();
    let mut active_projection =
        frozen_projection_for_manifest(deferred_environment(), &revision_two);
    active_projection.previous_resource_manifest = Some(revision_one.clone());
    awaken_session_execution_contract::RealizationRuntime::install_session_projection(
        &*managed.run_assembly,
        active_thread,
        active_projection,
        awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("R2 complete projection producer");

    let exact_before = host
        .run_assembly
        .installed_sessions
        .realizations()
        .read_realization("exact-cold-transition", |r| r.staging().transition.clone())
        .unwrap()
        .flatten()
        .expect("R1 exact transition projection");
    assert_eq!(exact_before.previous(), &revision_one, "R1/C1 previous");
    assert_eq!(exact_before.desired(), &revision_one, "R1/C1 desired");
    host.realization_executor
        .install_dispatched_resources("exact-cold-transition", &revision_one, None)
        .await
        .expect("R1/E1 exact desired-only consumer");
    assert_eq!(
        host.run_assembly
            .installed_sessions
            .realizations()
            .read_realization("exact-cold-transition", |r| r.staging().transition.clone())
            .unwrap()
            .flatten(),
        Some(exact_before),
        "R1/E1 retains the exact producer transition"
    );
    assert!(
        active_manifest(&host, "exact-cold-transition").is_none(),
        "R1/E1 staging does not publish active truth"
    );

    let active_transition = host
        .run_assembly
        .installed_sessions
        .realizations()
        .read_realization(active_thread, |r| r.staging().transition.clone())
        .unwrap()
        .flatten()
        .expect("R2 exact transition projection");
    assert_eq!(
        active_transition.previous(),
        &revision_one,
        "R2/C1 previous"
    );
    assert_eq!(active_transition.desired(), &revision_two, "R2/C1 desired");
    host.realization_executor
        .install_dispatched_resources(active_thread, &revision_two, None)
        .await
        .expect("R2/E1 exact desired consumer with active previous");
    assert_eq!(
        host.run_assembly
            .installed_sessions
            .realizations()
            .read_realization(active_thread, |r| r.staging().transition.clone())
            .unwrap()
            .flatten(),
        Some(active_transition),
        "R2/E1 retains the exact producer transition"
    );
    assert_eq!(
        active_manifest(&host, active_thread),
        Some(revision_one.clone()),
        "R2/E1 staging does not publish desired as active"
    );

    let mismatched_thread = "mismatched-cold-transition";
    let before_mismatch = host
        .run_assembly
        .installed_sessions
        .read(mismatched_thread, |slot| {
            (
                slot.realization.staging().transition.clone(),
                slot.realization.staging().effect_key.clone(),
                slot.inputs.workspace.clone(),
                slot.realization.staging().manifest.clone(),
            )
        })
        .expect("installed Sessions")
        .expect("R3 producer slot");
    let error = host
        .realization_executor
        .install_dispatched_resources(mismatched_thread, &revision_two, None)
        .await
        .expect_err("R3/E2 mismatched desired-only consumer");
    assert_eq!(error.code, "session_resource_transition_conflict", "R3/E2");
    assert_eq!(
        host.run_assembly
            .installed_sessions
            .read(mismatched_thread, |slot| {
                (
                    slot.realization.staging().transition.clone(),
                    slot.realization.staging().effect_key.clone(),
                    slot.inputs.workspace.clone(),
                    slot.realization.staging().manifest.clone(),
                )
            })
            .expect("installed Sessions"),
        Some(before_mismatch),
        "R3/E2 rejects without mutating correlated Resource state"
    );
}

#[tokio::test]
async fn complete_projection_resource_guard_excludes_desired_only_staging() {
    // Lock cause/effect table. C1 the complete projection owner holds the
    // `resource_projection` suffix guard; C2 desired-only cold staging races;
    // C3 the guard is released. Effects: E1 C1+C2 blocks before requirements,
    // transition, or active-manifest publication; E2 C2+C3 stages the proven
    // Empty->desired command and still leaves active completion absent.
    //
    // | Rule | C1 | C2 | C3 | Effect |
    // | L1 | T | T | F | E1 |
    // | L2 | F | T | T | E2 |
    let host = adoption_host().await;
    let _managed = host.clone();
    let thread = "resource-projection-guard";
    let manifest = crate::session_resource::SessionResourceManifest::new(
        host.run_assembly.local_workspace(),
        crate::session_resource::ResolvedSessionResources::default(),
    );
    host.run_assembly
        .installed_sessions
        .install(thread)
        .expect("installed Sessions");
    let guard = host
        .run_assembly
        .installed_sessions
        .realizations()
        .lock_resource_projection(thread)
        .await
        .unwrap();
    let mut staging = tokio::spawn({
        let host = host.clone();
        let manifest = manifest.clone();
        async move {
            host.realization_executor
                .install_dispatched_resources(thread, &manifest, None)
                .await
        }
    });
    tokio::task::yield_now().await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), &mut staging)
            .await
            .is_err(),
        "L1 desired-only staging must wait for the complete projection guard"
    );
    assert!(
        host.run_assembly
            .installed_sessions
            .realizations()
            .read_realization(thread, |r| {
                r.staging().transition.is_none()
                    && r.staging().effect_key.is_none()
                    && r.staging().manifest.is_none()
            })
            .unwrap()
            .unwrap_or(false),
        "L1 no correlated Resource fact is published while the guard is held"
    );

    drop(guard);
    staging
        .await
        .expect("L2 staging task")
        .expect("L2 cold staging succeeds after guard release");
    assert!(
        host.run_assembly
            .installed_sessions
            .realizations()
            .read_realization(thread, |r| {
                r.staging().transition.is_some()
                    && r.staging().effect_key.is_some()
                    && r.staging().manifest.is_none()
            })
            .unwrap()
            .unwrap_or(false),
        "L2 cold requirements and command stage without active completion"
    );
}

#[tokio::test]
async fn exact_prevalidated_staging_rejects_foreign_active_and_defers_bound_physical_inputs() {
    // Exact-staging cause/effect table. C1 resident active is previous/desired/
    // foreign; C2 no Environment is resident; C3 a current or prospective
    // durable binding exists; C4 desired File bytes are unavailable; C5 an
    // exact immutable Skill is selected. Effects: E1 foreign active rejects
    // before any Resource read or slot write; E2 C2+C3 defers File/provider
    // compilation until adoption; E3 C5 still stages the dispatch-semantic
    // Skill bytes needed by a cold Coordinator; E4 previous/desired without C3
    // may compile (covered by the live replacement and cold File tests).
    //
    // | Rule | C1 | C2 | C3 | C4 | Effect |
    // | P1 | foreign | any | any | T | E1 |
    // | P2 | none | T | T | T + C5 | E2+E3 |
    // | P3 | previous/desired | any | F | F | E4 |
    let storage = tempfile::tempdir().expect("Skill storage");
    let host = (adoption_dependencies()
        .await
        .with_skill_store(storage.path().join("skills")))
    .build()
    .expect("validated Host dependencies");
    let _managed = host.clone();
    let skill = crate::host::tests::frozen_skill_version(
        "cold-recovery",
        "Cold recovery",
        "recover on another Coordinator",
        "retain exact immutable bytes",
        &[],
    );
    let skill_hash = skill.bundle_sha256.clone();
    host.capabilities
        .tools
        .skills
        .create(
            awaken_resource_contract::SkillDefinition {
                id: "cold-recovery".into(),
                workspace_id: "workspace-a".into(),
                display_title: None,
                latest_version: 1,
                last_version: 1,
                timestamps: Default::default(),
            },
            skill,
        )
        .await
        .expect("P2 exact Skill store")
        .expect("P2 create exact Skill");
    let missing_resources = crate::session_resource::ResolvedSessionResources::try_new(
        vec![crate::session_resource::ResolvedInput {
            created_at_unix_ms: None,
            updated_at_unix_ms: None,
            binding_id: awaken_resource_contract::BindingId::new("missing-file-binding"),
            source: crate::session_resource::ResolvedInputSource::File {
                file_id: awaken_resource_contract::FileId::from("missing-file"),
            },
            mount_path: "/missing.txt".into(),
            access: awaken_resource_contract::ResourceAccess::ReadOnly,
            instructions: None,
        }],
        vec![crate::session_resource::ResolvedSkillBinding {
            kind: awaken_agent_contract::AgentSkillKind::Custom,
            skill_id: "cold-recovery".into(),
            version: 1,
            bundle_sha256: skill_hash,
        }],
    )
    .expect("valid missing-File projection");
    let previous = crate::session_resource::SessionResourceManifest::at_revision(
        "workspace-a",
        1,
        Default::default(),
    );
    let desired = crate::session_resource::SessionResourceManifest::at_revision(
        "workspace-a",
        2,
        missing_resources,
    );
    let transition =
        crate::session_resource::SessionResourceTransition::new(previous.clone(), desired.clone())
            .expect("exact transition");

    let foreign_thread = "foreign-active-resource";
    let foreign = crate::session_resource::SessionResourceManifest::at_revision(
        "workspace-a",
        9,
        Default::default(),
    );
    host.run_assembly
        .installed_sessions
        .install(foreign_thread)
        .expect("installed Sessions");
    host.realization_executor
        .staging()
        .register_thread_resource_manifest(foreign_thread, foreign.clone())
        .unwrap();
    host.run_assembly
        .installed_sessions
        .install_with(foreign_thread, |slot| {
            slot.replace_frozen_with(|next| next.workspace = Some("sentinel-workspace".into()));
            slot.realization.staging_mut().effect_key = Some("sentinel-key".into());
        })
        .expect("installed Sessions");
    let _guard = host
        .run_assembly
        .installed_sessions
        .realizations()
        .lock_resource_projection(foreign_thread)
        .await
        .unwrap();
    let error = host
        .realization_executor
        .staging()
        .stage_prevalidated_resource_transition_under_resource_projection(
            _guard.held(),
            host.realization_executor.peers(),
            foreign_thread,
            &transition,
            None,
            None,
            false,
        )
        .await
        .expect_err("P1 foreign active generation must fail closed");
    assert_eq!(error.code, "session_resource_transition_conflict", "P1");
    assert!(
        host.run_assembly
            .installed_sessions
            .read(foreign_thread, |slot| {
                slot.realization.staging().manifest.as_ref() == Some(&foreign)
                    && slot.inputs.workspace.as_deref() == Some("sentinel-workspace")
                    && slot.realization.staging().effect_key.as_deref() == Some("sentinel-key")
                    && slot.realization.staging().resources.mounts.is_empty()
            })
            .expect("installed Sessions")
            .unwrap_or(false),
        "P1 rejects before compilation or any correlated slot write"
    );
    drop(_guard);

    let bound_thread = "prospective-bound-resource";
    host.run_assembly
        .installed_sessions
        .install(bound_thread)
        .expect("installed Sessions");
    let _guard = host
        .run_assembly
        .installed_sessions
        .realizations()
        .lock_resource_projection(bound_thread)
        .await
        .unwrap();
    host.realization_executor
        .staging()
        .stage_prevalidated_resource_transition_under_resource_projection(
            _guard.held(),
            host.realization_executor.peers(),
            bound_thread,
            &transition,
            None,
            None,
            true,
        )
        .await
        .expect("P2 prospective binding defers missing File compilation");
    assert!(
        host.run_assembly
            .installed_sessions
            .realizations()
            .read_realization(bound_thread, |r| {
                r.staging().manifest.is_none()
                    && r.staging().transition.is_none()
                    && r.staging().effect_key.is_none()
                    && r.staging().resources.mounts.is_empty()
                    && r.staging().skills.as_ref().is_some_and(|skills| {
                        skills.len() == 1 && skills[0].skill_id.as_str() == "cold-recovery"
                    })
            })
            .unwrap()
            .unwrap_or(false),
        "P2/E2-E3 only exact semantic Skill bytes precede adoption"
    );
}

/// Cause/effect decision table for Worker-side File staging:
/// | Rule | Worker File source | exact claim | local File DB entry | Effect |
/// |---|---|---|---|---|
/// | W1 | remote adapter | present | absent | stage returned digest/bytes; later runtime validation does not reopen a local File database |
/// | W2 | remote adapter | absent | absent | fail closed; no mount |
#[tokio::test]
async fn cold_worker_file_staging_uses_only_the_claim_bound_source() {
    struct ClaimFileSource;

    #[async_trait::async_trait]
    impl crate::FileContentSource<crate::ResourceOperationFence> for ClaimFileSource {
        async fn read(
            &self,
            workspace_id: &str,
            file_id: &str,
            _purpose: &awaken_resource_contract::FileReadPurpose,
            fence: Option<&crate::ResourceOperationFence>,
        ) -> Result<
            Option<awaken_resource_contract::ResolvedFileContent>,
            awaken_resource_contract::FileContentSourceError,
        > {
            let Some(crate::ResourceOperationFence::Run(claim)) = fence else {
                return Ok(None);
            };
            if workspace_id != "workspace-remote"
                || file_id != "file-remote"
                || claim.run_id.0 != "run-remote"
                || claim.owner != "worker-remote"
                || claim.epoch != 7
            {
                return Ok(None);
            }
            let bytes = b"remote immutable File".to_vec();
            Ok(Some(awaken_resource_contract::ResolvedFileContent {
                file_id: file_id.into(),
                content_id: awaken_resource_contract::content_id(&bytes),
                filename: "remote.txt".into(),
                media_type: "text/plain".into(),
                bytes,
            }))
        }
    }

    let host = (adoption_dependencies()
        .await
        .with_file_content_source(Arc::new(ClaimFileSource)))
    .build()
    .expect("validated Host dependencies");
    let managed = host.clone();
    let manifest = crate::session_resource::SessionResourceManifest::new(
        "workspace-remote",
        crate::session_resource::ResolvedSessionResources::try_new(
            vec![crate::session_resource::ResolvedInput {
                created_at_unix_ms: None,
                updated_at_unix_ms: None,
                binding_id: awaken_resource_contract::BindingId::new("remote-file-binding"),
                source: crate::session_resource::ResolvedInputSource::File {
                    file_id: awaken_resource_contract::FileId::from("file-remote"),
                },
                mount_path: "/input.txt".into(),
                access: awaken_resource_contract::ResourceAccess::ReadOnly,
                instructions: None,
            }],
            Vec::new(),
        )
        .unwrap(),
    );
    let claim = ingress::RunClaim {
        run_id: RunId("run-remote".into()),
        owner: "worker-remote".into(),
        epoch: 7,
    };

    host.realization_executor
        .install_dispatched_resources("remote-file-ok", &manifest, Some(&claim))
        .await
        .expect("W1");
    let mount = host
        .realization_executor
        .sandbox_spec("remote-file-ok")
        .mounts
        .into_iter()
        .find(|mount| mount.mount_id == "file-remote")
        .expect("W1 exact mount");
    assert!(
        matches!(
            mount.source,
            awaken_provisioning_contract::MountSource::InlineBytes { ref contents, .. }
                if contents == b"remote immutable File"
        ),
        "W1"
    );
    assert!(
        active_manifest(&host, "remote-file-ok").is_none(),
        "W1 desired-only staging does not publish active completion"
    );
    managed
        .realization_executor
        .validate_thread_resource_bindings("remote-file-ok")
        .await
        .expect("W1 has no redundant local File catalog check");

    let error = host
        .realization_executor
        .install_dispatched_resources("remote-file-no-claim", &manifest, None)
        .await
        .expect_err("W2");
    assert!(error.to_string().contains("not found"), "W2: {error}");
    assert!(
        host.realization_executor
            .sandbox_spec("remote-file-no-claim")
            .mounts
            .is_empty(),
        "W2"
    );
}

#[tokio::test]
async fn sandbox_binding_is_validated_before_provider_adoption() {
    let storage = tempfile::tempdir().expect("storage");
    let host = adoption_host_at(storage.path()).await;
    assert!(
        host.realization_executor
            .adopt_bound_session_environment(
                "thread-a",
                Some("not-json"),
                &host
                    .realization_executor
                    .environments()
                    .providers()
                    .session_provider,
                None,
                false
            )
            .await
            .is_err()
    );

    let wrong = serde_json::to_string(&awaken_provisioning_contract::SandboxHandle::new(
        "local", "thread-b",
    ))
    .unwrap();
    assert!(
        host.realization_executor
            .adopt_bound_session_environment(
                "thread-a",
                Some(&wrong),
                &host
                    .realization_executor
                    .environments()
                    .providers()
                    .session_provider,
                None,
                false
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn recovery_mode_controls_the_production_adoption_seam() {
    // Fixture rule: C0 an executable Session first owns the canonical Runtime
    // composition and exact empty Resource transition; E0 its durable handle
    // can then exercise continuity/rebuild adoption without a partial slot path.
    //
    // Adoption cause/effect decision table:
    // | rule | provider observation | recovery policy | resident | effect |
    // | A1 | Ready exact handle | continuity | absent | adopt exact binding |
    // | A2 | definitively unavailable | rebuild | absent | classify RebuildRequired; actual target pending without accepted closure/access |
    // | A3 | definitively unavailable | continuity | absent | fail, zero mutation |
    // The dead-resident companion test covers A2 with a resident Arc: only the
    // exact observed Arc is discarded before rebuild is projected. Parameter
    // aggregation must preserve these existing typed observations and effects.
    // C4 the producer and consumer configure one real Root before Host build;
    // replay reads its original Create/lease/binding rather than seeding a new
    // scope or copying a binding into a fresh Unmaterialized projection.
    let storage = tempfile::tempdir().expect("storage");
    let thread = "thread-adoption";
    let roots = ManagedSessionRootFixture::new().await;
    let first = roots
        .configure(adoption_dependencies_at(storage.path()).await)
        .build()
        .unwrap();
    let first_managed = managed_test_host(first.clone());
    let projection =
        empty_frozen_projection(first.run_assembly.local_workspace(), eager_environment());
    let installed = roots
        .install_projection(&first_managed, thread, projection.clone())
        .await
        .unwrap();
    let encoded = available_local_environment_binding(&first, thread, &installed).await;
    let handle: awaken_provisioning_contract::SandboxHandle =
        serde_json::from_str(&encoded).expect("A1 exact ready handle");
    drop(first_managed);
    drop(first);

    let replacement = roots
        .configure(adoption_dependencies_at(storage.path()).await)
        .build()
        .unwrap();
    let replacement_managed = managed_test_host(replacement.clone());
    roots
        .install_projection(&replacement_managed, thread, projection)
        .await
        .expect("A1 replay the same Root and its original consumer identity");
    let run_id = RunId("run-adoption".into());
    let adopted = adopt_bound_sandbox(
        replacement.run_assembly.as_ref(),
        Some(&encoded),
        thread,
        &run_id,
        &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
        ingress::WorkerRecoveryMode::RequireSandboxContinuity,
    )
    .await
    .expect("continuity mode adopts the durable handle");
    assert_eq!(
        adopted,
        awaken_session_realization::session_environment::SessionEnvironmentAdoptionDisposition::Ready
    );
    assert_eq!(
        replacement
            .run_assembly
            .session_environment_handle(thread)
            .await
            .expect("published adopted handle"),
        handle
    );

    let missing_thread = "thread-missing";
    let missing_initial = super::test_support::empty_frozen_projection(
        replacement.local_workspace(),
        super::test_support::eager_environment(),
    );
    let missing_projection = roots
        .install_projection(
            &replacement_managed,
            missing_thread,
            missing_initial.clone(),
        )
        .await
        .expect("A2 install the independent missing-row Root before original creation");
    let missing = super::test_support::unavailable_local_environment_binding(
        &replacement,
        missing_thread,
        &missing_projection,
    )
    .await;
    // The Create helper committed original Root identity after installation.
    // Replay that same initial declaration to install Store-read Resident and
    // materialization; the old pre-Create projection is not recovery authority.
    let current_missing = roots
        .install_projection(&replacement_managed, missing_thread, missing_initial)
        .await
        .expect("A2 install original Root readback after physical loss");
    assert_eq!(
        current_missing.environment.binding(),
        Some(missing.as_str()),
        "A2 actual original source binding"
    );
    let source_generation_id = current_missing.environment.generation().map(|g| g.id());
    let refusal = replacement
        .realization_executor
        .adopt_bound_session_environment(
            missing_thread,
            Some(&missing),
            &replacement
                .realization_executor
                .environments()
                .providers()
                .session_provider,
            source_generation_id,
            true,
        )
        .await
        .expect_err("A2 missing UID cannot substitute for original source closure/access");
    assert_eq!(
        refusal.disposition(),
        crate::realization_error::Disposition::Retry,
        "A2 retryable source wait"
    );
    assert_eq!(
        refusal.code(),
        "session_environment_materialization_pending",
        "A2 canonical Root fence before source consumption"
    );
    assert_eq!(
        refusal.to_string(),
        "Session materialization remains pending its exact original effect and source closure",
        "A2 exact original-source refusal"
    );
    // The original Root guard runs before consuming a protected source owner.
    // It therefore refuses recovery classification as well as a target effect
    // until closure/access is accepted. Ordinary context opening separately
    // requires prior adoption; its existing missing-binding precondition must
    // reject before another effect, rather than bypassing the Root refusal.
    let error = match replacement.run_assembly.ctx_for(missing_thread, None).await {
        Ok(_) => panic!("A2 unknown source evidence cannot authorize target materialization"),
        Err(error) => error,
    };
    assert_eq!(
        error.kind,
        crate::SessionExecutionErrorKind::Internal,
        "A2 context requires the original binding to be adopted"
    );
    assert_eq!(error.code, "internal", "A2 existing context precondition");
    assert_eq!(
        error.message,
        format!(
            "Session {missing_thread} durable Environment was not adopted before context construction"
        ),
        "A2 no alternate construction path"
    );
    assert!(
        replacement
            .run_assembly
            .session_environment_handle(missing_thread)
            .await
            .is_none(),
        "A2 zero replacement publication"
    );
    assert!(
        !replacement
            .storage_dir()
            .unwrap()
            .join("sandboxes")
            .join(missing_thread)
            .exists(),
        "A2 zero target physical creation"
    );
    let after = roots
        .read_projection(missing_thread, &current_missing)
        .await
        .unwrap();
    assert_eq!(
        after.environment, current_missing.environment,
        "A2 original source binding/effect/generation retained"
    );
    assert_eq!(
        after.environment_materialization, current_missing.environment_materialization,
        "A2 original Create intent/history retained"
    );

    assert!(
        adopt_bound_sandbox(
            replacement.run_assembly.as_ref(),
            Some(&missing),
            missing_thread,
            &run_id,
            &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
            ingress::WorkerRecoveryMode::RequireSandboxContinuity,
        )
        .await
        .is_err(),
        "continuity mode fails closed when the bound sandbox is gone"
    );
}

#[tokio::test]
async fn cold_adoption_uses_the_provider_owned_by_frozen_model_provisioning() {
    // Cause/effect graph: C1=HostExecutor/Provider model; C2=BackendOwned
    // model; C3=durable handle exists only below the selected provider root.
    // Effects: E1=adopt from configured Session root; E2=adopt from trusted
    // host-identity root; E3=missing selected root fails continuity.
    //
    // Decision table: A1 C1+C3(configured)=>E1 (covered by
    // recovery_mode_controls...); A2 C2+C3(trusted)=>E2; A3 either model
    // with its selected root absent=>E3 (covered by that test's missing row).
    // C4 both Hosts share one configured actual Root/assignment; the source
    // handle is created and committed under that original authority. Provider
    // selection remains independent from Runtime/ACP adapter construction.
    let storage = tempfile::tempdir().expect("storage");
    let trusted_root = storage.path().join("trusted-local-sandboxes");
    let thread = "thread-backend-owned-adoption";
    let candidate = awaken_runtime_contract::resolved::ResolvedModelCandidate::try_backend_owned(
        awaken_runtime_contract::resolved::ModelBinding::new("provider", "", "acp:codex"),
        awaken_runtime_contract::CredentialRef {
            id: "local".into(),
            revision: 1,
        },
        awaken_runtime_contract::resolved::BackendModelSelection::Default,
        "test-acp-adapter-v1",
        "test-acp-capability-v1",
        Default::default(),
    )
    .expect("A2 valid BackendOwned candidate");
    let provisioning = candidate.provisioning().clone();
    let mut activation = test_activation(thread, "fixture-backend-owned-adoption");
    activation.snapshot.resolved_spec.model_binding = candidate;

    let roots = ManagedSessionRootFixture::new().await;
    let mut first = adoption_dependencies_at(storage.path()).await;
    first.env.backend_owned_session_provider = Some(
        awaken_session_environment::SandboxEnvironmentProvider::workdir(&trusted_root)
            .into_session_provider(),
    );
    let first = roots.configure(first).build().unwrap();
    let first_managed = managed_test_host(first.clone());

    let projection = empty_frozen_projection_for_snapshot(
        first.run_assembly.local_workspace(),
        eager_environment(),
        &activation.snapshot,
    );
    let installed = roots
        .install_projection(&first_managed, thread, projection.clone())
        .await
        .unwrap();
    let encoded = available_local_environment_binding(&first, thread, &installed).await;
    let handle: awaken_provisioning_contract::SandboxHandle =
        serde_json::from_str(&encoded).expect("A2 exact ready handle");
    drop(first_managed);
    drop(first);
    assert!(
        trusted_root.join(thread).exists(),
        "A2 trusted root owns the durable environment"
    );
    assert!(
        !storage.path().join("sandboxes").join(thread).exists(),
        "A2 ordinary provider root cannot satisfy this handle"
    );

    let mut replacement = adoption_dependencies_at(storage.path()).await;
    replacement.env.backend_owned_session_provider = Some(
        awaken_session_environment::SandboxEnvironmentProvider::workdir(&trusted_root)
            .into_session_provider(),
    );
    let replacement = roots.configure(replacement).build().unwrap();

    let replacement_managed = managed_test_host(replacement.clone());
    roots
        .install_projection(&replacement_managed, thread, projection)
        .await
        .expect("A2 replay the original Root under the same provider selection");
    let adopted = adopt_bound_sandbox(
        replacement.run_assembly.as_ref(),
        Some(&encoded),
        thread,
        &RunId("run-backend-owned-adoption".into()),
        &provisioning,
        ingress::WorkerRecoveryMode::RequireSandboxContinuity,
    )
    .await
    .expect("A2 adopt from the provisioning-selected provider");
    assert_eq!(
        adopted,
        awaken_session_realization::session_environment::SessionEnvironmentAdoptionDisposition::Ready,
        "A2"
    );
    assert_eq!(
        replacement
            .run_assembly
            .session_environment_handle(thread)
            .await
            .expect("A2 adopted"),
        handle,
        "A2"
    );
}

#[tokio::test]
async fn a_resident_environment_is_reused_without_a_second_adoption() {
    // R1: canonical Runtime composition + direct empty transition creates the
    // resident Environment; matching binding adoption reuses that exact owner.
    let storage = tempfile::tempdir().expect("storage");
    let thread = "thread-resident-adoption";
    let host = adoption_host_at(storage.path()).await;
    let _managed = managed_test_host(host.clone());
    let ctx = host
        .run_assembly
        .ctx_for(thread, None)
        .await
        .expect("resident session");
    let handle = ctx
        .env
        .as_ref()
        .expect("eager environment")
        .lifecycle()
        .handle();
    let encoded = serde_json::to_string(&handle).unwrap();

    let adopted = adopt_bound_sandbox(
        host.run_assembly.as_ref(),
        Some(&encoded),
        thread,
        &RunId("resident-run".into()),
        &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
        ingress::WorkerRecoveryMode::RequireSandboxContinuity,
    )
    .await
    .expect("resident handle is already adopted");

    assert_eq!(
        adopted,
        awaken_session_realization::session_environment::SessionEnvironmentAdoptionDisposition::Ready,
        "the lifecycle owner publishes no duplicate wrapper"
    );
    assert_eq!(
        host.run_assembly.session_environment_handle(thread).await,
        Some(handle)
    );
}

#[tokio::test]
async fn a_dead_resident_environment_fails_continuity_and_is_fenced_before_rebuild() {
    // R1/R2: a canonically created resident whose provider state disappears is
    // retained by continuity mode and discarded only by explicit rebuild.
    let storage = tempfile::tempdir().expect("storage");
    let thread = "thread-dead-resident";
    let host = adoption_host_at(storage.path()).await;
    let _managed = managed_test_host(host.clone());
    let ctx = host
        .run_assembly
        .ctx_for(thread, None)
        .await
        .expect("resident session");
    let handle = ctx
        .env
        .as_ref()
        .expect("eager environment")
        .lifecycle()
        .handle();
    let encoded = serde_json::to_string(&handle).unwrap();
    std::fs::remove_dir_all(storage.path().join("sandboxes").join(thread))
        .expect("terminate local sandbox out of band");

    let run = RunId("dead-resident-run".into());
    assert!(
        adopt_bound_sandbox(
            host.run_assembly.as_ref(),
            Some(&encoded),
            thread,
            &run,
            &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
            ingress::WorkerRecoveryMode::RequireSandboxContinuity,
        )
        .await
        .is_err(),
        "continuity never silently replaces a dead resident sandbox"
    );
    assert_eq!(
        host.run_assembly.session_environment_handle(thread).await,
        Some(handle.clone()),
        "a failed continuity check does not mutate the owner registry"
    );

    let adopted = adopt_bound_sandbox(
        host.run_assembly.as_ref(),
        Some(&encoded),
        thread,
        &run,
        &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
        ingress::WorkerRecoveryMode::RebuildFromCommittedTruth,
    )
    .await
    .expect("explicit rebuild may discard the dead resident environment");
    assert_eq!(
        adopted,
        awaken_session_realization::session_environment::SessionEnvironmentAdoptionDisposition::RebuildRequired
    );
    assert!(
        host.run_assembly
            .resident_session_environment(thread)
            .await
            .is_none()
    );
    assert!(
        !host
            .run_assembly
            .installed_sessions
            .read_run(thread, |run| run.context.is_some())
            .expect("installed Sessions")
            .unwrap_or(false)
    );
}

#[tokio::test]
async fn a_stale_binding_cannot_evict_a_different_resident_environment() {
    // R1: canonical resident + stale full-handle binding => reject adoption and
    // retain both the exact Environment owner and Runtime.
    let storage = tempfile::tempdir().expect("storage");
    let thread = "thread-binding-fence";
    let host = adoption_host_at(storage.path()).await;
    let _managed = managed_test_host(host.clone());
    let ctx = host
        .run_assembly
        .ctx_for(thread, None)
        .await
        .expect("resident session");
    let resident = ctx
        .env
        .as_ref()
        .expect("eager environment")
        .lifecycle()
        .handle();
    let stale = awaken_provisioning_contract::SandboxHandle::new(
        resident.provider_kind(),
        resident.sandbox_id.clone(),
    );
    let encoded = serde_json::to_string(&stale).unwrap();

    assert!(
        adopt_bound_sandbox(
            host.run_assembly.as_ref(),
            Some(&encoded),
            thread,
            &RunId("stale-binding-run".into()),
            &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
            ingress::WorkerRecoveryMode::RebuildFromCommittedTruth,
        )
        .await
        .is_err(),
        "rebuild mode cannot override the full-handle fence"
    );
    assert_eq!(
        host.run_assembly.session_environment_handle(thread).await,
        Some(resident)
    );
    assert!(
        host.run_assembly
            .installed_sessions
            .read_run(thread, |run| run.context.is_some())
            .expect("installed Sessions")
            .unwrap_or(false)
    );
}

#[tokio::test]
async fn an_aba_replacement_with_the_same_handle_survives_stale_discard() {
    // Cause/effect rule: C1 canonical V2 creation and first adoption publish
    // owner A; C2 spec-aware re-adoption yields owner B with the same handle;
    // C3 stale discard still names A. E1 exact Arc identity fences C3 and B
    // remains Resident. A legacy V1 fixture cannot prove C2 and is excluded.
    // C0 an explicit same-Actor Root admits and commits the source Create
    // before either adoption; no test fence supplies its original identity.
    // C4 existing Resident + exact binding/handle retains Durable identity
    // while the canonical test setter replaces only its Arc. A fresh empty
    // slot has no original identity and remains the setter's legacy row.
    // Rule C0+C1+C2+C4=>same Root identity; C3 cannot discard replacement B.
    let storage = tempfile::tempdir().expect("storage");
    let thread = "thread-environment-aba";
    let roots = ManagedSessionRootFixture::new().await;
    let host = roots
        .configure(adoption_dependencies_at(storage.path()).await)
        .build()
        .unwrap();
    let managed = managed_test_host(host.clone());
    let projection = roots
        .install_projection(
            &managed,
            thread,
            empty_frozen_projection(host.run_assembly.local_workspace(), eager_environment()),
        )
        .await
        .unwrap();
    let binding = available_local_environment_binding(&host, thread, &projection).await;
    adopt_bound_sandbox(
        host.run_assembly.as_ref(),
        Some(&binding),
        thread,
        &RunId("aba-fixture-run".into()),
        &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
        ingress::WorkerRecoveryMode::RequireSandboxContinuity,
    )
    .await
    .expect("C1 publish first exact owner");
    let observed = host
        .run_assembly
        .resident_session_environment(thread)
        .await
        .expect("observed owner");
    let spec = host
        .realization_executor
        .staging()
        .sandbox_spec_for_resolved_resources_and_provider(
            thread,
            &projection.resources,
            &host
                .realization_executor
                .environments()
                .providers()
                .session_provider,
        )
        .unwrap();
    let spec = host
        .realization_executor
        .environments()
        .validate_session_environment_capabilities(
            host.realization_executor.peers(),
            &host
                .realization_executor
                .environments()
                .providers()
                .session_provider,
            &spec,
        )
        .expect("C2 exact adoption spec");
    let replacement = Arc::new(
        host.realization_executor
            .environments()
            .providers()
            .session_provider
            .adopt(&spec, &observed.lifecycle().handle())
            .await
            .expect("same-handle replacement"),
    );
    assert_eq!(
        observed.lifecycle().handle(),
        replacement.lifecycle().handle()
    );
    assert!(!Arc::ptr_eq(&observed, &replacement));

    let original_identity = host
        .run_assembly
        .installed_sessions
        .realizations()
        .read_realization(thread, |r| match &r.environment().owner {
            crate::environment_owner::SessionEnvironmentOwner::Resident(bound) => {
                bound.identity.clone()
            }
            _ => panic!("C1 exact original Resident owner"),
        })
        .unwrap()
        .expect("C1 original Root identity");
    assert!(
        matches!(
            original_identity,
            crate::environment_owner::BoundSessionEnvironmentIdentity::Durable { .. }
        ),
        "C0 original Root supplied Durable identity"
    );
    host.run_assembly
        .install_test_resident_session_environment(thread, replacement.clone());
    assert_eq!(
        host.run_assembly
            .installed_sessions
            .realizations()
            .read_realization(thread, |r| {
                match &r.environment().owner {
                    crate::environment_owner::SessionEnvironmentOwner::Resident(bound) => {
                        bound.identity.clone()
                    }
                    _ => panic!("C4 replacement remains Resident"),
                }
            })
            .unwrap(),
        Some(original_identity),
        "C4 same-handle Arc replacement preserves original Root identity"
    );

    assert!(
        !host
            .run_assembly
            .discard_session_environment(thread, &observed)
            .await,
        "object identity fences a stale observer even when the handle is reused"
    );
    let current = host
        .run_assembly
        .resident_session_environment(thread)
        .await
        .expect("replacement kept");
    assert!(Arc::ptr_eq(&current, &replacement));
}

#[tokio::test]
async fn cancellation_resolution_does_not_touch_an_invalid_sandbox_binding() {
    // Test design. Causes: C1 a cancellation-only claim carries an invalid
    // opaque sandbox binding. Effects: E1 cancellation resolves without
    // parsing/adopting that binding; E2 no sandbox side effect occurs.
    // Constraint/Invariant: terminal control does not require execution
    // realization. Decision rule: exercise C1 and require control-only success.
    let storage = tempfile::tempdir().expect("storage");
    let host = adoption_host_at(storage.path()).await;
    let thread = ThreadId("thread-control-only".to_string());
    let run = RunId("run-control-only".to_string());
    let fingerprint = CatalogFingerprint("control-only-catalog".to_string());
    let activation = RunActivation::new(
        run.clone(),
        thread.clone(),
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("control-only-snapshot".to_string()),
            metadata: Default::default(),
            root_agent_id: AgentId("control-only-agent".to_string()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: fingerprint.clone(),
                instructions: "test".to_string(),
                max_steps: Some(1),
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding::new("provider", "model", "backend"),
                ),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint,
        },
        Vec::new(),
    );
    let claimed = ingress::Claimed {
        request: ingress::RunDispatch::new(activation),
        lease: ingress::DispatchLease::from_persisted(run, "control-owner".to_string(), 100, 2)
            .unwrap(),
        session_work_authority: None,
        credential_bindings: Vec::new(),
        credential_deliveries: Vec::new(),
        cancellation_requested: true,
        pending: Vec::new(),
        recovered: false,
        session_activity_admission_required: false,
        sandbox: Some("this is deliberately not a sandbox handle".to_string()),
        assignment: None,
    };
    let resolver = crate::host::session_worker_resolver(&host.run_assembly, &host.dispatch)
        .expect("Host dispatch resolver");

    resolver
        .worker_for_claimed(&claimed)
        .await
        .expect("terminal control bypasses sandbox decoding/adoption");
    assert!(
        host.run_assembly
            .resident_session_environment(&thread.0)
            .await
            .is_none()
    );
    assert!(
        !host
            .run_assembly
            .installed_sessions
            .read_run(&thread.0, |run| run.context.is_some())
            .expect("installed Sessions")
            .unwrap_or(false)
    );
}

#[tokio::test]
async fn outcome_work_receipt_installs_baseline_before_runtime_and_later_session_use() {
    // Cause/effect table: ordinary Run + Coordinator-issued Session Work receipt
    // -> canonical frozen realization before runtime. No receipt -> O5's existing
    // ordinary path. Wrong Session/owner receipt -> reject before environment effects.
    // Later public Session use must reuse the exact baseline, not fail because an
    // extension eagerly opened the Thread. Run admission identity stays OrdinaryRoot.
    for invalid_receipt in [false, true] {
        let thread = format!("outcome-baseline-{invalid_receipt}");
        let activation = test_activation(&thread, "outcome-owned-run");
        let frozen = empty_frozen_projection_for_snapshot(
            "workspace",
            eager_environment(),
            &activation.snapshot,
        );
        let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
        let dispatch = Arc::new(
            awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
                .await
                .unwrap(),
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let host = (adoption_dependencies()
            .await
            .with_ephemeral_dispatch(dispatch.clone())
            .with_session_realization_control(Arc::new(RecordingSessionControl {
                resume_calls: calls.clone(),
                continuations: Arc::new(std::sync::Mutex::new(Vec::new())),
                phases: Arc::new(std::sync::Mutex::new(Vec::new())),
                projection: Arc::new(std::sync::Mutex::new(Some(frozen.clone()))),
                mcp_stage: None,
                fail_begin: Arc::new(AtomicBool::new(false)),
            })))
        .build()
        .expect("validated Host dependencies");
        let managed = managed_test_host(host.clone());
        managed
            .run_assembly
            .install_test_environment_binding_sink(Arc::new(ToggleBindingSink {
                fail: AtomicBool::new(false),
                calls: AtomicUsize::new(0),
                binding: std::sync::Mutex::new(None),
            }));
        dispatch
            .enqueue(with_empty_session_resources(
                ingress::RunDispatch::new(activation),
                "workspace",
            ))
            .await
            .unwrap();
        let mut claimed = dispatch
            .claim("worker-a", 30000, &Default::default())
            .await
            .unwrap()
            .unwrap();
        // Serialized edge receipt fixture; production issuance is solely the Coordinator
        // acquisition port. The real Control path validates it again before realization.
        claimed.session_work_authority = Some(
            serde_json::from_value(serde_json::json!({
                "session_id": if invalid_receipt {"foreign"} else {thread.as_str()},
                "worker_owner":"worker-a", "authority":{"kind":"not_required"},
            }))
            .unwrap(),
        );
        let result = crate::host::session_worker_resolver(&host.run_assembly, &host.dispatch)
            .expect("Host dispatch resolver")
            .worker_for_claimed(&claimed)
            .await;
        if invalid_receipt {
            assert!(result.is_err());
            assert!(
                host.run_assembly
                    .resident_session_environment(&thread)
                    .await
                    .is_none()
            );
            continue;
        }
        result.expect("Outcome Work receipt realizes exact baseline");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            host.run_assembly
                .installed_sessions
                .read(&thread, |slot| slot.inputs.baseline.is_some()
                    && slot.run.context.is_some())
                .expect("installed Sessions")
                .unwrap_or(false)
        );
        assert!(
            host.run_assembly
                .resident_session_environment(&thread)
                .await
                .is_some()
        );
        assert_eq!(
            claimed.request.admission_shape(),
            ingress::DispatchAdmissionShape::OrdinaryRoot
        );
        awaken_session_execution_contract::RealizationRuntime::install_session_projection(
            &*managed.run_assembly,
            &thread,
            frozen,
            awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch,
        )
        .await
        .expect("later Session use preserves frozen baseline");
    }
}

#[tokio::test]
async fn outcome_projection_read_does_not_adopt_worker_owned_environment() {
    // Causes: frozen placement is Worker and its durable Environment is Resident,
    // but this cold Coordinator has no local owner. Effects: prepare/read reuse
    // coordination context and committed Outcome truth without physical adoption;
    // a pure read creates no model call or replacement Environment.
    let thread = "outcome-coordinator-read";
    let activation = test_activation(thread, "outcome-source");
    let mut projection = empty_frozen_projection_for_snapshot(
        "workspace",
        eager_environment(),
        &activation.snapshot,
    );
    projection.baseline.runtime_placement = awaken_session_types::SessionRuntimePlacement::Worker;
    projection.environment = awaken_session_state_contract::SessionEnvironmentState::Resident {
        binding: "opaque-worker-owned-environment".into(),
        effect_id: None,
        generation: None,
        idle_since_unix_ms: None,
    };
    let host = adoption_host().await;
    let managed = managed_test_host(host.clone());
    awaken_session_execution_contract::RealizationRuntime::install_session_projection(
        &*managed.run_assembly,
        thread,
        projection,
        awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .unwrap();
    host.outcomes
        .prepare_outcome(thread, "outcome-a", "finish", "done", 1)
        .await
        .expect("prepare uses no local Environment");
    assert!(
        host.thread_reads
            .committed_outcome(thread, "outcome-a")
            .await
            .expect("read remote-owned outcome")
            .is_none()
    );
    assert!(
        host.run_assembly
            .resident_session_environment(thread)
            .await
            .is_none()
    );
}

/// Whether the realizer holds a live Environment for `thread`.
async fn has_environment(host: &ExecutorComponents, thread: &str) -> bool {
    host.run_assembly
        .resident_session_environment(thread)
        .await
        .is_some()
}

/// The Session's active Resource manifest as Resource staging recorded it.
fn active_manifest(
    host: &ExecutorComponents,
    thread: &str,
) -> Option<crate::session_resource::SessionResourceManifest> {
    host.realization_executor
        .staging()
        .thread_resource_manifest(thread)
        .unwrap()
}

use awaken_dispatch_contract::{
    DispatchClaims as _, DispatchEnqueue as _, DispatchSettlement as _, SessionRunReservations as _,
};
