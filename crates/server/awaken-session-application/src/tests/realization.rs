use super::*;
use awaken_session_contract::SessionRealizationControl;

fn terminal_prepare_commands(
    work: awaken_session_contract::SessionTerminalCleanupWork,
) -> Vec<awaken_session_contract::SessionCleanupCommand> {
    match work.action {
        awaken_session_contract::SessionTerminalCleanupAction::Prepare { commands } => commands,
        action => panic!("expected terminal preparation action, got {action:?}"),
    }
}

/// Recovery-scan causal graph: C1 one supervisor cycle owns Resource,
/// Environment, realization, Event, and Outcome convergence; C2 every handler
/// previously opened the same durable candidate index; C3 cutover validation is
/// a distinct post-cycle audit. Effects are E1 one candidate scan feeds all
/// handlers, E2 handlers reload roots by id before effects, and E3 the final
/// audit remains an independent second scan.
///
/// | Rule | Candidate scan | Cutover audit | Expected scans |
/// |---|---|---|---|
/// | S1 | succeeds | enabled | 2 (E1 + E3) |
/// | S2 | unavailable | not reached | 1 and retryable failure |
#[test]
fn one_recovery_cycle_scans_candidates_once_before_the_final_audit() {
    run_composed_async_test(|| async {
        let durable: Arc<dyn ManagedSessionRepository> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("session repository"),
        );
        let observed = Arc::new(FaultingSessionRepository::new(durable));
        let application = Arc::new(application(
            observed.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        ));

        observed.fail_recovery_scan_once();
        let unavailable = application.clone().reconcile_pending_session_state().await;
        assert_eq!(unavailable.retryable_failures, 1, "S2");
        assert_eq!(observed.recovery_scan_count(), 1, "S2");

        let cycle = application.reconcile_pending_session_state().await;
        assert_eq!(cycle.retryable_failures, 0, "S1");
        assert_eq!(observed.recovery_scan_count(), 3, "S1/E1/E3");
    });
}

#[derive(Default)]
pub(super) struct RecordingResourceRuntime {
    applied: Mutex<Vec<(u64, awaken_session_contract::ResolvedSessionResources)>>,
    pub(super) replaced_tools:
        Mutex<Vec<(String, awaken_session_contract::SessionToolConfiguration)>>,
}

#[async_trait::async_trait]
impl SessionRuntime for RecordingResourceRuntime {
    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_contract::FrozenSessionProjection,
        mode: awaken_session_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        install_complete_test_projection(self, thread, projection, mode).await
    }

    async fn apply_session_inputs(
        &self,
        _thread: &str,
        transition: &awaken_session_contract::SessionResourceTransition,
    ) -> Result<(), RunError> {
        self.applied.lock().unwrap().push((
            transition.desired().revision,
            transition.desired().resources.clone(),
        ));
        Ok(())
    }

    async fn replace_session_tools(
        &self,
        thread: &str,
        tools: awaken_session_contract::SessionToolConfiguration,
    ) -> Result<(), RunError> {
        self.replaced_tools
            .lock()
            .unwrap()
            .push((thread.to_owned(), tools));
        Ok(())
    }

    async fn run(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        NoopRuntime.run(agent, thread, content).await
    }

    async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        decision: awaken_session_contract::ToolPermissionDecision,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        NoopRuntime.resume(thread, tool_use_id, decision).await
    }

    async fn resume_custom(
        &self,
        thread: &str,
        tool_use_id: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        is_error: bool,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        NoopRuntime
            .resume_custom(thread, tool_use_id, content, is_error)
            .await
    }

    async fn define_outcome(
        &self,
        thread: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<awaken_session_contract::OutcomeDrive, RunError> {
        NoopRuntime
            .define_outcome(thread, description, rubric, max_iterations)
            .await
    }

    fn model(&self) -> String {
        "resource-generation-test".into()
    }
}

/// Skills-only frozen-projection cause/effect graph. C1=active mounted inputs;
/// C2=one active exact Skill pin; C3=pending replacement. E1 `from_active`
/// preserves a nonzero installed generation; E2 the Session application's
/// canonical projection carries the same revision and manifest together.
/// Constraints: empty inputs do not mean an empty Resource manifest, and the
/// projection must not consult the mutable Skill catalog.
/// Decision rule S1: C1=no, C2=yes, C3=no => (revision 1, exact Skill pin).
#[tokio::test]
async fn skills_only_active_generation_is_preserved_in_the_frozen_projection() {
    // Decision rule: execute S1 and require the exact skills-only revision and
    // manifest pair in the frozen projection.
    let repo: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let application = application(repo, Arc::new(RecordingEnvironmentSource::default()));
    let mut session = persisted("skills-only-projection", false, "idle");
    let resources = skill_resources("design");
    session.resources =
        awaken_session_contract::SessionResourceState::from_active(resources.clone());

    let projection = application
        .frozen_session_projection("workspace".into(), &session, false)
        .await
        .expect("S1 canonical frozen projection");

    assert_eq!(projection.resource_revision, 1, "S1/E1");
    assert_eq!(projection.resources, resources, "S1/E2");
}

/// Rolled-back projection cause/effect graph: C1 generation 1 is active; C2 a
/// generation-2 replacement fails and rolls back; C3 the next Run rebuilds its
/// frozen projection and reconciles the resident Runtime. Effects: E1 durable
/// revision 2 remains only a monotonic watermark; E2 both rebuild paths carry
/// the exact active pair (revision 1, manifest 1); E3 no process-local pseudo
/// generation 2 is published for the strict claimed-Worker fence to reject;
/// C4 no transcript prefix was admitted, so E4 the frozen request context is
/// empty without consulting committed Thread history.
///
/// | Rule | C1 | C2 | C3 | C4 | Frozen pair | Runtime pair | Context |
/// |---|---|---|---|---|---|---|---|
/// | G1 | yes | rollback | yes | absent | (1, manifest 1) | (1, manifest 1) | empty |
#[tokio::test]
async fn rolled_back_resource_generation_recovers_only_the_installed_pair() {
    // Constraint/Invariant: desired-generation watermark never substitutes for
    // the active installed revision/manifest pair. Decision rule: execute G1 and
    // require both frozen and Runtime projections to recover pair (1, manifest 1).
    let runtime = Arc::new(RecordingResourceRuntime::default());
    let repo: Arc<dyn ManagedSessionRepository> =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    let app = application_with_runtime(
        runtime.clone(),
        repo,
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let installed = file_resources("installed");
    let attempted = file_resources("rejected");
    let mut session = persisted("resource-rollback-recovery", false, "idle");
    session
        .resources
        .prepare(&session.session_id, installed.clone())
        .unwrap();
    session.resources.commit().unwrap();
    session
        .resources
        .prepare(&session.session_id, attempted)
        .unwrap();
    session.resources.start_attempt().unwrap();
    session
        .resources
        .rollback("Workdir rejected read-only File")
        .unwrap();
    assert_eq!(session.resources.revision, 2, "G1/E1");

    let projection = app
        .frozen_session_projection("workspace".into(), &session, true)
        .await
        .expect("G1 frozen active projection");
    assert_eq!(projection.resource_revision, 1, "G1/E2");
    assert_eq!(projection.resources, installed, "G1/E2");
    assert!(projection.request_context.is_empty(), "G1/C4/E4");

    app.reconcile_persisted_resources("workspace", session)
        .await
        .expect("G1 active Runtime reconciliation");
    assert_eq!(
        runtime.applied.lock().unwrap().as_slice(),
        [(1, installed)].as_slice(),
        "G1/E2/E3"
    );
}

#[tokio::test]
async fn running_session_persists_manifest_and_applies_only_at_idle_boundary() {
    // Cause/effect graph: C1 Session Running/Idle; C2 valid complete desired
    // manifest; C3 no prior pending attempt. Effects: E1 Running commits pending
    // with attempts=0 and preserves active; E2 the later Idle reconciler fences
    // one attempt and commits pending to active. Decision rules M1
    // Running+C2+C3=>E1; M2 Idle+pending=>E2. FMECA: mutating mounts during a
    // Run can invalidate files observed by that execution (S9/O5/D6); deferring only
    // the effect, not the intent, preserves both execution safety and visibility.
    let repo: Arc<dyn ManagedSessionRepository> =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    create(
        repo.as_ref(),
        persisted("running-manifest", false, "running"),
    )
    .await;
    let app = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let desired = file_resources("safe-boundary");
    let outcome = app
        .replace_session_resource_manifest(
            "running-manifest",
            ReplaceSessionResourceManifest {
                resources: desired.clone(),
                expected_resource_revision: None,
                idempotency_key: Some("manifest-safe-boundary".into()),
                request_fingerprint: "manifest-safe-boundary-hash".into(),
            },
        )
        .await
        .expect("M1 intent accepted");
    assert!(
        outcome.session.resources.active.inputs().is_empty(),
        "M1/E1"
    );
    assert_eq!(
        outcome.session.resources.pending.as_ref(),
        Some(&desired),
        "M1/E1"
    );
    assert!(
        outcome
            .session
            .resources
            .activations
            .iter()
            .all(|activation| activation.attempts == 0),
        "M1/E1"
    );

    let mut idle = outcome.session;
    idle.execution = SessionExecutionState::Idle;
    app.commit_session_snapshot("workspace", idle, "test-safe-boundary", Vec::new())
        .await
        .unwrap();
    let reconciliation = app.reconcile_resource_activations().await;
    assert!(reconciliation.failures.is_empty(), "M2/E2");
    let committed = repo.get("running-manifest").await.unwrap();
    assert!(committed.resources.pending.is_none(), "M2/E2");
    assert_eq!(committed.resources.active, desired, "M2/E2");
}

fn profiled_policy_manifest(
    file: &str,
    skill_version: u64,
) -> awaken_session_contract::ResolvedSessionResources {
    let (inputs, _) = file_resources(file).into_parts();
    let mut skill = skill_resources("review").skills()[0].clone();
    skill.version = skill_version;
    skill.bundle_sha256 = format!("sha-review-{skill_version}");
    awaken_session_contract::ResolvedSessionResources::try_new(inputs, vec![skill]).unwrap()
}

async fn seed_profiled_resource_policy_sessions(repo: Arc<dyn ManagedSessionRepository>) {
    for (id, policy) in [
        (
            "resource-managed",
            awaken_session_contract::SessionMutationPolicy::Managed,
        ),
        (
            "resource-frozen",
            awaken_session_contract::SessionMutationPolicy::Frozen,
        ),
        (
            "resource-files",
            awaken_session_contract::SessionMutationPolicy::FileResources,
        ),
    ] {
        let mut session = persisted(id, false, "idle");
        let awaken_session_contract::SessionBaselineState::Frozen(baseline) = &mut session.baseline
        else {
            unreachable!("fixture baseline is frozen")
        };
        baseline.mutation_policy = policy;
        session.resources = awaken_session_contract::SessionResourceState::from_active(
            profiled_policy_manifest("initial", 1),
        );
        create(repo.as_ref(), session).await;
    }
}

async fn assert_managed_and_frozen_resource_policy_rules(
    app: Arc<SessionApplication>,
    repo: Arc<dyn ManagedSessionRepository>,
) {
    let command = |resources| ReplaceSessionResourceManifest {
        request_fingerprint: awaken_session_contract::stable_fingerprint(&resources),
        resources,
        expected_resource_revision: None,
        idempotency_key: None,
    };

    app.replace_session_resource_manifest(
        "resource-managed",
        command(profiled_policy_manifest("next", 1)),
    )
    .await
    .expect("R1/E1");

    let frozen_before = repo.get("resource-frozen").await.unwrap();
    assert!(
        matches!(
            app.replace_session_resource_manifest(
                "resource-frozen",
                command(profiled_policy_manifest("next", 1)),
            )
            .await,
            Err(SessionResourceManifestError::Rejected(_))
        ),
        "R2/E2"
    );
    assert_eq!(
        repo.get("resource-frozen").await.unwrap(),
        frozen_before,
        "R2/E2"
    );
}

async fn assert_file_resource_and_credential_policy_rules(
    app: Arc<SessionApplication>,
    repo: Arc<dyn ManagedSessionRepository>,
) {
    let command = |resources| ReplaceSessionResourceManifest {
        request_fingerprint: awaken_session_contract::stable_fingerprint(&resources),
        resources,
        expected_resource_revision: None,
        idempotency_key: None,
    };
    app.replace_session_resource_manifest(
        "resource-files",
        command(profiled_policy_manifest("next", 1)),
    )
    .await
    .expect("R3/E3");
    let files_before = repo.get("resource-files").await.unwrap();
    assert!(
        matches!(
            app.replace_session_resource_manifest(
                "resource-files",
                command(profiled_policy_manifest("next", 2)),
            )
            .await,
            Err(SessionResourceManifestError::Rejected(_))
        ),
        "R4/E4"
    );
    assert_eq!(
        repo.get("resource-files").await.unwrap(),
        files_before,
        "R4/E4"
    );

    let rotation = app
        .rotate_repository_credential(
            "resource-files",
            "workspace",
            &awaken_resource_contract::BindingId::from("missing-repository"),
            CredentialMaterialInput::opaque(
                "test",
                awaken_agent_contract::RedactedString::new("never-consumed"),
            ),
        )
        .await
        .expect_err("R5/E5");
    assert!(
        rotation.to_string().contains("credentials are immutable"),
        "R5/E5 policy must precede missing Vault/binding checks: {rotation}"
    );
    assert_eq!(
        repo.get("resource-files").await.unwrap(),
        files_before,
        "R5/E5"
    );
}

#[tokio::test]
async fn profiled_resource_mutation_policy_fences_the_root_before_external_effects() {
    // Cause/effect graph: C1 policy Managed/Frozen/FileResources; C2 complete
    // manifest changes a File; C3 it changes an exact Skill; C4 Repository-token
    // rotation is requested while no Vault ingress is configured. Effects: E1
    // Managed+C2 commits; E2 Frozen rejects with no root change; E3
    // FileResources+C2 commits; E4 FileResources+C3 rejects with no root change;
    // E5 profiled C4 rejects on immutable policy before consulting Vault.
    // Decision rules R1=Managed+C2=>E1, R2=Frozen+C2=>E2,
    // R3=FileResources+C2=>E3, R4=FileResources+C3=>E4,
    // R5=profiled+C4=>E5. Each independent cause partition owns a boxed
    // test-only future so the decision oracles do not share one oversized async
    // frame; production stack limits and state transitions remain unchanged.
    let repo: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("profiled policy repository"),
    );
    Box::pin(seed_profiled_resource_policy_sessions(repo.clone())).await;
    let app = Arc::new(application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));
    Box::pin(assert_managed_and_frozen_resource_policy_rules(
        app.clone(),
        repo.clone(),
    ))
    .await;
    Box::pin(assert_file_resource_and_credential_policy_rules(app, repo)).await;
}

#[tokio::test]
async fn retryable_initial_realization_is_fenced_and_budgeted_durably() {
    // Cause/effect graph: C1 failure retryable/permanent; C2 persisted attempts
    // below/at budget; C3 asserted lease exact/stale. Effects: E1 retryable below
    // budget keeps Preparing and expires the lease; E2 the next assignment
    // advances epoch and attempts; E3 retryable at budget or permanent enters
    // activation_failed; E4 stale delivery mutates nothing. Decision rules
    // covered: R1 retryable+attempt1<2+exact => E1; R2 next exact claim => E2;
    // R3 retryable+attempt2=2+exact => E3; stale ownership is covered by the
    // realization authority table. C4 executable refresh fails before claim;
    // E5 no lease/revision mutation, and successful retry enters R1. Constraint
    // K1 refresh owns no realization state. D1 C4=>E5; D2 !C4=>R1.
    // FMECA: an unbounded retry can strand create
    // forever (S8/O5/D6), while immediate terminal failure loses recoverability
    // (S8/O4/D4); persisted attempts plus lease expiry bound both modes.
    let repo: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(repo.as_ref(), persisted("retry-budget", false, "preparing")).await;
    let refresh = Arc::new(ToggleProjectionRefresh::default());
    refresh.fail.store(true, Ordering::SeqCst);
    let mut app = application_with_configuration(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            realization_retry_budget: 2,
            ..Default::default()
        },
    );
    app.set_executable_projection_refresh(refresh.clone())
        .unwrap();
    let target = |incarnation: &str| awaken_session_contract::SessionRealizationTarget {
        owner: "worker".into(),
        runtime_incarnation: incarnation.into(),
        lease_expires_at_unix_ms: u64::MAX,
        reassign_existing_lease: false,
    };

    let before_refresh_failure = repo.get("retry-budget").await.unwrap();
    assert!(
        matches!(
            app.begin_session_realization(awaken_session_contract::BeginSessionRealization {
                session_id: "retry-budget".into(),
                target: target("worker/refresh-failure"),
            })
            .await,
            Err(awaken_session_contract::SessionRealizationControlFailure::Unavailable(_))
        ),
        "D1/E5"
    );
    assert_eq!(
        repo.get("retry-budget").await.unwrap(),
        before_refresh_failure,
        "D1/E5 no claim mutation"
    );
    refresh.fail.store(false, Ordering::SeqCst);

    let first = app
        .begin_session_realization(awaken_session_contract::BeginSessionRealization {
            session_id: "retry-budget".into(),
            target: target("worker/1"),
        })
        .await
        .expect("R1 begin");
    app.fail_session_realization(awaken_session_contract::FailSessionRealization {
        session_id: "retry-budget".into(),
        lease: first.lease,
        prepared_resource_revision: None,
        retryable: true,
        source_run_id: None,
        reason: "worker unavailable".into(),
    })
    .await
    .expect("R1 retryable failure");
    let after_first = repo.get("retry-budget").await.unwrap();
    assert_eq!(
        after_first.execution,
        SessionExecutionState::Preparing,
        "R1/E1"
    );
    assert_eq!(after_first.realization_progress.attempts, 1, "R1/E1");
    assert_eq!(
        after_first.realization.as_ref().unwrap().expires_at_unix_ms,
        0,
        "R1/E1"
    );

    let second = app
        .begin_session_realization(awaken_session_contract::BeginSessionRealization {
            session_id: "retry-budget".into(),
            target: target("worker/2"),
        })
        .await
        .expect("R2 reclaim");
    assert_eq!(second.lease.epoch, 2, "R2/E2");
    assert_eq!(
        repo.get("retry-budget")
            .await
            .unwrap()
            .realization_progress
            .attempts,
        2,
        "R2/E2"
    );
    app.fail_session_realization(awaken_session_contract::FailSessionRealization {
        session_id: "retry-budget".into(),
        lease: second.lease,
        prepared_resource_revision: None,
        retryable: true,
        source_run_id: None,
        reason: "worker unavailable again".into(),
    })
    .await
    .expect("R3 exhausted");
    assert_eq!(
        repo.get("retry-budget").await.unwrap().execution,
        SessionExecutionState::ActivationFailed,
        "R3/E3"
    );
}

#[tokio::test]
async fn lease_renewal_is_a_projection_free_root_cas() {
    // Renewal cause/effect graph: C1 exact live owner/incarnation/epoch; C2 the
    // requested expiry is newer/equal/older; C3 executable projection refresh
    // is unavailable; C4 asserted epoch is stale. Effects: E1 one root CAS
    // extends only lease expiry; E2 exact replay returns the current lease with
    // no write; E3 malformed/non-monotonic input is rejected; E4 stale fencing
    // is rejected; E5 no catalog refresh, transcript materialization, credential
    // migration, or desired-state phase is entered.
    //
    // | Rule | fence | requested expiry | refresh | Effect |
    // |---|---|---|---|---|
    // | R1 | exact/live | newer | unavailable | E1 + E5 |
    // | R2 | exact/live | equal/current | unavailable | E2 + E5 |
    // | R3 | exact/live | older than assertion | any | E3 |
    // | R4 | stale epoch | newer | any | E4 + E5 |
    let repo: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("renewal Session repository"),
    );
    let asserted = awaken_session_contract::SessionRealizationLease {
        owner: "worker-a".into(),
        runtime_incarnation: "worker-a/boot-1".into(),
        epoch: 7,
        expires_at_unix_ms: u64::MAX - 2,
    };
    let mut session = persisted("lease-only-renewal", true, "idle");
    session.realization = Some(asserted.clone());
    create(repo.as_ref(), session).await;
    let refresh = Arc::new(ToggleProjectionRefresh::default());
    refresh.fail.store(true, Ordering::SeqCst);
    let mut application = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    application
        .set_executable_projection_refresh(refresh.clone())
        .unwrap();

    let renewed = application
        .renew_session_realization(awaken_session_contract::RenewSessionRealization {
            session_id: "lease-only-renewal".into(),
            asserted_lease: asserted.clone(),
            requested_expires_at_unix_ms: u64::MAX - 1,
        })
        .await
        .expect("R1 renewal bypasses projection refresh");
    assert_eq!(renewed.expires_at_unix_ms, u64::MAX - 1, "R1/E1");
    assert_eq!(refresh.calls.load(Ordering::SeqCst), 0, "R1/E5");
    let after_first = repo.get("lease-only-renewal").await.unwrap();

    let replayed = application
        .renew_session_realization(awaken_session_contract::RenewSessionRealization {
            session_id: "lease-only-renewal".into(),
            asserted_lease: asserted.clone(),
            requested_expires_at_unix_ms: u64::MAX - 1,
        })
        .await
        .expect("R2 response-loss replay");
    assert_eq!(replayed, renewed, "R2/E2");
    assert_eq!(
        repo.get("lease-only-renewal").await.unwrap(),
        after_first,
        "R2/E2 no second write"
    );

    let older = application
        .renew_session_realization(awaken_session_contract::RenewSessionRealization {
            session_id: "lease-only-renewal".into(),
            asserted_lease: asserted.clone(),
            requested_expires_at_unix_ms: asserted.expires_at_unix_ms - 1,
        })
        .await;
    assert!(
        matches!(
            older,
            Err(awaken_session_contract::SessionRealizationControlFailure::Invalid(_))
        ),
        "R3/E3"
    );
    let mut stale = asserted;
    stale.epoch -= 1;
    let stale = application
        .renew_session_realization(awaken_session_contract::RenewSessionRealization {
            session_id: "lease-only-renewal".into(),
            asserted_lease: stale,
            requested_expires_at_unix_ms: u64::MAX,
        })
        .await;
    assert_eq!(
        stale,
        Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership),
        "R4/E4"
    );
    assert_eq!(refresh.calls.load(Ordering::SeqCst), 0, "R4/E5");
}

#[tokio::test]
async fn local_realization_uses_stable_owner_and_process_incarnation() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(repo.as_ref(), persisted("local-restart", false, "idle")).await;
    let configured = |owner: &str| SessionApplicationConfiguration {
        execution_placement: SessionExecutionPlacement::LocalWorker,
        local_realization_owner: owner.into(),
        ..Default::default()
    };

    let first = application_with_configuration(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        configured("embedded-worker"),
    );
    let first_session = first.realize_session("local-restart").await.expect("L1");
    let first_lease = first_session.realization.expect("L1 lease");
    assert_eq!(first_lease.owner, "embedded-worker", "L1/E1");
    assert_eq!(
        first
            .realize_session("local-restart")
            .await
            .expect("L1 exact replay")
            .realization
            .expect("L1 replay lease"),
        first_lease,
        "L1/E1"
    );

    let replacement = application_with_configuration(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        configured("embedded-worker"),
    );
    let replaced = replacement
        .realize_session("local-restart")
        .await
        .expect("L2 same logical owner may fence a crashed incarnation");
    let replaced_lease = replaced.realization.expect("L2 lease");
    assert_eq!(replaced_lease.owner, first_lease.owner, "L2/E2");
    assert_ne!(
        replaced_lease.runtime_incarnation, first_lease.runtime_incarnation,
        "L2/E2"
    );
    assert_eq!(replaced_lease.epoch, first_lease.epoch + 1, "L2/E2");

    let other = application_with_configuration(
        repo,
        Arc::new(RecordingEnvironmentSource::default()),
        configured("other-worker"),
    );
    assert!(
        matches!(
            other.realize_session("local-restart").await,
            Err(crate::SessionRealizationError::Control(
                awaken_session_contract::SessionRealizationControlFailure::StaleOwnership
            ))
        ),
        "L3/E3"
    );
    let reassigned = awaken_session_contract::SessionRealizationControl::begin_session_realization(
        &other,
        awaken_session_contract::BeginSessionRealization {
            session_id: "local-restart".into(),
            target: awaken_session_contract::SessionRealizationTarget {
                owner: "other-worker".into(),
                runtime_incarnation: "other-worker/boot-2".into(),
                lease_expires_at_unix_ms: u64::MAX,
                reassign_existing_lease: true,
            },
        },
    )
    .await
    .expect("L4 claim-authorized reassignment");
    assert_eq!(reassigned.lease.epoch, replaced_lease.epoch + 1, "L4/E4");
    assert_eq!(reassigned.lease.owner, "other-worker", "L4/E4");
}

/// Reference-projection FMECA and cause/effect decision table. C1 the
/// Resource index accepts or rejects a replacement; C2 the Session root CAS
/// wins or conflicts after projection. Effects are E1 no durable intent is
/// exposed without its safety edge, E2 a losing candidate is repaired back
/// to repository truth, and E3 a winning pending replacement retains both
/// installed and desired targets until settlement.
///
/// | Rule | C1 index | C2 root CAS | Effect |
/// |---|---|---|---|
/// | R1 | rejects | not attempted | E1 unchanged Session |
/// | R2 | accepts | conflicts | E2 installed target only |
/// | R3 | accepts | wins | E3 installed + desired targets |
#[tokio::test]
async fn resource_reference_projection_fails_closed_and_repairs_cas_losers() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let mut application = application(repo.clone(), environments);
    let references = Arc::new(RecordingReferenceIndex::default());
    application.set_resource_reference_authority(references.clone(), Arc::new(UnusedFileCatalog));

    let mut original = persisted("reference-fence", false, "idle");
    original.resources =
        awaken_session_contract::SessionResourceState::from_active(skill_resources("old"));
    create(repo.as_ref(), original).await;

    let mut rejected = repo.get("reference-fence").await.unwrap();
    let original_revision = rejected.revision;
    rejected
        .resources
        .prepare("reference-fence", skill_resources("new"))
        .unwrap();
    references.fail_replace.store(true, Ordering::SeqCst);
    assert!(
        application
            .commit_resource_snapshot("workspace", rejected, "R1", Vec::new())
            .await
            .is_err(),
        "R1"
    );
    assert_eq!(
        repo.get("reference-fence").await.unwrap().revision,
        original_revision,
        "R1/E1"
    );

    references.fail_replace.store(false, Ordering::SeqCst);
    let mut stale = repo.get("reference-fence").await.unwrap();
    let mut winner = stale.clone();
    winner.title = Some("concurrent winner".into());
    application
        .commit_session_snapshot("workspace", winner, "concurrent", Vec::new())
        .await
        .unwrap();
    stale
        .resources
        .prepare("reference-fence", skill_resources("new"))
        .unwrap();
    assert!(matches!(
        application
            .commit_resource_snapshot("workspace", stale, "R2", Vec::new())
            .await,
        Err(SessionMutationError::Conflict)
    ));
    let ids = || {
        references
            .records
            .lock()
            .unwrap()
            .iter()
            .map(|record| record.target.resource_id.clone())
            .collect::<BTreeSet<_>>()
    };
    assert_eq!(ids(), BTreeSet::from(["old".to_string()]), "R2/E2");

    let mut accepted = repo.get("reference-fence").await.unwrap();
    accepted
        .resources
        .prepare("reference-fence", skill_resources("new"))
        .unwrap();
    application
        .commit_resource_snapshot("workspace", accepted, "R3", Vec::new())
        .await
        .unwrap();
    assert_eq!(
        ids(),
        BTreeSet::from(["new".to_string(), "old".to_string()]),
        "R3/E3"
    );
}

/// Legacy Resource adoption FMECA and cause/effect decision table. C1 the
/// retained manifest has no activation records; C2 the Worker reports that
/// it prepared the exact active revision; C3 the reported revision is stale.
/// Effects are E1 one Active/attempted record is committed, E2 no second
/// physical-state authority is created, and E3 a mismatched generation is
/// rejected without mutating durable truth.
///
/// | Rule | Legacy inputs | Prepared revision | Effect |
/// |---|---|---|---|
/// | L1 | present | exact | E1 adopt once |
/// | L2 | present | stale | E3 reject, unchanged |
/// | L3 | absent/already adopted | any | E2 normal realization path |
#[tokio::test]
async fn remote_realization_adopts_only_the_exact_legacy_resource_generation() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let application = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let lease = awaken_session_contract::SessionRealizationLease {
        owner: "worker-a".into(),
        runtime_incarnation: "worker-a/boot-1".into(),
        epoch: 1,
        expires_at_unix_ms: u64::MAX,
    };
    for id in ["legacy-exact", "legacy-stale"] {
        let mut session = persisted(id, false, "preparing");
        session.resources =
            awaken_session_contract::SessionResourceState::from_active(file_resources(id));
        session.realization = Some(lease.clone());
        create(repo.as_ref(), session).await;
    }

    let exact = awaken_session_contract::SessionRealizationControl::activate_session_realization(
        &application,
        awaken_session_contract::ActivateSessionRealization {
            session_id: "legacy-exact".into(),
            lease: lease.clone(),
            prepared_resource_revision: Some(1),
            mcp_receipts: Vec::new(),
        },
    )
    .await
    .expect("L1 exact legacy generation");
    assert!(
        matches!(
            exact.action,
            awaken_session_contract::SessionRealizationAction::Publish { .. }
        ),
        "L1/E1"
    );
    let adopted = repo.get("legacy-exact").await.expect("L1 durable truth");
    assert_eq!(adopted.resources.activations.len(), 1, "L1/E1");
    assert_eq!(
        adopted.resources.activations[0].state,
        awaken_session_contract::ActivationState::Active,
        "L1/E1"
    );
    assert_eq!(adopted.resources.activations[0].attempts, 1, "L1/E1");

    let stale = awaken_session_contract::SessionRealizationControl::activate_session_realization(
        &application,
        awaken_session_contract::ActivateSessionRealization {
            session_id: "legacy-stale".into(),
            lease,
            prepared_resource_revision: Some(2),
            mcp_receipts: Vec::new(),
        },
    )
    .await
    .expect_err("L2 stale generation must fail closed");
    assert!(
        matches!(
            stale,
            awaken_session_contract::SessionRealizationControlFailure::Invalid(_)
        ),
        "L2/E3: {stale}"
    );
    assert!(
        repo.get("legacy-stale")
            .await
            .expect("L2 durable truth")
            .resources
            .activations
            .is_empty(),
        "L2/E3"
    );
}

/// Terminal Worker-placement FMECA and cause/effect decision table. C1 the
/// Session placement delegates live realization to a Worker; C2 the Session
/// is live with pending work; C3 it is terminal with Releasing work. Effects
/// are E1 no Coordinator execution effect for a live generation and E2 the
/// sole terminal cleanup path converges because no future Run can claim it.
///
/// | Rule | Placement | Lifecycle | Resource state | Effect |
/// |---|---|---|---|---|
/// | W1 | Worker | idle | Prepared | E1 remain unattempted |
/// | W2 | Worker | terminal, current renewal live | Releasing, no preparation | E2 remain pending without Coordinator failure |
/// | W3 | Worker | expired assertion + current renewal | canonical Worker driver | E3 prepare, dispose, release durably |
#[test]
fn worker_placement_defers_live_effects_but_not_terminal_cleanup() {
    // Coverage rationale: the async case below remains the sole W1-W3 oracle.
    // The shared composed-test executor changes stack placement only and adds
    // no realization owner, cleanup queue, or alternate receipt path.
    run_composed_async_test(worker_placement_defers_live_effects_but_not_terminal_cleanup_case);
}

async fn worker_placement_defers_live_effects_but_not_terminal_cleanup_case() {
    // Constraint/Invariant: Worker placement owns live realization, while the
    // Session repository still owns terminal cleanup progress. Decision rule:
    // execute W1-W3 and distinguish deferred live effects, pending cleanup, and
    // exact two-stage driver completion.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let worker_baseline = |id: &str, status: &str| {
        let mut session = persisted(id, false, status);
        let awaken_session_contract::SessionBaselineState::Frozen(baseline) = &mut session.baseline
        else {
            unreachable!("fixture is frozen")
        };
        baseline.runtime_placement = SessionRuntimePlacement::Worker;
        session
    };

    let mut live = worker_baseline("worker-live", "idle");
    live.resources
        .prepare("worker-live", file_resources("live"))
        .expect("W1 prepared generation");
    create(repo.as_ref(), live).await;

    let mut terminal = worker_baseline("worker-terminal", "terminated");
    let asserted_terminal_lease = awaken_session_contract::SessionRealizationLease {
        owner: "worker-terminal-owner".into(),
        runtime_incarnation: "worker-terminal-incarnation".into(),
        epoch: 1,
        expires_at_unix_ms: crate::activity::now_unix_ms().saturating_sub(20_001),
    };
    let terminal_lease = awaken_session_contract::SessionRealizationLease {
        expires_at_unix_ms: crate::activity::now_unix_ms().saturating_add(30_000),
        ..asserted_terminal_lease.clone()
    };
    terminal.realization = Some(terminal_lease.clone());
    terminal.resources =
        awaken_session_contract::SessionResourceState::from_active(file_resources("terminal"));
    terminal.resources.adopt_legacy_active("worker-terminal");
    terminal
        .resources
        .begin_release()
        .expect("W2 release intent");
    create(repo.as_ref(), terminal).await;

    let application = application_with_configuration(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    let report = application.reconcile_resource_activations().await;
    assert!(report.failures.is_empty(), "W2/E2: {:#?}", report.failures);
    assert_eq!(report.settled.len(), 1, "W2/E2 durable pending root");

    let live = repo.get("worker-live").await.expect("W1 durable truth");
    assert!(live.resources.pending.is_some(), "W1/E1");
    assert_eq!(live.resources.activations[0].attempts, 0, "W1/E1");
    let pending = repo.get("worker-terminal").await.expect("W2 durable truth");
    assert!(pending.terminal_cleanup.is_requested(), "W2/E2");
    let work = application
        .terminal_cleanup_work("worker-terminal", &asserted_terminal_lease)
        .await
        .unwrap()
        .expect("W2 terminal fence");
    assert_eq!(work.assignment.lease, terminal_lease, "W2 exact root lease");
    let commands = terminal_prepare_commands(work);
    assert_eq!(commands.len(), 1, "W2/E2");
    assert_eq!(commands[0].thread_id, "worker-terminal", "W2/E2");
    awaken_session_contract::drive_session_terminal_cleanup(
        "worker-terminal",
        &asserted_terminal_lease,
        &application,
        &NoopRuntime,
    )
    .await
    .expect("W3 canonical Worker driver");
    let terminal = repo.get("worker-terminal").await.expect("W3 durable truth");
    assert!(
        terminal.resources.activations.iter().all(
            |activation| activation.state == awaken_session_contract::ActivationState::Released
        ),
        "W3/E3 durable={:?}",
        terminal.resources.activations,
    );
}

#[tokio::test]
async fn remote_terminal_cleanup_uses_canonical_driver_and_cold_completion_replay() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 a terminal Worker-placed Session retains one exact
    // realization lease and may receive a compatible shorter assertion; C2 the
    // frozen target set contains root and child; C3 the external Worker has not
    // driven, drives the canonical two-stage protocol, or replays after the
    // final disposal CAS; C4 the asserted lease is exact or stale. Effects: E1
    // Coordinator performs no Runtime effect and retains the aggregate queue;
    // E2 the Worker sees child-first preparation under the exact assignment;
    // E3 its single driver durably prepares both targets before one disposal;
    // E4 stale authority fails closed; E5 Completed replay performs no effect.
    //
    // | Rule | lease | durable phase | Effect |
    // | W1 | expired assertion + same-generation live current renewal | Requested | E1 + E2 |
    // | W2 | stale | Requested | E4 |
    // | W3 | exact | Requested | E3 |
    // | W4 | exact | Completed | E5 |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("remote cleanup Session repository"),
    );
    let asserted_lease = awaken_session_contract::SessionRealizationLease {
        owner: "worker-a".into(),
        runtime_incarnation: "worker-a:incarnation-1".into(),
        epoch: 7,
        expires_at_unix_ms: 1,
    };
    let lease = awaken_session_contract::SessionRealizationLease {
        expires_at_unix_ms: crate::activity::now_unix_ms().saturating_add(30_000),
        ..asserted_lease.clone()
    };
    let mut session = persisted("remote-cleanup", true, "terminated");
    session.realization = Some(lease.clone());
    create(repo.as_ref(), session).await;
    let runtime = Arc::new(RecordingCleanupRuntime::default());
    *runtime.delegated_snapshot.lock().unwrap() = awaken_session_contract::DelegatedRunSnapshot {
        delegated_runs: Vec::new(),
        coordinated_thread_ids: vec![awaken_agent_contract::agent::thread::Id(
            "remote-cleanup-child".into(),
        )],
        watermark: 31,
        runtime_commit_cursor: 41,
    };
    let application = SessionApplication::new_with_configuration(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );

    let retained = application
        .release_terminal_resources("workspace", "remote-cleanup")
        .await
        .expect("W1 Coordinator leaves external work durable")
        .expect("W1 aggregate remains the Worker queue");
    assert!(retained.terminal_cleanup.is_requested(), "W1/E1");
    assert!(runtime.intents.lock().unwrap().is_empty(), "W1/E1");
    let mut admitted_lease = asserted_lease.clone();
    admitted_lease.expires_at_unix_ms = 0;
    let work = application
        .terminal_cleanup_work("remote-cleanup", &admitted_lease)
        .await
        .expect("W1 exact lease")
        .expect("W1 terminal fence");
    assert_eq!(work.assignment.session_id, "remote-cleanup", "W1/E2");
    assert_eq!(work.assignment.lease, lease, "W1/E2 current lease");
    let commands = terminal_prepare_commands(work);
    assert_eq!(commands.len(), 1, "W1/E2 child phase");
    assert_eq!(commands[0].thread_id, "remote-cleanup-child", "W1/E2");

    let mut stale = admitted_lease;
    stale.epoch += 1;
    assert!(
        matches!(
            application
                .terminal_cleanup_work("remote-cleanup", &stale)
                .await,
            Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "W2/E4"
    );

    let worker_runtime = RecordingCleanupRuntime::default();
    let outcome = awaken_session_contract::drive_session_terminal_cleanup(
        "remote-cleanup",
        &asserted_lease,
        &application,
        &worker_runtime,
    )
    .await
    .expect("W3 canonical Worker driver");
    assert_eq!(
        outcome,
        awaken_session_contract::SessionTerminalCleanupDriveOutcome::Completed,
        "W3/E3"
    );
    let completed = repo
        .get("remote-cleanup")
        .await
        .expect("W3 durable Session");
    assert!(completed.terminal_cleanup.is_completed(), "W3/E3");
    assert_eq!(worker_runtime.intents.lock().unwrap().len(), 2, "W3/E3");
    assert_eq!(
        *worker_runtime.terminal_effect_order.lock().unwrap(),
        vec![
            "cleanup:remote-cleanup-child".to_string(),
            "cleanup:remote-cleanup".to_string(),
            "dispose".to_string(),
        ],
        "W3/E3 prepare child, prepare root, dispose once"
    );
    assert!(runtime.intents.lock().unwrap().is_empty(), "W3/E1");

    awaken_session_contract::drive_session_terminal_cleanup(
        "remote-cleanup",
        &asserted_lease,
        &application,
        &worker_runtime,
    )
    .await
    .expect("W4/E5 exact Completed replay");
    assert_eq!(worker_runtime.intents.lock().unwrap().len(), 2, "W4/E5");
    assert!(runtime.intents.lock().unwrap().is_empty(), "W4/E5");
}

#[tokio::test]
async fn restoring_terminal_cleanup_binds_the_exact_target_only_to_the_root() {
    // Cause/effect graph: C1 the durable Environment is Restoring; C2 the
    // frozen cleanup set contains a child and the root; C3 a preparation
    // command is child/root; C4 a child carries no target or forges the root
    // target. Effects: E1 child-first work remains executable with no restore
    // target; E2 a forged child target is rejected before Runtime I/O; E3 the
    // later root preparation carries the exact durable request; E4 Disposal
    // carries that same request and atomically completes the one cleanup.
    //
    // | Rule | thread | asserted target | Effect |
    // |---|---|---|---|
    // | R1 | child | none | E1 |
    // | R2 | child | root target | E2 |
    // | R3 | root | exact durable target | E3 then E4 |
    //
    // Constraint: child and root are phases of one aggregate operation. The
    // restore request is transport evidence for the root physical target, not
    // another cleanup state machine or a child-owned effect.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("Restoring cleanup Session repository"),
    );
    let session_id = "restoring-cleanup";
    let child_id = "restoring-cleanup-child";
    let lease = awaken_session_contract::SessionRealizationLease {
        owner: "worker-restoring".into(),
        runtime_incarnation: "worker-restoring:incarnation-1".into(),
        epoch: 3,
        expires_at_unix_ms: crate::activity::now_unix_ms().saturating_add(30_000),
    };
    let generation = awaken_session_contract::SandboxGeneration::new(
        session_id,
        1,
        90_000,
        "restoring-environment",
        "restoring-base-image",
    );
    let checkpoint = awaken_session_contract::SandboxCheckpointRef {
        id: "restoring-checkpoint".into(),
        format: "awaken-fs-tar-v1".into(),
        digest: "restoring-checkpoint-digest".into(),
        size_bytes: 7,
        created_at_unix_ms: 1,
        expires_at_unix_ms: 90_000,
        environment_fingerprint: generation.environment_fingerprint.clone(),
        base_image_fingerprint: generation.base_image_fingerprint.clone(),
        excluded_mounts: Vec::new(),
        suspend_effect_id: "restoring-suspend-effect".into(),
    };
    let operation = awaken_session_contract::SessionEnvironmentOperation::new(
        "workspace",
        session_id,
        "restore",
        &generation,
        1,
        Some(lease.clone()),
        Some(&checkpoint),
    );
    let mut session = persisted(session_id, true, "terminated");
    session.realization = Some(lease.clone());
    session.environment = awaken_session_contract::SessionEnvironmentState::Restoring {
        operation,
        checkpoint,
        generation,
    };
    let exact_restore = session
        .environment
        .restoring_request("workspace", session_id)
        .expect("R3 exact durable restore request");
    create(repo.as_ref(), session).await;

    let coordinator_runtime = Arc::new(RecordingCleanupRuntime::default());
    *coordinator_runtime.delegated_snapshot.lock().unwrap() =
        awaken_session_contract::DelegatedRunSnapshot {
            delegated_runs: Vec::new(),
            coordinated_thread_ids: vec![awaken_agent_contract::agent::thread::Id(child_id.into())],
            watermark: 5,
            runtime_commit_cursor: 8,
        };
    let application = SessionApplication::new_with_configuration(
        coordinator_runtime,
        Arc::new(NoopMcpRealizer),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    application
        .release_terminal_resources("workspace", session_id)
        .await
        .expect("R1 terminal intent")
        .expect("R1 aggregate remains pending");

    let work = application
        .terminal_cleanup_work(session_id, &lease)
        .await
        .expect("R1 child work projection")
        .expect("R1 pending child work");
    let child = terminal_prepare_commands(work)
        .into_iter()
        .next()
        .expect("R1 child command");
    assert_eq!(child.thread_id, child_id, "R1/E1 child-first");
    assert_eq!(child.restore_target, None, "R1/E1 root-only evidence");

    let mut forged_child = child.clone();
    forged_child.restore_target = Some(exact_restore.clone());
    let forged_effect =
        awaken_session_contract::SessionTerminalCleanupEffect::new(forged_child, lease.clone());
    assert!(
        matches!(
            application
                .authorize_terminal_cleanup_effect(&forged_effect)
                .await,
            Err(awaken_session_contract::SessionRealizationControlFailure::Invalid(_))
        ),
        "R2/E2"
    );

    let worker_runtime = RecordingCleanupRuntime::default();
    let outcome = awaken_session_contract::drive_session_terminal_cleanup(
        session_id,
        &lease,
        &application,
        &worker_runtime,
    )
    .await
    .expect("R3/R4 canonical cleanup drive");
    assert_eq!(
        outcome,
        awaken_session_contract::SessionTerminalCleanupDriveOutcome::Completed,
        "R3/R4"
    );
    {
        let intents = worker_runtime.intents.lock().unwrap();
        assert_eq!(intents.len(), 2, "R1/R3 child then root");
        assert_eq!(intents[0].thread_id, child_id, "R1/E1");
        assert_eq!(intents[0].restore_target, None, "R1/E1");
        assert_eq!(intents[1].thread_id, session_id, "R3/E3");
        assert_eq!(
            intents[1].restore_target.as_ref(),
            Some(&exact_restore),
            "R3/E3 exact root request"
        );
    }
    let completed = repo.get(session_id).await.expect("R4 durable completion");
    assert!(completed.terminal_cleanup.is_completed(), "R4/E4");
    assert!(
        matches!(
            completed.environment,
            awaken_session_contract::SessionEnvironmentState::Unmaterialized
        ),
        "R4/E4"
    );
}

/// Repository-publication control cause/effect graph. C1 a Requested terminal
/// operation has an exact publication intent; C2 a child preparation is absent
/// or durable; C3 the asserted lease is exact or stale; C4 the publication
/// receipt is mismatched, exact, or an exact response-loss replay; C5 ordinary
/// cleanup commands may be empty while publication is pending; C6 the durable
/// Session row has one immutable Workspace owner; C7 the publication outcome is
/// an exact receipt or typed permanent rejection; C8 each cleanup poll projects
/// its assignment and action from the same current root snapshot. Effects: E1 the
/// child is the sole first preparation; E2 publication remains hidden behind the
/// child and projects `Waiting`; E3 a Waiting publication remains cold-claimable; E4 stale/wrong
/// evidence fails without mutation; E5 the exact receipt is durably replayable;
/// E6 only then is root preparation exposed and the canonical driver performs
/// the one aggregate disposal;
/// E7 the command projection carries that canonical Workspace beside the
/// command so transports need not accept a Worker-selected tenant; E8 an exact
/// rejection is durable and replayable before ordinary root preparation without
/// a second Git command; E9 every cleanup work value carries the current lease
/// and post-outcome root revision. C9 a pending credentialed Repository publication is claimed only by a Worker
/// with the existing Session-Resources and Repository-credential capabilities;
/// C10 its aggregate Environment either names an existing live publication
/// source or would require terminal cleanup to invent/restore one; C11 the
/// asserted terminal timestamp may be expired while the current same-generation
/// root is live, but an expired current root cannot start publication.
///
/// | Rule | child preparation | lease | publication outcome | Effect |
/// |---|---|---|---|---|
/// | P1 | pending | exact | none | E1 + E2 + E9 |
/// | P2 | complete | stale | none | E4 |
/// | P3 | none | expired same-owner Resident source | none, action=Waiting | E3 + E7 + E9 |
/// | P3a | none | unclaimed, credential capability absent | none | E4 |
/// | P3b | none | Unmaterialized source | none | E4, zero root CAS |
/// | P3c | none | asserted expired, current same-generation live | none | E3 + E7 + E9 |
/// | P3d | none | asserted and current expired | none | E4 before projection |
/// | P4 | complete | exact | wrong | E4 |
/// | P5 | complete | exact | exact/replay | E5 + E6 + E9 |
/// | P6 | complete | exact successor of expired Resident affinity | rejection/replay | E8 + E6 + E9 |
fn repository_publication_terminal_session(
    session_id: &str,
    child: Option<&str>,
    lease: Option<awaken_session_contract::SessionRealizationLease>,
    environment: awaken_session_contract::SessionEnvironmentState,
) -> PersistedSession {
    let resources = credentialed_repository_resources("source", "repo-1");
    let intent = repository_publication_intent(&resources);
    let mut session = persisted(session_id, true, "terminated");
    session.environment = environment;
    session.resources = awaken_session_contract::SessionResourceState::from_active(resources);
    session
        .terminal_cleanup
        .request_with_publication(session_id, intent)
        .expect("publication fence");
    session
        .terminal_cleanup
        .freeze_targets(session_id, child.into_iter().map(str::to_string), 17, 19)
        .expect("publication cleanup target");
    session.realization = lease;
    session
}
#[tokio::test]
async fn remote_repository_publication_is_child_gated_lease_fenced_and_replayable() {
    // Constraint: SessionCleanupOperation is the only queue/receipt registry;
    // this matrix therefore observes only its command projections and root CAS.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("Repository publication Session repository"),
    );
    let exact_lease = awaken_session_contract::SessionRealizationLease {
        owner: "worker-a".into(),
        runtime_incarnation: "worker-a:publication".into(),
        epoch: 4,
        expires_at_unix_ms: u64::MAX,
    };
    create(
        repo.as_ref(),
        repository_publication_terminal_session(
            "publication-child-barrier",
            Some("publication-child"),
            Some(exact_lease.clone()),
            awaken_session_contract::SessionEnvironmentState::Resident {
                binding: "publication-child-barrier:publication-source".into(),
                effect_id: None,
                generation: None,
                idle_since_unix_ms: None,
            },
        ),
    )
    .await;
    let expired_cold_lease = awaken_session_contract::SessionRealizationLease {
        owner: "worker-b".into(),
        runtime_incarnation: "worker-b:previous-publication".into(),
        epoch: 2,
        expires_at_unix_ms: 1,
    };
    create(
        repo.as_ref(),
        repository_publication_terminal_session(
            "publication-cold-claim",
            None,
            Some(expired_cold_lease.clone()),
            awaken_session_contract::SessionEnvironmentState::Resident {
                binding: "publication-cold-claim:publication-source".into(),
                effect_id: None,
                generation: None,
                idle_since_unix_ms: None,
            },
        ),
    )
    .await;
    let resources = credentialed_repository_resources("source", "repo-1");
    let mut rejected_intent = repository_publication_intent(&resources);
    rejected_intent.expectation.expected_prior_commit =
        Some("1111111111111111111111111111111111111111".into());
    let mut rejected = persisted("publication-rejected", true, "terminated");
    rejected.environment = awaken_session_contract::SessionEnvironmentState::Resident {
        binding: "publication-rejected:publication-source".into(),
        effect_id: None,
        generation: None,
        idle_since_unix_ms: None,
    };
    rejected.realization = Some(expired_cold_lease.clone());
    rejected.resources = awaken_session_contract::SessionResourceState::from_active(resources);
    rejected
        .terminal_cleanup
        .request_with_publication("publication-rejected", rejected_intent)
        .unwrap();
    rejected
        .terminal_cleanup
        .freeze_targets("publication-rejected", [], 17, 19)
        .unwrap();
    create(repo.as_ref(), rejected).await;
    let application = application_with_configuration(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    assert!(
        matches!(
            application
                .terminal_repository_publication_command(
                    "publication-cold-claim",
                    &expired_cold_lease,
                )
                .await,
            Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "P3d/E4 expired current root cannot project a publication"
    );

    let child_work = application
        .terminal_cleanup_work("publication-child-barrier", &exact_lease)
        .await
        .expect("P1 exact lease")
        .expect("P1 terminal fence");
    assert_eq!(child_work.assignment.lease, exact_lease, "P1 current lease");
    let child_commands = terminal_prepare_commands(child_work);
    assert_eq!(child_commands.len(), 1, "P1/E1");
    assert_eq!(child_commands[0].thread_id, "publication-child", "P1/E1");
    assert!(
        application
            .terminal_repository_publication_command("publication-child-barrier", &exact_lease,)
            .await
            .expect("P1 exact lease")
            .is_none(),
        "P1/E2"
    );
    let child_worker = RecordingCleanupRuntime::default();
    child_worker
        .fail_publication_once
        .store(true, Ordering::SeqCst);
    awaken_session_contract::drive_session_terminal_cleanup(
        "publication-child-barrier",
        &exact_lease,
        &application,
        &child_worker,
    )
    .await
    .expect_err("P2 injected publication failure stops after child preparation");
    assert!(
        matches!(
            application
                .terminal_cleanup_work("publication-child-barrier", &exact_lease)
                .await
                .unwrap()
                .unwrap()
                .action,
            awaken_session_contract::SessionTerminalCleanupAction::Waiting
        ),
        "P2 publication barrier withholds root cleanup"
    );
    let mut stale = exact_lease.clone();
    stale.epoch += 1;
    assert!(
        matches!(
            application
                .terminal_repository_publication_command("publication-child-barrier", &stale,)
                .await,
            Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "P2/E4"
    );

    let target = awaken_session_contract::SessionRealizationTarget {
        owner: "worker-b".into(),
        runtime_incarnation: "worker-b:1:publication".into(),
        lease_expires_at_unix_ms: u64::MAX,
        reassign_existing_lease: false,
    };
    let mut manifest = terminal_cleanup_manifest();
    manifest
        .capabilities
        .insert(awaken_worker_contract::SESSION_RESOURCES_CAPABILITY.to_string());
    let workers = Arc::new(RecordingWorkerObservations::default());
    workers.replace(vec![terminal_cleanup_worker(&target, manifest.clone())]);
    application
        .set_worker_observation_source(workers.clone())
        .expect("install publication Worker observations");
    assert!(
        application
            .claim_next_terminal_cleanup(target.clone())
            .await
            .expect("P3a incompatible Worker is skipped")
            .is_none(),
        "P3a/E4 missing Repository credential capability"
    );
    manifest
        .capabilities
        .insert(awaken_worker_contract::REPOSITORY_CREDENTIALS_CAPABILITY.to_string());
    workers.replace(vec![terminal_cleanup_worker(&target, manifest)]);
    let assignment = application
        .claim_next_terminal_cleanup(target.clone())
        .await
        .expect("P3 claim")
        .expect("P3 publication-only assignment");
    assert_eq!(assignment.session_id, "publication-cold-claim", "P3/E3");
    assert!(
        matches!(
            application
                .terminal_cleanup_work(&assignment.session_id, &assignment.lease)
                .await
                .unwrap()
                .unwrap()
                .action,
            awaken_session_contract::SessionTerminalCleanupAction::Waiting
        ),
        "P3/E3"
    );
    let expired_assertion = awaken_session_contract::SessionRealizationLease {
        expires_at_unix_ms: 1,
        ..assignment.lease.clone()
    };
    let publication = application
        .terminal_repository_publication_command(&assignment.session_id, &expired_assertion)
        .await
        .expect("P3c expired assertion publication poll")
        .expect("P3c publication projection");
    assert_eq!(publication.workspace_id, "workspace", "P3c/E7");
    assert_eq!(
        publication.current_lease, assignment.lease,
        "P3c/E8 projection carries the current live same-generation root"
    );
    let publication = publication.command;
    let awaken_session_contract::ResolvedInputSource::Repository {
        repository_id,
        config,
        ..
    } = &publication.intent.input.source
    else {
        panic!("publication fixture is a Repository")
    };
    let wrong = awaken_session_contract::SessionRepositoryPublicationReceipt::new(
        &publication,
        awaken_provisioning_contract::RepositoryPublicationReceipt {
            repository_id: repository_id.to_string(),
            source_remote_url: config.remote_url.clone(),
            branch: "wrong-branch".into(),
            commit: publication.intent.expectation.commit.clone(),
        },
    );
    assert!(
        matches!(
            application
                .record_terminal_repository_publication_receipt(
                    &assignment.session_id,
                    &assignment.lease,
                    wrong,
                )
                .await,
            Err(awaken_session_contract::SessionRealizationControlFailure::Invalid(_))
        ),
        "P4/E4"
    );
    assert!(
        repo.get(&assignment.session_id)
            .await
            .unwrap()
            .terminal_cleanup
            .repository_publication_receipt(&assignment.session_id)
            .unwrap()
            .is_none(),
        "P4/E4 no mutation"
    );

    let exact = awaken_session_contract::SessionRepositoryPublicationReceipt::new(
        &publication,
        awaken_provisioning_contract::RepositoryPublicationReceipt {
            repository_id: repository_id.to_string(),
            source_remote_url: config.remote_url.clone(),
            branch: publication.intent.expectation.branch.clone(),
            commit: publication.intent.expectation.commit.clone(),
        },
    );
    application
        .record_terminal_repository_publication_receipt(
            &assignment.session_id,
            &assignment.lease,
            exact.clone(),
        )
        .await
        .expect("P5 exact receipt");
    application
        .record_terminal_repository_publication_receipt(
            &assignment.session_id,
            &assignment.lease,
            exact.clone(),
        )
        .await
        .expect("P5 response-loss replay");
    assert_eq!(
        repo.get(&assignment.session_id)
            .await
            .unwrap()
            .terminal_cleanup
            .repository_publication_receipt(&assignment.session_id)
            .unwrap(),
        Some(&exact),
        "P5/E5"
    );
    assert!(
        application
            .terminal_repository_publication_command(&assignment.session_id, &assignment.lease)
            .await
            .unwrap()
            .is_none(),
        "P5/E6"
    );
    let root_work = application
        .terminal_cleanup_work(&assignment.session_id, &assignment.lease)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        root_work.assignment.session_id, assignment.session_id,
        "P5 current assignment Session"
    );
    assert_eq!(
        root_work.assignment.lease, assignment.lease,
        "P5 current assignment generation"
    );
    assert!(
        root_work.assignment.projection.revision.0 > assignment.projection.revision.0,
        "P5 work refreshes the frozen root snapshot after publication"
    );
    let root = terminal_prepare_commands(root_work);
    assert_eq!(root.len(), 1, "P5/E6");
    assert_eq!(root[0].thread_id, assignment.session_id, "P5/E6");
    let root_worker = RecordingCleanupRuntime::default();
    awaken_session_contract::drive_session_terminal_cleanup(
        &assignment.session_id,
        &assignment.lease,
        &application,
        &root_worker,
    )
    .await
    .expect("P5 root preparation and aggregate disposal");
    assert!(
        repo.get(&assignment.session_id)
            .await
            .unwrap()
            .terminal_cleanup
            .is_completed(),
        "P5/E6"
    );
    let rejected_assignment = application
        .claim_next_terminal_cleanup(target.clone())
        .await
        .expect("P6 claim")
        .expect("P6 rejected publication assignment");
    assert_eq!(
        rejected_assignment.session_id, "publication-rejected",
        "P6 exact Session"
    );
    let projection = application
        .terminal_repository_publication_command(
            &rejected_assignment.session_id,
            &rejected_assignment.lease,
        )
        .await
        .unwrap()
        .expect("P6 command");
    let rejection = awaken_session_contract::SessionRepositoryPublicationRejection::new(
        &projection.command,
        awaken_provisioning_contract::RepositoryPublicationRejection::RemoteRefAbsent,
    )
    .unwrap();
    application
        .record_terminal_repository_publication_rejection(
            &rejected_assignment.session_id,
            &rejected_assignment.lease,
            rejection.clone(),
        )
        .await
        .expect("P6 exact rejection");
    application
        .record_terminal_repository_publication_rejection(
            &rejected_assignment.session_id,
            &rejected_assignment.lease,
            rejection.clone(),
        )
        .await
        .expect("P6 response-loss replay");
    assert!(
        application
            .terminal_repository_publication_command(
                &rejected_assignment.session_id,
                &rejected_assignment.lease,
            )
            .await
            .unwrap()
            .is_none(),
        "P6/E8 no second Git command"
    );
    let root_work = application
        .terminal_cleanup_work(&rejected_assignment.session_id, &rejected_assignment.lease)
        .await
        .unwrap()
        .expect("P6 ordinary root work");
    assert!(
        root_work.assignment.projection.revision.0 > rejected_assignment.projection.revision.0,
        "P6/E9 work refreshes the root after the rejection CAS"
    );
    let root = terminal_prepare_commands(root_work);
    assert_eq!(root.len(), 1, "P6 ordinary root preparation is exposed");
    assert_eq!(
        root[0].thread_id, rejected_assignment.session_id,
        "P6 root preparation"
    );
    let rejected_worker = RecordingCleanupRuntime::default();
    awaken_session_contract::drive_session_terminal_cleanup(
        &rejected_assignment.session_id,
        &rejected_assignment.lease,
        &application,
        &rejected_worker,
    )
    .await
    .expect("P6 root preparation and aggregate disposal");
    let durable = repo.get("publication-rejected").await.unwrap();
    assert!(durable.terminal_cleanup.is_completed(), "P6/E6 completed");
    assert_eq!(
        durable
            .terminal_cleanup
            .repository_publication_rejection("publication-rejected")
            .unwrap(),
        Some(&rejection),
        "P6/E8 rejection survives the same root CAS lifecycle"
    );
    let invalid_source = repository_publication_terminal_session(
        "publication-unmaterialized",
        None,
        None,
        awaken_session_contract::SessionEnvironmentState::Unmaterialized,
    );
    create(repo.as_ref(), invalid_source).await;
    assert!(
        matches!(
            application.claim_next_terminal_cleanup(target).await,
            Err(awaken_session_contract::SessionRealizationControlFailure::Invalid(_))
        ),
        "P3b/E4 an Unmaterialized Session cannot acquire a publication lease"
    );
    assert!(
        repo.get("publication-unmaterialized")
            .await
            .unwrap()
            .realization
            .is_none(),
        "P3b/E4 invalid source admission performs zero root mutation"
    );
}

#[tokio::test]
async fn cold_terminal_cleanup_claim_pages_past_a_saturated_invalid_batch() {
    // Cause/effect graph: C1 the reconciliation index begins with one complete
    // fixed-size page of decode-valid terminal rows; C2 every row in that page
    // has a pending Repository publication but no canonical live source; C3 a
    // healthy Requested terminal row sorts immediately after the saturated
    // page; C4 the authenticated Worker is eligible for the healthy action;
    // C5 the healthy row's root CAS commits but its response is lost once.
    // Effects: E1 each invalid row fails closed with zero root mutation; E2 the
    // scan advances only by the repository's typed keyset cursor; E3 the same
    // claim call reaches and fences the healthy row; E4 no unbounded query,
    // quarantine track, or ignore-list becomes a second recovery authority;
    // E5 the retry starts from canonical readback and returns the same epoch.
    //
    // | Rule | first page | next row | Worker | Effect |
    // |---|---|---|---|---|
    // | H1 | fewer than 256 invalid | any | eligible | existing in-page scan |
    // | H2 | exactly 256 invalid | healthy + one lost CAS response | eligible | E1 + E2 + E3 + E4 + E5 |
    // | H3 | exactly 256 invalid | absent | eligible | first Invalid after all pages |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("paged cleanup Session repository"),
    );
    for index in 0..256 {
        let session_id = format!("hol-a-invalid-{index:03}");
        create(
            repo.as_ref(),
            repository_publication_terminal_session(
                &session_id,
                None,
                None,
                awaken_session_contract::SessionEnvironmentState::Unmaterialized,
            ),
        )
        .await;
    }
    create(
        repo.as_ref(),
        registry_race_terminal_session("hol-z-healthy", None),
    )
    .await;

    let target = registry_race_target();
    let repository: Arc<dyn ManagedSessionRepository> = repo.clone();
    let faulting = Arc::new(FaultingSessionRepository::new(repository));
    faulting.commit_then_conflict_once("claim-terminal-cleanup-recovery");
    let application = application_with_configuration(
        faulting,
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    let workers = Arc::new(RecordingWorkerObservations::default());
    workers.replace(vec![terminal_cleanup_worker(
        &target,
        terminal_cleanup_manifest(),
    )]);
    application
        .set_worker_observation_source(workers.clone())
        .expect("install the one Worker observation authority");

    let assignment = application
        .claim_next_terminal_cleanup(target)
        .await
        .expect("H2 typed pagination skips the saturated invalid page")
        .expect("H2 healthy row remains visible");
    assert_eq!(assignment.session_id, "hol-z-healthy", "H2/E2/E3");
    assert_eq!(assignment.lease.epoch, 1, "H2/E5 exact CAS readback");
    assert!(
        workers.list_calls.load(Ordering::SeqCst) >= 2,
        "H2/E5 retries revalidate the Worker before traversing the keyset again"
    );
    assert!(
        repo.get("hol-a-invalid-000")
            .await
            .unwrap()
            .realization
            .is_none(),
        "H2/E1 invalid rows remain mutation-free"
    );
    assert!(
        matches!(
            application
                .claim_next_terminal_cleanup(registry_race_target())
                .await,
            Err(awaken_session_contract::SessionRealizationControlFailure::Invalid(_))
        ),
        "H3 returns the first fail-closed projection error only after every page has no claimable row"
    );
}

#[tokio::test]
async fn cold_terminal_cleanup_claim_uses_the_existing_scan_and_fences_one_assignment() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 placement is Worker-owned; C2 cleanup has an
    // immutable Requested command; C3 the realization lease is absent, expired
    // under the same logical Worker installation, exact-current with a strictly
    // older or equal expiry, live under that owner's predecessor incarnation,
    // or belongs to a foreign logical Worker; C4 the authenticated registry
    // snapshot is current and explicitly supports the closed cleanup protocol;
    // C5 the root CAS response is lost after commit. Effects: E1 an unbound,
    // expired same-owner, or exact-current strictly older lease allocates epoch
    // N+1 and returns the frozen projection/lease; E2 foreign, predecessor,
    // ineligible, Fenced, nonterminal, and local rows perform zero root CAS; E3
    // exact-current equal expiry is skipped so one unavailable candidate cannot
    // starve the scan; E4 this invocation replays its exact committed assignment
    // after an ambiguous CAS response. Commands remain exclusively in
    // terminal_cleanup, and Worker compatibility is an AND gate, never authority
    // to cross the provider's stable installation namespace.
    //
    // | Rule | C1+C2 | lease | Effect |
    // | C1 | yes | same owner, predecessor incarnation, live | E2 |
    // | C2 | yes | absent | E1 |
    // | C3 | yes | expired same logical owner | E1 after C2 is current/skipped |
    // | C4 | yes | exact current, strictly older expiry | E1 once per heartbeat target |
    // | C5 | yes | exact current, equal expiry | E3 |
    // | C6 | yes | live or expired foreign owner | E2; immutable provider affinity |
    // | C7 | no / Fenced / unauthenticated or legacy Worker | any | E2 |
    // | C8 | yes + ambiguous committed CAS | exact attempted | E4 |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("cold cleanup Session repository"),
    );
    let target = awaken_session_contract::SessionRealizationTarget {
        owner: "worker-a".into(),
        runtime_incarnation: "worker-a:2:boot-new".into(),
        lease_expires_at_unix_ms: u64::MAX,
        reassign_existing_lease: false,
    };
    let lease = |owner: &str, incarnation: &str, epoch: u64, expiry: u64| {
        awaken_session_contract::SessionRealizationLease {
            owner: owner.into(),
            runtime_incarnation: incarnation.into(),
            epoch,
            expires_at_unix_ms: expiry,
        }
    };
    let requested = |id: &str, current| {
        let mut session = persisted(id, true, "terminated");
        assert!(session.terminal_cleanup.request(id));
        session
            .terminal_cleanup
            .freeze_targets(id, [], 0, 0)
            .expect("freeze root cleanup target");
        session.realization = current;
        session
    };

    create(
        repo.as_ref(),
        requested(
            "claim-a-current",
            Some(lease("worker-a", "worker-a:2:boot-new", 4, u64::MAX)),
        ),
    )
    .await;
    create(
        repo.as_ref(),
        requested(
            "claim-b-foreign-live",
            Some(lease("worker-b", "worker-b:1:boot", 5, u64::MAX)),
        ),
    )
    .await;
    create(
        repo.as_ref(),
        requested(
            "claim-b0-current-renewable",
            Some(lease("worker-a", "worker-a:2:boot-new", 4, u64::MAX - 1)),
        ),
    )
    .await;
    create(
        repo.as_ref(),
        requested(
            "claim-c-predecessor",
            Some(lease("worker-a", "worker-a:1:boot-old", 7, u64::MAX)),
        ),
    )
    .await;
    create(repo.as_ref(), requested("claim-d-absent", None)).await;
    create(
        repo.as_ref(),
        requested(
            "claim-e-expired",
            Some(lease("worker-a", "worker-a:1:boot-old", 9, 1)),
        ),
    )
    .await;
    create(
        repo.as_ref(),
        requested(
            "claim-f-expired-foreign",
            Some(lease("worker-b", "worker-b:1:boot", 11, 1)),
        ),
    )
    .await;
    let mut fenced = persisted("claim-g-fenced", true, "terminated");
    assert!(fenced.terminal_cleanup.request("claim-g-fenced"));
    create(repo.as_ref(), fenced).await;
    let mut local = requested("claim-h-local", None);
    let awaken_session_contract::SessionBaselineState::Frozen(baseline) = &mut local.baseline
    else {
        unreachable!("fixture is frozen")
    };
    baseline.runtime_placement = SessionRuntimePlacement::Local;
    create(repo.as_ref(), local).await;
    create(
        repo.as_ref(),
        persisted("claim-i-nonterminal", true, "idle"),
    )
    .await;

    let application = application_with_configuration(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    let workers = Arc::new(RecordingWorkerObservations::default());
    workers.replace(vec![terminal_cleanup_worker(
        &target,
        terminal_cleanup_manifest(),
    )]);
    application
        .set_worker_observation_source(workers.clone())
        .expect("install the one Worker observation authority");
    for (rule, expected_id, expected_epoch) in [
        ("C4", "claim-b0-current-renewable", 5),
        ("C2", "claim-d-absent", 1),
        ("C3", "claim-e-expired", 10),
    ] {
        let assignment = application
            .claim_next_terminal_cleanup(target.clone())
            .await
            .expect(rule)
            .expect(rule);
        assert_eq!(assignment.session_id, expected_id, "{rule}/E1");
        assert_eq!(assignment.lease.epoch, expected_epoch, "{rule}/E1");
        assert_eq!(assignment.lease.owner, target.owner, "{rule}/E1");
        assert_eq!(
            assignment.lease.runtime_incarnation, target.runtime_incarnation,
            "{rule}/E1"
        );
        assert_eq!(
            assignment.projection.baseline.agent_id, "agent",
            "{rule}/E1"
        );
    }
    assert!(
        application
            .claim_next_terminal_cleanup(target.clone())
            .await
            .expect("C1+C5-C7")
            .is_none(),
        "C1+C5-C7/E2+E3"
    );
    assert_eq!(
        repo.get("claim-a-current").await.unwrap().realization,
        Some(lease("worker-a", "worker-a:2:boot-new", 4, u64::MAX)),
        "C5/E3 equal expiry performs zero root mutation"
    );
    assert_eq!(
        repo.get("claim-c-predecessor").await.unwrap().realization,
        Some(lease("worker-a", "worker-a:1:boot-old", 7, u64::MAX)),
        "C1/E2 live predecessor affinity performs zero root mutation"
    );
    let foreign = repo.get("claim-f-expired-foreign").await.unwrap();
    assert_eq!(
        foreign.realization,
        Some(lease("worker-b", "worker-b:1:boot", 11, 1)),
        "C6/E2 foreign provider installation remains authoritative"
    );
    let invalid = application
        .claim_next_terminal_cleanup(awaken_session_contract::SessionRealizationTarget {
            reassign_existing_lease: true,
            ..target.clone()
        })
        .await;
    assert!(
        matches!(
            invalid,
            Err(awaken_session_contract::SessionRealizationControlFailure::Invalid(_))
        ),
        "claim-next never doubles as live reassignment"
    );

    let inner: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("ambiguous cleanup Session repository"),
    );
    create(inner.as_ref(), requested("claim-conflict", None)).await;
    let faulting = Arc::new(FaultingSessionRepository::new(inner.clone()));
    faulting.commit_then_conflict_once("claim-terminal-cleanup-recovery");
    let application = application_with_configuration(
        faulting,
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    let workers = Arc::new(RecordingWorkerObservations::default());
    workers.replace(vec![terminal_cleanup_worker(
        &target,
        terminal_cleanup_manifest(),
    )]);
    application
        .set_worker_observation_source(workers.clone())
        .expect("install the one Worker observation authority");
    let replayed = application
        .claim_next_terminal_cleanup(target)
        .await
        .expect("C8 ambiguous CAS replay")
        .expect("C8 assignment");
    assert_eq!(replayed.session_id, "claim-conflict", "C8/E4");
    assert_eq!(replayed.lease.epoch, 1, "C8/E4 does not allocate twice");
    assert!(
        workers.list_calls.load(Ordering::SeqCst) >= 2,
        "C8 recomputes Worker eligibility after the root CAS conflict"
    );
}

fn registry_race_terminal_session(
    session_id: &str,
    realization: Option<awaken_session_contract::SessionRealizationLease>,
) -> PersistedSession {
    let mut session = persisted(session_id, true, "terminated");
    assert!(session.terminal_cleanup.request(session_id));
    session
        .terminal_cleanup
        .freeze_targets(session_id, [], 0, 0)
        .expect("freeze Registry-race cleanup target");
    session.realization = realization;
    session
}

fn registry_race_target() -> awaken_session_contract::SessionRealizationTarget {
    awaken_session_contract::SessionRealizationTarget {
        owner: "worker-registry-race".into(),
        runtime_incarnation: "worker-registry-race:2:boot-current".into(),
        lease_expires_at_unix_ms: u64::MAX,
        reassign_existing_lease: false,
    }
}

/// Terminal effect authorization cause/effect graph: C1 the asserted effect
/// lease expired more than 20 seconds ago; C2 the current Session root is the
/// same owner/incarnation/epoch and is either renewed/live or still expired;
/// C3 the asserted epoch is exact or a foreign successor. Effects: E1 an old
/// assertion may start cleanup preparation, terminal Memory, and physical
/// disposal only through its live same-generation current renewal; E2 an
/// unrenewed current root is stale before any new effect or Workspace/provider
/// authority is returned; E3 a foreign epoch remains stale even while the
/// current predecessor is live. Control's current root is the only lease clock;
/// receipt recording remains generation-based so an admitted long preparation
/// may finish after ordinary expiry and durably expose the later disposal.
///
/// | Rule | asserted | current root | Authorized effects |
/// |---|---|---|---|
/// | A1 | exact, expired >20s | same generation renewed/live | E1: preparation + Memory + disposal |
/// | A2 | exact, expired >20s | same generation expired | E2: none; no Workspace/provider authority |
/// | A3 | foreign epoch | predecessor renewed/live | E3: none |
#[tokio::test]
async fn terminal_effect_authorization_requires_current_live_same_generation() {
    let now = crate::activity::now_unix_ms();
    let asserted = awaken_session_contract::SessionRealizationLease {
        owner: "worker-terminal-authority".into(),
        runtime_incarnation: "worker-terminal-authority:2:boot-current".into(),
        epoch: 29,
        expires_at_unix_ms: now.saturating_sub(20_001),
    };
    let renewed = awaken_session_contract::SessionRealizationLease {
        expires_at_unix_ms: now.saturating_add(30_000),
        ..asserted.clone()
    };
    let memory_input = awaken_session_contract::ResolvedInput {
        binding_id: awaken_resource_contract::BindingId::from("terminal-memory"),
        source: awaken_session_contract::ResolvedInputSource::MemoryStore {
            memory_store_id: awaken_resource_contract::MemoryStoreId::from(
                "terminal-authority-memory",
            ),
            config: awaken_resource_contract::MemoryStoreConfigVersion {
                memory_store_id: awaken_resource_contract::MemoryStoreId::from(
                    "terminal-authority-memory",
                ),
                version: awaken_resource_contract::ConfigVersion::INITIAL,
                retention_policy: Default::default(),
            },
        },
        mount_path: "/memory/terminal-authority".into(),
        access: awaken_resource_contract::ResourceAccess::ReadWrite,
        instructions: None,
    };
    let materialization = awaken_provisioning_contract::MemoryMaterializationEvidence::new(
        "terminal-authority-memory",
        memory_input.mount_path.clone(),
        Vec::new(),
    )
    .expect("terminal authority Memory evidence");
    let terminal_session =
        |session_id: &str,
         realization: Option<awaken_session_contract::SessionRealizationLease>| {
            let mut session = registry_race_terminal_session(session_id, realization);
            session.resources = awaken_session_contract::SessionResourceState::from_active(
                awaken_session_contract::ResolvedSessionResources::try_new(
                    vec![memory_input.clone()],
                    Vec::new(),
                )
                .expect("terminal authority Memory input"),
            );
            let realization_fingerprint =
                awaken_provisioning_contract::SandboxRealizationFingerprint::from_spec(
                    &awaken_provisioning_contract::SandboxSpec {
                        scope: session_id.into(),
                        isolation: awaken_provisioning_contract::IsolationClass::Workdir,
                        environment: None,
                        command: Vec::new(),
                        deny_tool_egress: false,
                        mounts: Vec::new(),
                        env: Vec::new(),
                        packages: Default::default(),
                        network: awaken_provisioning_contract::NetworkPolicy::Unrestricted,
                        outputs_path: "/mnt/session/outputs".into(),
                        requests: Default::default(),
                        limits: Default::default(),
                        filesystem_continuity: Default::default(),
                        control_services: Default::default(),
                        lease_ttl_secs: None,
                    },
                );
            let handle = awaken_provisioning_contract::SandboxHandle::local_v2(
                session_id,
                awaken_provisioning_contract::LocalSandboxHandleV2 {
                    previous: awaken_provisioning_contract::LocalSandboxHandleV1 {
                        outputs_path: "/mnt/session/outputs".into(),
                        base_env: Vec::new(),
                        continuation_excluded_paths: Vec::new(),
                        deny_tool_egress: false,
                    },
                    realization_fingerprint,
                    effect_fence: asserted
                        .sandbox_effect_fence("create-terminal-authority-memory")
                        .expect("terminal authority Memory creation fence"),
                    physical_incarnation: format!("{session_id}:physical"),
                    owned_paths: vec![memory_input.mount_path.clone()],
                },
            )
            .with_memory_materializations(vec![materialization.clone()])
            .expect("terminal authority durable Memory evidence");
            session.environment = awaken_session_contract::SessionEnvironmentState::Resident {
                binding: serde_json::to_string(&handle)
                    .expect("terminal authority Environment binding"),
                effect_id: None,
                generation: None,
                idle_since_unix_ms: None,
            };
            session
        };
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("terminal effect authority Session repository"),
    );
    create(
        repo.as_ref(),
        terminal_session("terminal-authority-renewed", Some(renewed)),
    )
    .await;
    create(
        repo.as_ref(),
        terminal_session("terminal-authority-expired", Some(asserted.clone())),
    )
    .await;
    let application = application(repo, Arc::new(RecordingEnvironmentSource::default()));

    let effect_for =
        |session_id: &str,
         work: awaken_session_contract::SessionTerminalCleanupWork,
         lease: awaken_session_contract::SessionRealizationLease| {
            awaken_session_contract::SessionTerminalCleanupEffect::new(
                terminal_prepare_commands(work)
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| panic!("{session_id} root preparation command")),
                lease,
            )
        };
    let renewed_work = application
        .terminal_cleanup_work("terminal-authority-renewed", &asserted)
        .await
        .expect("A1 exact generation")
        .expect("A1 pending work");
    let renewed_effect = effect_for("terminal-authority-renewed", renewed_work, asserted.clone());
    application
        .authorize_terminal_cleanup_effect(&renewed_effect)
        .await
        .expect("A1/E1 old assertion uses current renewal");
    let renewed_memory = awaken_session_contract::terminal_memory_reconciliation_intent(
        &memory_input,
        &materialization,
        &renewed_effect,
    )
    .expect("A1 exact terminal Memory intent");
    application
        .authorize_terminal_memory_intent(&renewed_memory)
        .await
        .expect("A1/E1 old Memory assertion uses current renewal");
    let renewed_preparation = awaken_session_contract::SessionCleanupPreparation::try_new(
        &renewed_effect,
        renewed_effect
            .sandbox_effect_fence()
            .expect("A1 terminal preparation fence"),
        Vec::new(),
    )
    .expect("A1 terminal preparation");
    application
        .record_terminal_cleanup_preparation(&asserted, renewed_preparation)
        .await
        .expect("A1 admitted preparation remains recordable");
    let renewed_disposal = match application
        .terminal_cleanup_work("terminal-authority-renewed", &asserted)
        .await
        .expect("A1 exact disposal generation")
        .expect("A1 pending disposal")
        .action
    {
        awaken_session_contract::SessionTerminalCleanupAction::Dispose { command } => {
            awaken_session_contract::SessionTerminalCleanupDisposalEffect::new(
                command,
                asserted.clone(),
            )
        }
        action => panic!("A1 expected disposal action, got {action:?}"),
    };
    assert_eq!(
        application
            .authorize_terminal_cleanup_disposal(&renewed_disposal)
            .await
            .expect("A1/E1 old disposal assertion uses current renewal"),
        "workspace",
        "A1/E1 disposal Workspace authority"
    );

    let expired_work = application
        .terminal_cleanup_work("terminal-authority-expired", &asserted)
        .await
        .expect("A2 exact generation")
        .expect("A2 pending work");
    let expired_effect = effect_for("terminal-authority-expired", expired_work, asserted.clone());
    assert!(
        matches!(
            application
                .authorize_terminal_cleanup_effect(&expired_effect)
                .await,
            Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "A2/E2"
    );
    let expired_memory = awaken_session_contract::terminal_memory_reconciliation_intent(
        &memory_input,
        &materialization,
        &expired_effect,
    )
    .expect("A2 exact terminal Memory intent");
    assert!(
        matches!(
            application
                .authorize_terminal_memory_intent(&expired_memory)
                .await,
            Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "A2/E2 terminal Memory"
    );
    let expired_preparation = awaken_session_contract::SessionCleanupPreparation::try_new(
        &expired_effect,
        expired_effect
            .sandbox_effect_fence()
            .expect("A2 terminal preparation fence"),
        Vec::new(),
    )
    .expect("A2 terminal preparation");
    application
        .record_terminal_cleanup_preparation(&asserted, expired_preparation)
        .await
        .expect("A2 admitted preparation receipt remains generation-based");
    let expired_disposal = match application
        .terminal_cleanup_work("terminal-authority-expired", &asserted)
        .await
        .expect("A2 exact disposal generation")
        .expect("A2 pending disposal")
        .action
    {
        awaken_session_contract::SessionTerminalCleanupAction::Dispose { command } => {
            awaken_session_contract::SessionTerminalCleanupDisposalEffect::new(
                command,
                asserted.clone(),
            )
        }
        action => panic!("A2 expected disposal action, got {action:?}"),
    };
    assert!(
        matches!(
            application
                .authorize_terminal_cleanup_disposal(&expired_disposal)
                .await,
            Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "A2/E2 terminal disposal returns no Workspace/provider authority"
    );

    let foreign_effect = awaken_session_contract::SessionTerminalCleanupEffect::new(
        renewed_effect.command.clone(),
        awaken_session_contract::SessionRealizationLease {
            epoch: asserted.epoch + 1,
            ..asserted.clone()
        },
    );
    assert!(
        matches!(
            application
                .authorize_terminal_cleanup_effect(&foreign_effect)
                .await,
            Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "A3/E3"
    );
    let foreign_memory = awaken_session_contract::terminal_memory_reconciliation_intent(
        &memory_input,
        &materialization,
        &foreign_effect,
    )
    .expect("A3 foreign terminal Memory intent");
    assert!(
        matches!(
            application
                .authorize_terminal_memory_intent(&foreign_memory)
                .await,
            Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "A3/E3 terminal Memory"
    );
    let foreign_disposal = awaken_session_contract::SessionTerminalCleanupDisposalEffect::new(
        renewed_disposal.command,
        foreign_effect.lease,
    );
    assert!(
        matches!(
            application
                .authorize_terminal_cleanup_disposal(&foreign_disposal)
                .await,
            Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "A3/E3 terminal disposal"
    );
}

/// Registry/root-CAS race cause/effect graph: C1 the exact Worker incarnation
/// is valid before the recovery scan; C2 the canonical Worker observation at
/// the CAS boundary is exact, revoked, or unavailable; C3 the root CAS wins;
/// C4 the exact post-CAS readback remains valid or is revoked; C5 a concurrent
/// root mutation replaces the just-minted realization before compensation.
/// Effects: E1 only current C2 authority may enter root CAS; E2 an unavailable
/// C2 read returns unavailable with the prior root unchanged; E3 revoked C4
/// authority restores the exact prior realization; E4 compensation never
/// overwrites a concurrent successor. Both reads use the installed
/// `WorkerObservationSource`; Session root CAS remains the sole lease writer.
///
/// | Rule | C2 at CAS | C3 | C4 readback | C5 successor | Effect |
/// |---|---|---|---|---|---|
/// | R1 | revoked | no | - | no | E1; no assignment/root mutation |
/// | R2 | unavailable | no | - | no | E2 |
/// | R3a | exact | yes | revoked | no | E3 |
/// | R3b | exact | yes | revoked | yes | E4 |
/// | R3c | exact | ambiguous win | revoked on retry | no | E3 |
#[tokio::test]
async fn cold_terminal_cleanup_claim_revalidates_registry_around_root_cas() {
    let target = registry_race_target();
    let registered = || terminal_cleanup_worker(&target, terminal_cleanup_manifest());

    let r1_repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("R1 Session repository"),
    );
    create(
        r1_repo.as_ref(),
        registry_race_terminal_session("registry-race-r1", None),
    )
    .await;
    let r1_revision = r1_repo.get("registry-race-r1").await.unwrap().revision;
    let r1_application = application_with_configuration(
        r1_repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    let r1_workers = Arc::new(RecordingWorkerObservations::default());
    r1_workers.script([
        WorkerObservationStep::Workers(vec![registered()]),
        WorkerObservationStep::Workers(Vec::new()),
    ]);
    r1_application
        .set_worker_observation_source(r1_workers.clone())
        .expect("R1 Worker observation authority");
    assert!(
        r1_application
            .claim_next_terminal_cleanup(target.clone())
            .await
            .expect("R1 revoked Worker is a closed scan result")
            .is_none(),
        "R1/E1"
    );
    let r1_durable = r1_repo.get("registry-race-r1").await.unwrap();
    assert_eq!(r1_durable.realization, None, "R1/E1");
    assert_eq!(r1_durable.revision, r1_revision, "R1/E1 zero root CAS");
    assert_eq!(r1_workers.list_calls.load(Ordering::SeqCst), 2, "R1/E1");

    let r2_repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("R2 Session repository"),
    );
    create(
        r2_repo.as_ref(),
        registry_race_terminal_session("registry-race-r2", None),
    )
    .await;
    let r2_revision = r2_repo.get("registry-race-r2").await.unwrap().revision;
    let r2_application = application_with_configuration(
        r2_repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    let r2_workers = Arc::new(RecordingWorkerObservations::default());
    r2_workers.script([
        WorkerObservationStep::Workers(vec![registered()]),
        WorkerObservationStep::Fail,
    ]);
    r2_application
        .set_worker_observation_source(r2_workers.clone())
        .expect("R2 Worker observation authority");
    assert!(
        matches!(
            r2_application
                .claim_next_terminal_cleanup(target.clone())
                .await,
            Err(awaken_session_contract::SessionRealizationControlFailure::Unavailable(_))
        ),
        "R2/E2"
    );
    let r2_durable = r2_repo.get("registry-race-r2").await.unwrap();
    assert_eq!(r2_durable.realization, None, "R2/E2");
    assert_eq!(r2_durable.revision, r2_revision, "R2/E2 zero root CAS");
    assert_eq!(r2_workers.list_calls.load(Ordering::SeqCst), 2, "R2/E2");

    let r3_prior = awaken_session_contract::SessionRealizationLease {
        owner: target.owner.clone(),
        runtime_incarnation: "worker-registry-race:1:boot-prior".into(),
        epoch: 7,
        expires_at_unix_ms: 1,
    };
    let r3_inner: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("R3 Session repository"),
    );
    create(
        r3_inner.as_ref(),
        registry_race_terminal_session("registry-race-r3", Some(r3_prior.clone())),
    )
    .await;
    let r3_repo = Arc::new(FaultingSessionRepository::new(r3_inner.clone()));
    let r3_application = application_with_configuration(
        r3_repo,
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    let r3_workers = Arc::new(RecordingWorkerObservations::default());
    r3_workers.script([
        WorkerObservationStep::Workers(vec![registered()]),
        WorkerObservationStep::Workers(vec![registered()]),
        WorkerObservationStep::Workers(Vec::new()),
    ]);
    r3_application
        .set_worker_observation_source(r3_workers.clone())
        .expect("R3 Worker observation authority");
    assert!(
        r3_application
            .claim_next_terminal_cleanup(target.clone())
            .await
            .expect("R3 revoked readback is compensated")
            .is_none(),
        "R3a/E3"
    );
    assert_eq!(
        r3_inner.get("registry-race-r3").await.unwrap().realization,
        Some(r3_prior),
        "R3a/E3 restores the exact prior realization"
    );
    assert_eq!(r3_workers.list_calls.load(Ordering::SeqCst), 3, "R3a/E3");

    let r3b_inner: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("R3b Session repository"),
    );
    create(
        r3b_inner.as_ref(),
        registry_race_terminal_session("registry-race-r3b", None),
    )
    .await;
    let r3b_successor = awaken_session_contract::SessionRealizationLease {
        owner: target.owner.clone(),
        runtime_incarnation: "worker-registry-race:3:boot-successor".into(),
        epoch: 99,
        expires_at_unix_ms: u64::MAX,
    };
    let r3b_repo = Arc::new(FaultingSessionRepository::new(r3b_inner.clone()));
    r3b_repo.commit_concurrent_realization_then_conflict_once(
        "revoke-terminal-cleanup-recovery",
        r3b_successor.clone(),
    );
    let r3b_application = application_with_configuration(
        r3b_repo,
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    let r3b_workers = Arc::new(RecordingWorkerObservations::default());
    r3b_workers.script([
        WorkerObservationStep::Workers(vec![registered()]),
        WorkerObservationStep::Workers(vec![registered()]),
        WorkerObservationStep::Workers(Vec::new()),
    ]);
    r3b_application
        .set_worker_observation_source(r3b_workers)
        .expect("R3b Worker observation authority");
    assert!(
        r3b_application
            .claim_next_terminal_cleanup(target.clone())
            .await
            .expect("R3b concurrent successor resolves the revoked claim")
            .is_none(),
        "R3b/E4"
    );
    assert_eq!(
        r3b_inner
            .get("registry-race-r3b")
            .await
            .unwrap()
            .realization,
        Some(r3b_successor),
        "R3b/E4 compensation cannot overwrite the concurrent root"
    );

    let r3c_inner: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("R3c Session repository"),
    );
    create(
        r3c_inner.as_ref(),
        registry_race_terminal_session("registry-race-r3c", None),
    )
    .await;
    let r3c_repo = Arc::new(FaultingSessionRepository::new(r3c_inner.clone()));
    r3c_repo.commit_then_conflict_once("claim-terminal-cleanup-recovery");
    let r3c_application = application_with_configuration(
        r3c_repo,
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    let r3c_workers = Arc::new(RecordingWorkerObservations::default());
    r3c_workers.script([
        WorkerObservationStep::Workers(vec![registered()]),
        WorkerObservationStep::Workers(vec![registered()]),
        WorkerObservationStep::Workers(Vec::new()),
    ]);
    r3c_application
        .set_worker_observation_source(r3c_workers)
        .expect("R3c Worker observation authority");
    assert!(
        r3c_application
            .claim_next_terminal_cleanup(registry_race_target())
            .await
            .expect("R3c ambiguous win is compensated after retry revocation")
            .is_none(),
        "R3c/E3"
    );
    assert_eq!(
        r3c_inner
            .get("registry-race-r3c")
            .await
            .unwrap()
            .realization,
        None,
        "R3c/E3 ambiguous committed claim restores its prior realization"
    );
}

#[tokio::test]
async fn cold_terminal_cleanup_claim_requires_current_compatible_worker_before_root_cas() {
    // Worker-admission cause/effect graph: C1 the canonical registry source is
    // absent; C2 the exact current incarnation is registered; C3 its manifest
    // explicitly supports terminal-cleanup v2; C4 the frozen active Resource
    // generation requires Session Resources and the manifest supplies it; C5
    // an installed Repository has a credential binding and the manifest
    // supplies the Repository-credential capability; C6 an unowned Environment
    // has never been materialized; C7 a retained checkpoint format is
    // explicitly supported. E1 only the applicable C2-C7 facts admit
    // the root lease CAS; E2 every missing/stale/incompatible fact leaves the
    // Session root unchanged; E3 Draining/full-capacity Workers may release
    // existing resources because cleanup is not new Run capacity; E4 physical
    // Environment state without a historical owner fails closed.
    //
    // | Rule | source | protocol | resources | Environment | Effect |
    // |---|---|---|---|---|---|
    // | W1a | absent | - | - | unmaterialized | E2 unavailable |
    // | W1b | read failure | - | - | unmaterialized | E2 unavailable |
    // | W2 | stale incarnation | v2 | present | unmaterialized | E2 skip |
    // | W3 | Starting | v2 | present | unmaterialized | E2 skip |
    // | W4 | exact | legacy ANY | present | unmaterialized | E2 skip |
    // | W5 | exact | v2 | missing capability | unmaterialized | E2 skip |
    // | W6 | exact Draining/full | v2 | exact capability | unmaterialized | E1+E3 |
    // | W7 | exact | v2, Repository credential missing/exact | credentialed | E2 then E1 |
    // | W8 | exact | v2, checkpoint format missing | none | Hibernated/owned | E2 |
    // | W9 | exact | v2, checkpoint format exact | none | Hibernated/owned | E1 |
    // | W10 | exact | v2 | exact capability | Resident/no owner | E2+E4 |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("Worker-compatible cleanup Session repository"),
    );
    let target = awaken_session_contract::SessionRealizationTarget {
        owner: "worker-compatible".into(),
        runtime_incarnation: "worker-compatible:3:boot".into(),
        lease_expires_at_unix_ms: u64::MAX,
        reassign_existing_lease: false,
    };
    let mut requested = persisted("claim-compatible", true, "terminated");
    requested.resources = awaken_session_contract::SessionResourceState::from_active(
        file_resources("claim-compatible-file"),
    );
    assert!(requested.terminal_cleanup.request("claim-compatible"));
    requested
        .terminal_cleanup
        .freeze_targets("claim-compatible", [], 0, 0)
        .expect("freeze compatible cleanup target");
    create(repo.as_ref(), requested).await;

    let application = application_with_configuration(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    assert!(
        matches!(
            application
                .claim_next_terminal_cleanup(target.clone())
                .await,
            Err(awaken_session_contract::SessionRealizationControlFailure::Unavailable(_))
        ),
        "W1/E2"
    );
    assert!(
        repo.get("claim-compatible")
            .await
            .unwrap()
            .realization
            .is_none(),
        "W1/E2 zero root mutation"
    );

    let workers = Arc::new(RecordingWorkerObservations::default());
    let mut stale = terminal_cleanup_worker(&target, terminal_cleanup_manifest());
    stale.snapshot.identity =
        awaken_worker_contract::WorkerIdentity::new(target.owner.clone(), "stale-boot", 4);
    workers.replace(vec![stale]);
    application
        .set_worker_observation_source(workers.clone())
        .expect("install exact Worker observations");
    workers.fail.store(true, Ordering::SeqCst);
    assert!(
        matches!(
            application
                .claim_next_terminal_cleanup(target.clone())
                .await,
            Err(awaken_session_contract::SessionRealizationControlFailure::Unavailable(_))
        ),
        "W1b/E2"
    );
    workers.fail.store(false, Ordering::SeqCst);
    assert!(
        application
            .claim_next_terminal_cleanup(target.clone())
            .await
            .expect("W2 stale Worker is ineligible")
            .is_none(),
        "W2/E2"
    );

    let mut starting = terminal_cleanup_worker(&target, terminal_cleanup_manifest());
    starting.snapshot.state = awaken_worker_contract::WorkerState::Starting;
    workers.replace(vec![starting]);
    assert!(
        application
            .claim_next_terminal_cleanup(target.clone())
            .await
            .expect("W3 unready Worker is ineligible")
            .is_none(),
        "W3/E2"
    );

    workers.replace(vec![terminal_cleanup_worker(
        &target,
        awaken_worker_contract::WorkerManifest::default(),
    )]);
    assert!(
        application
            .claim_next_terminal_cleanup(target.clone())
            .await
            .expect("W4 legacy manifest is an ineligible candidate")
            .is_none(),
        "W4/E2"
    );

    workers.replace(vec![terminal_cleanup_worker(
        &target,
        terminal_cleanup_manifest(),
    )]);
    assert!(
        application
            .claim_next_terminal_cleanup(target.clone())
            .await
            .expect("W5 missing Resource capability is ineligible")
            .is_none(),
        "W5/E2"
    );

    let mut manifest = terminal_cleanup_manifest();
    manifest
        .capabilities
        .insert(awaken_worker_contract::SESSION_RESOURCES_CAPABILITY.to_string());
    let mut worker = terminal_cleanup_worker(&target, manifest);
    worker.snapshot.state = awaken_worker_contract::WorkerState::Draining;
    worker.snapshot.in_flight = worker.snapshot.manifest.capacity.max_concurrent;
    workers.replace(vec![worker]);
    let assignment = application
        .claim_next_terminal_cleanup(target.clone())
        .await
        .expect("W6 compatible Worker admission")
        .expect("W6 assignment");
    assert_eq!(assignment.session_id, "claim-compatible", "W6/E1");

    let credentialed_session_id = "claim-active-credential";
    let resources = credentialed_repository_resources("source", "repo-active");
    let mut credentialed = persisted(credentialed_session_id, true, "terminated");
    credentialed.resources = awaken_session_contract::SessionResourceState::from_active(resources);
    assert!(
        credentialed
            .terminal_cleanup
            .request(credentialed_session_id)
    );
    credentialed
        .terminal_cleanup
        .freeze_targets(credentialed_session_id, [], 0, 0)
        .expect("freeze credentialed cleanup target");
    create(repo.as_ref(), credentialed).await;
    let mut resource_only_manifest = terminal_cleanup_manifest();
    resource_only_manifest
        .capabilities
        .insert(awaken_worker_contract::SESSION_RESOURCES_CAPABILITY.to_string());
    workers.replace(vec![terminal_cleanup_worker(
        &target,
        resource_only_manifest.clone(),
    )]);
    assert!(
        application
            .claim_next_terminal_cleanup(target.clone())
            .await
            .expect("W7 missing Repository credential capability is ineligible")
            .is_none(),
        "W7/E2"
    );
    resource_only_manifest
        .capabilities
        .insert(awaken_worker_contract::REPOSITORY_CREDENTIALS_CAPABILITY.to_string());
    workers.replace(vec![terminal_cleanup_worker(
        &target,
        resource_only_manifest,
    )]);
    let credentialed_assignment = application
        .claim_next_terminal_cleanup(target.clone())
        .await
        .expect("W7 credential-compatible Worker admission")
        .expect("W7 assignment");
    assert_eq!(
        credentialed_assignment.session_id, credentialed_session_id,
        "W7/E1"
    );

    let checkpoint_session_id = "claim-checkpoint";
    let generation = awaken_session_contract::SandboxGeneration::new(
        checkpoint_session_id,
        1,
        u64::MAX,
        "env-7",
        "base-image",
    );
    let mut checkpoint = persisted(checkpoint_session_id, true, "terminated");
    assert!(checkpoint.terminal_cleanup.request(checkpoint_session_id));
    checkpoint
        .terminal_cleanup
        .freeze_targets(checkpoint_session_id, [], 0, 0)
        .expect("freeze checkpoint cleanup target");
    checkpoint.environment = awaken_session_contract::SessionEnvironmentState::Hibernated {
        checkpoint: awaken_session_contract::SandboxCheckpointRef {
            id: "checkpoint-object".into(),
            format: "portable-checkpoint-v1".into(),
            digest: "sha256:checkpoint".into(),
            size_bytes: 7,
            created_at_unix_ms: 1,
            expires_at_unix_ms: u64::MAX,
            environment_fingerprint: "env-7".into(),
            base_image_fingerprint: "base-image".into(),
            excluded_mounts: Vec::new(),
            suspend_effect_id: "suspend-effect".into(),
        },
        generation,
    };
    checkpoint.realization = Some(awaken_session_contract::SessionRealizationLease {
        owner: target.owner.clone(),
        runtime_incarnation: "worker-compatible:2:old-boot".into(),
        epoch: 5,
        expires_at_unix_ms: 1,
    });
    create(repo.as_ref(), checkpoint).await;
    workers.replace(vec![terminal_cleanup_worker(
        &target,
        terminal_cleanup_manifest(),
    )]);
    assert!(
        application
            .claim_next_terminal_cleanup(target.clone())
            .await
            .expect("W8 unsupported checkpoint format is ineligible")
            .is_none(),
        "W8/E2"
    );
    let mut checkpoint_manifest = terminal_cleanup_manifest();
    checkpoint_manifest
        .checkpoint_formats
        .insert("portable-checkpoint-v1".into());
    workers.replace(vec![terminal_cleanup_worker(&target, checkpoint_manifest)]);
    let checkpoint_assignment = application
        .claim_next_terminal_cleanup(target.clone())
        .await
        .expect("W9 supported checkpoint admission")
        .expect("W9 assignment");
    assert_eq!(
        checkpoint_assignment.session_id, checkpoint_session_id,
        "W9/E1"
    );

    let mut unowned = persisted("claim-unowned-resident", true, "terminated");
    assert!(unowned.terminal_cleanup.request("claim-unowned-resident"));
    unowned
        .terminal_cleanup
        .freeze_targets("claim-unowned-resident", [], 0, 0)
        .expect("freeze unowned cleanup target");
    unowned.environment = awaken_session_contract::SessionEnvironmentState::Resident {
        binding: "physical-without-owner".into(),
        effect_id: None,
        generation: None,
        idle_since_unix_ms: None,
    };
    create(repo.as_ref(), unowned).await;
    assert!(
        matches!(
            application.claim_next_terminal_cleanup(target).await,
            Err(awaken_session_contract::SessionRealizationControlFailure::Invalid(_))
        ),
        "W10/E2+E4"
    );
    assert!(
        repo.get("claim-unowned-resident")
            .await
            .unwrap()
            .realization
            .is_none(),
        "W10/E2 zero root mutation"
    );
}

#[tokio::test]
async fn local_terminal_cleanup_claim_reads_the_exact_root_beyond_the_recovery_batch() {
    // Exact-local claim cause/effect graph: C1 the requested local Session is
    // inside or beyond the repository's bounded recovery page; C2 earlier
    // terminal rows remain pending; C3 a lease is exact-current or live under
    // another process incarnation. E1 the named root alone is read, fenced,
    // and returned; E2 no earlier row is claimed or allowed to starve it; E3
    // exact response-loss replay returns the same lease; E4 a live predecessor
    // is never preempted before expiry; E5 the co-located owner renews that
    // exact generation without requiring an external Worker Registry record.
    //
    // | Rule | target position | earlier pending rows | Effect |
    // |---|---|---|---|
    // | X1 | first page | any | E1 |
    // | X2 | after 256-row page | 257 | E1 + E2 |
    // | X3 | exact-current | any | E3 |
    // | X4 | live other incarnation | any | E4 |
    // | X5 | exact-current local renewal | any | E5 |
    //
    // X2 is the regression rule: local execution must never reuse the external
    // claim-next scan as an approximate lookup for a known Session id. X1-X4
    // also require the internal claim-result representation to return the
    // complete assignment unchanged; indirection cannot alter any lease fact.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("exact local terminal Session repository"),
    );
    let requested_local = |id: &str| {
        let mut session = persisted(id, true, "terminated");
        let awaken_session_contract::SessionBaselineState::Frozen(baseline) = &mut session.baseline
        else {
            unreachable!("fixture is frozen")
        };
        baseline.runtime_placement = SessionRuntimePlacement::Local;
        assert!(session.terminal_cleanup.request(id));
        session
            .terminal_cleanup
            .freeze_targets(id, [], 0, 0)
            .expect("freeze local cleanup target");
        session
    };
    for index in 0..257 {
        let id = format!("batch-prefix-{index:03}");
        create(repo.as_ref(), requested_local(&id)).await;
    }
    let target_id = "zz-exact-local-terminal";
    create(repo.as_ref(), requested_local(target_id)).await;

    let application = application_with_configuration(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::LocalWorker,
            local_realization_owner: "embedded-terminal-worker".into(),
            ..Default::default()
        },
    );
    let assignment = application
        .claim_local_terminal_cleanup_assignment(target_id)
        .await
        .expect("X2 exact local assignment");
    assert_eq!(assignment.session_id, target_id, "X2/E1");
    assert_eq!(assignment.lease.epoch, 1, "X2/E1");
    assert_eq!(assignment.lease.owner, "embedded-terminal-worker", "X2/E1");
    let replay = application
        .claim_local_terminal_cleanup_assignment(target_id)
        .await
        .expect("X3 exact replay");
    assert_eq!(replay.lease, assignment.lease, "X3/E3");
    let requested_expiry_unix_ms = assignment
        .lease
        .expires_at_unix_ms
        .saturating_add(crate::realization::LOCAL_SESSION_REALIZATION_LEASE_MS);
    let renewed = application
        .renew_session_realization(awaken_session_contract::RenewSessionRealization {
            session_id: target_id.into(),
            asserted_lease: assignment.lease.clone(),
            requested_expires_at_unix_ms: requested_expiry_unix_ms,
        })
        .await
        .expect("X5 exact local generation renewal");
    assert_eq!(renewed.owner, assignment.lease.owner, "X5/E5");
    assert_eq!(
        renewed.runtime_incarnation, assignment.lease.runtime_incarnation,
        "X5/E5"
    );
    assert_eq!(renewed.epoch, assignment.lease.epoch, "X5/E5");
    assert_eq!(
        renewed.expires_at_unix_ms, requested_expiry_unix_ms,
        "X5/E5"
    );
    let replacement = application_with_configuration(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::LocalWorker,
            local_realization_owner: "embedded-terminal-worker".into(),
            ..Default::default()
        },
    );
    assert!(
        matches!(
            replacement
                .claim_local_terminal_cleanup_assignment(target_id)
                .await,
            Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "X4/E4"
    );
    assert!(
        repo.get("batch-prefix-000")
            .await
            .unwrap()
            .realization
            .is_none(),
        "X2/E2"
    );
}

#[tokio::test]
async fn recovery_treats_a_concurrently_deleted_terminal_session_as_converged() {
    let inner: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(
        inner.as_ref(),
        persisted("deleted-during-scan", false, "terminated"),
    )
    .await;
    let repository = Arc::new(FaultingSessionRepository::new(inner));
    let application = application(
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    // The recovery scan already holds the terminal candidate when an eager
    // cleanup actor wins and removes the aggregate before the re-read.
    repository.get_not_found_once();
    let report = application.reconcile_resource_activations().await;

    assert!(
        report.failures.is_empty(),
        "stale scan is not a retryable outage"
    );
}

/// No-publication crash-window graph. C1 the terminal fence carries no
/// Repository publication intent; C2 preparation fails before its receipt;
/// C3 recovery/replay observes Requested or Completed. Effects: E1 the durable
/// target stays Requested until preparation is accepted; E2 no publication
/// command/effect is invented; E3 recovery reuses the exact cleanup effect id,
/// then performs one aggregate disposal; E4 Completed is absorbing and only
/// replays the aggregate-level process-local acknowledgement.
///
/// | Rule | publication intent | cleanup phase | Effect |
/// |---|---|---|---|
/// | L1 | absent | first preparation fails | E1 + E2, Requested retained |
/// | L2 | absent | retry succeeds | E3, preparation ack once |
/// | L3 | absent | Completed replay | E2 + E4, no preparation replay |
#[tokio::test]
async fn terminal_cleanup_recovery_reuses_intent_and_skips_completed_effects() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let mut session = persisted("cleanup-recovery", false, "terminated");
    session.environment.set_resident("opaque-environment");
    create(repo.as_ref(), session).await;

    let runtime = Arc::new(RecordingCleanupRuntime::default());
    runtime.fail_once.store(true, Ordering::SeqCst);
    let application = SessionApplication::new_with_configuration(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration::default(),
    );

    application
        .release_terminal_resources("workspace", "cleanup-recovery")
        .await
        .expect_err("R1 injected crash leaves cleanup pending");
    let pending = repo
        .get("cleanup-recovery")
        .await
        .expect("R1 durable intent");
    assert!(pending.terminal_cleanup.is_requested(), "R1 intent");
    assert_eq!(
        pending.environment.binding(),
        Some("opaque-environment"),
        "R1 Environment remains authoritative until receipt commit"
    );

    let completed = application
        .release_terminal_resources("workspace", "cleanup-recovery")
        .await
        .expect("R2 recovery")
        .expect("R2 retained Session");
    assert!(completed.terminal_cleanup.is_completed(), "R2 receipt");
    assert!(
        matches!(
            completed.environment,
            awaken_session_contract::SessionEnvironmentState::Unmaterialized
        ),
        "R2 Environment projection removed with receipt"
    );
    assert_eq!(
        runtime.acknowledged_effects.lock().unwrap().len(),
        1,
        "R2 acknowledges preparation only after its durable root CAS"
    );
    assert_eq!(runtime.acknowledged_completed.lock().unwrap().len(), 1);

    application
        .release_terminal_resources("workspace", "cleanup-recovery")
        .await
        .expect("R3 completed replay");
    let intents = runtime.intents.lock().unwrap();
    assert_eq!(intents.len(), 2, "R3 performs no effect");
    assert_eq!(
        intents[0].effect_id, intents[1].effect_id,
        "R1/R2 stable intent"
    );
    assert_eq!(runtime.effective_ids.lock().unwrap().len(), 1, "R1/R2");
    assert!(
        runtime.publication_intents.lock().unwrap().is_empty(),
        "L1-L3/E2 legacy cleanup never invents publication"
    );
    let acknowledgements = runtime.acknowledged_effects.lock().unwrap();
    assert_eq!(
        acknowledgements.len(),
        1,
        "R3 does not replay preparation acknowledgement"
    );
    let completed_acknowledgements = runtime.acknowledged_completed.lock().unwrap();
    assert_eq!(
        completed_acknowledgements.len(),
        2,
        "R3 replays only the aggregate completion acknowledgement"
    );
    assert_eq!(
        completed_acknowledgements[0], completed_acknowledgements[1],
        "R3 completion acknowledgement retains the exact lease generation"
    );
}

/// A competing cleanup worker may win any root CAS after performing the exact
/// durable phase. The loser must re-read that phase and continue; returning the
/// conflict would leave a hidden Session and its Environment indefinitely live.
#[tokio::test]
async fn terminal_cleanup_rebases_after_competing_root_writer() {
    let durable: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let repo = Arc::new(FaultingSessionRepository::new(durable));
    let mut session = persisted("cleanup-root-race", false, "terminated");
    session.environment.set_resident("opaque-environment");
    create(repo.as_ref(), session).await;
    repo.commit_then_conflict_once("terminal-cleanup-fence");

    let runtime = Arc::new(RecordingCleanupRuntime::default());
    let application = SessionApplication::new_with_configuration(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration::default(),
    );

    let completed = application
        .release_terminal_resources("workspace", "cleanup-root-race")
        .await
        .expect("CAS loser resumes from the winning durable phase")
        .expect("archived Session remains");
    assert!(completed.terminal_cleanup.is_completed());
    assert!(matches!(
        completed.environment,
        awaken_session_contract::SessionEnvironmentState::Unmaterialized
    ));
    assert_eq!(runtime.effective_ids.lock().unwrap().len(), 1);
    assert!(
        repo.reconcilable_sessions()
            .await
            .unwrap()
            .sessions
            .is_empty()
    );
}

/// Completion-response-loss design. Causes: C1 the exact terminal effect and
/// receipt succeed; C2 the `terminal-cleanup-disposal` CAS commits; C3 its
/// response is reported as a conflict; C4 retry reads the aggregate's Completed
/// target set and exact realization lease. Effects: E1 physical cleanup runs
/// once; E2 retry performs no second provider effect; E3 the same command+lease
/// is acknowledged from durable truth; E4 the caller observes completion.
/// Constraint: acknowledgement is process-local retirement, never a receipt or
/// a substitute cleanup authority.
///
/// | Rule | C1 | C2 | C3 | C4 | Effect |
/// |---|---|---|---|---|---|
/// | A1 | T | T | T | T | E1 + E2 + E3 + E4 |
#[tokio::test]
async fn terminal_cleanup_acknowledges_a_committed_completion_after_response_loss() {
    let durable: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let repo = Arc::new(FaultingSessionRepository::new(durable));
    let mut session = persisted("cleanup-completion-response-loss", false, "terminated");
    session.environment.set_resident("opaque-environment");
    create(repo.as_ref(), session).await;
    repo.commit_then_conflict_once("terminal-cleanup-disposal");

    let runtime = Arc::new(RecordingCleanupRuntime::default());
    let application = SessionApplication::new_with_configuration(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        repo,
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration::default(),
    );

    let completed = application
        .release_terminal_resources("workspace", "cleanup-completion-response-loss")
        .await
        .expect("A1 response-loss retry reads the committed completion")
        .expect("A1 archived Session remains");
    assert!(completed.terminal_cleanup.is_completed(), "A1/E4");
    assert_eq!(runtime.intents.lock().unwrap().len(), 1, "A1/E1/E2");
    let assignments = runtime.terminal_assignments.lock().unwrap();
    assert_eq!(
        assignments.len(),
        2,
        "A1 installs Prepare and Dispose under one terminal generation"
    );
    assert_eq!(
        assignments[0].lease, assignments[1].lease,
        "A1 one generation"
    );
    let acknowledgements = runtime.acknowledged_completed.lock().unwrap();
    assert_eq!(acknowledgements.len(), 1, "A1/E3");
    assert_eq!(
        acknowledgements[0].1, assignments[0].lease,
        "A1/E3 reuses the durable terminal generation"
    );
    assert_eq!(
        acknowledgements[0].0, "cleanup-completion-response-loss",
        "A1/E3 reuses the durable target"
    );
}

/// Delete cleanup has two legitimate triggers but one cleanup kernel. Causes:
/// C1 the eager actor and lifecycle actor share a hidden Session; C2 the winner
/// commits the final cleanup receipt and tombstone while the loser is at its
/// next root CAS; C3 the loser observes repository NotFound rather than a CAS
/// Conflict. Effects: E1 the winner's tombstone remains authoritative; E2 the
/// loser returns the existing `None` completion receipt; E3 no duplicate
/// Runtime effect or recoverable-cleanup warning is produced. Decision rule
/// T1=C1+C2+C3 => E1+E2+E3. A NotFound before the first read follows the same
/// terminal no-op; other mutation failures remain errors in the crash matrix.
#[tokio::test]
async fn terminal_cleanup_stale_loser_accepts_the_winners_tombstone() {
    // Constraint/Invariant: the winner's repository tombstone is final; a stale
    // loser may acknowledge it but cannot replay Runtime cleanup.
    let durable: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let repo = Arc::new(FaultingSessionRepository::new(durable));
    create(
        repo.as_ref(),
        persisted("cleanup-tombstone-race", false, "idle"),
    )
    .await;
    let runtime = Arc::new(RecordingCleanupRuntime::default());
    let application = SessionApplication::new_with_configuration(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration::default(),
    );
    let transition = application
        .commit_delete_intent(SessionDeleteCommand::new("cleanup-tombstone-race"))
        .await
        .expect("delete fence");
    repo.tombstone_after_operation_once("resource-release-complete");

    let loser = application
        .release_terminal_resources(&transition.owner_scope, "cleanup-tombstone-race")
        .await
        .expect("T1/E2 concurrent winner is terminal success");

    assert!(loser.is_none(), "T1/E2 tombstone is the completion receipt");
    assert!(matches!(
        repo.get("cleanup-tombstone-race").await,
        Err(awaken_session_contract::SessionRepositoryError::NotFound)
    ));
    assert_eq!(runtime.effective_ids.lock().unwrap().len(), 1, "T1/E3");
}

/// Complete durable terminal-cleanup crash matrix. Causes are the process stop
/// location relative to C1=fence CAS, C2=quiesce, C3=target-intent CAS,
/// C4=idempotent preparation, C5=preparation CAS plus physical disposal, and
/// C6=disposal-receipt CAS.
/// Effects are E1 no pre-intent I/O, E2 recover from the last committed phase,
/// E3 reuse one effect identity, E4 keep Environment authority until completion,
/// E5 replay Completed without another provider effect, and E6 retire the
/// process-local preparation/disposal projections only after their respective
/// CAS is durably observable.
///
/// | Rule | Crash point | Durable phase | Recovery effect |
/// |---|---|---|---|
/// | F0 | before C1 | NotRequested | E1; normal fence begins later |
/// | F1 | after C1/before C2 | Fenced | E2 quiesces before targets |
/// | F2 | after C2/before C3 | Fenced | E2 repeats quiesce, freezes once |
/// | F3 | after C3/before C4 | Requested | E2 executes exact frozen targets |
/// | F4 | preparation response lost | Requested | E3 idempotent preparation replay |
/// | F5 | disposal receipt CAS unavailable | Disposing | E4 retained; preparation acked; disposal replayable |
/// | F6 | disposal CAS accepted | Completed | E5 no preparation replay + E6 |
#[tokio::test]
async fn terminal_cleanup_f0_through_f6_recover_from_durable_phase() {
    let durable: Arc<dyn ManagedSessionRepository> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let repo = Arc::new(FaultingSessionRepository::new(durable));
    let mut session = persisted("cleanup-f0-f6", false, "terminated");
    session.environment.set_resident("opaque-environment");
    create(repo.as_ref(), session).await;

    let runtime = Arc::new(RecordingCleanupRuntime::default());
    let application = SessionApplication::new_with_configuration(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration::default(),
    );

    let f0 = repo.get("cleanup-f0-f6").await.expect("F0 truth");
    assert!(!f0.terminal_cleanup.needs_reconciliation(), "F0/E1");
    assert!(runtime.intents.lock().unwrap().is_empty(), "F0/E1");

    runtime.fail_quiesce_once.store(true, Ordering::SeqCst);
    application
        .release_terminal_resources("workspace", "cleanup-f0-f6")
        .await
        .expect_err("F1 crash before quiescence");
    let f1 = repo.get("cleanup-f0-f6").await.expect("F1 truth");
    assert!(f1.terminal_cleanup.is_fenced(), "F1/E2");
    assert!(runtime.intents.lock().unwrap().is_empty(), "F1/E1");

    repo.fail_once("resource-release-intent");
    application
        .release_terminal_resources("workspace", "cleanup-f0-f6")
        .await
        .expect_err("F2 target-intent CAS outage after quiescence");
    let f2 = repo.get("cleanup-f0-f6").await.expect("F2 truth");
    assert!(f2.terminal_cleanup.is_fenced(), "F2/E2");
    assert!(runtime.intents.lock().unwrap().is_empty(), "F2/E1");

    runtime
        .fail_before_effect_once
        .store(true, Ordering::SeqCst);
    application
        .release_terminal_resources("workspace", "cleanup-f0-f6")
        .await
        .expect_err("F3 crash after target freeze before effect");
    let f3 = repo.get("cleanup-f0-f6").await.expect("F3 truth");
    assert!(f3.terminal_cleanup.is_requested(), "F3/E2");
    assert!(runtime.intents.lock().unwrap().is_empty(), "F3/E1");
    assert!(runtime.effective_ids.lock().unwrap().is_empty(), "F3/E1");

    runtime.fail_once.store(true, Ordering::SeqCst);
    application
        .release_terminal_resources("workspace", "cleanup-f0-f6")
        .await
        .expect_err("F4 lost effect receipt");
    let f4 = repo.get("cleanup-f0-f6").await.expect("F4 truth");
    assert!(f4.terminal_cleanup.is_requested(), "F4/E2");
    assert_eq!(runtime.effective_ids.lock().unwrap().len(), 1, "F4/E3");
    assert_eq!(
        f4.environment.binding(),
        Some("opaque-environment"),
        "F4/E4"
    );

    repo.fail_once("terminal-cleanup-disposal");
    application
        .release_terminal_resources("workspace", "cleanup-f0-f6")
        .await
        .expect_err("F5 completion CAS outage");
    let f5 = repo.get("cleanup-f0-f6").await.expect("F5 truth");
    assert!(
        f5.terminal_cleanup.is_requested(),
        "F5/E2 Disposing wraps Requested"
    );
    assert_eq!(runtime.effective_ids.lock().unwrap().len(), 1, "F5/E3");
    assert_eq!(
        f5.environment.binding(),
        Some("opaque-environment"),
        "F5/E4"
    );
    assert_eq!(
        runtime.acknowledged_effects.lock().unwrap().len(),
        1,
        "F5/E6 preparation acknowledgement follows its own durable CAS"
    );
    let assignment_projections_after_f5 = runtime.terminal_assignments.lock().unwrap().len();

    let f6 = application
        .release_terminal_resources("workspace", "cleanup-f0-f6")
        .await
        .expect("F6 recovery")
        .expect("archived Session retained");
    assert!(f6.terminal_cleanup.is_completed(), "F6/E5");
    assert!(matches!(
        f6.environment,
        awaken_session_contract::SessionEnvironmentState::Unmaterialized
    ));
    assert_eq!(
        runtime.acknowledged_effects.lock().unwrap().len(),
        1,
        "F6/E6 disposal recovery does not replay preparation acknowledgement"
    );
    assert_eq!(
        runtime.terminal_assignments.lock().unwrap().len(),
        assignment_projections_after_f5 + 1,
        "F6/E6 retries only the durable Disposing action"
    );
    assert_eq!(runtime.acknowledged_completed.lock().unwrap().len(), 1);
    let attempts = runtime.intents.lock().unwrap().len();
    application
        .release_terminal_resources("workspace", "cleanup-f0-f6")
        .await
        .expect("F6 response-loss replay");
    assert_eq!(runtime.intents.lock().unwrap().len(), attempts, "F6/E5");
    assert_eq!(runtime.effective_ids.lock().unwrap().len(), 1, "F6/E5");
    let acknowledgements = runtime.acknowledged_completed.lock().unwrap();
    assert_eq!(
        acknowledgements.len(),
        2,
        "F6/E5 aggregate completion acknowledgement is replay-safe"
    );
    assert_eq!(
        acknowledgements[0], acknowledgements[1],
        "F6/E5 exact replay"
    );
}

/// Deterministic archive/child race: the cleanup worker is paused after the
/// durable terminal fence but before its one Runtime snapshot. Both a legacy
/// synchronous delegated Run and an asynchronous coordinated Thread committed
/// in that interval must be included in the same frozen target set and can
/// never be lost behind a premature Completed marker.
#[tokio::test]
async fn terminal_fence_quiesces_before_freezing_concurrent_child() {
    // Test design. Causes: C1 terminal cleanup has committed its fence and pauses
    // before snapshot; C2 synchronous and coordinated children arrive in that
    // interval. Effects: E1 quiescence includes both in one frozen target set; E2
    // Completed cannot precede their cleanup. Constraint/Invariant: terminal fence
    // closes admission before target freeze. Decision rule: race C1+C2 and require
    // both children in E1 before completion.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(
        repo.as_ref(),
        persisted("cleanup-concurrent-child", false, "terminated"),
    )
    .await;
    let runtime = Arc::new(RecordingCleanupRuntime::default());
    runtime.block_quiesce.store(true, Ordering::SeqCst);
    let application = Arc::new(SessionApplication::new_with_configuration(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration::default(),
    ));

    let cleanup = {
        let application = application.clone();
        tokio::spawn(async move {
            application
                .release_terminal_resources("workspace", "cleanup-concurrent-child")
                .await
        })
    };
    runtime.quiesce_entered.notified().await;
    let fenced = repo
        .get("cleanup-concurrent-child")
        .await
        .expect("terminal fence committed before quiescence");
    assert!(fenced.terminal_cleanup.is_fenced());

    *runtime.delegated_snapshot.lock().unwrap() = awaken_session_contract::DelegatedRunSnapshot {
        delegated_runs: vec![awaken_session_contract::DelegatedRun {
            run_id: awaken_agent_contract::agent::run::Id("child-after-fence".into()),
            parent_call_id: "delegate-call".into(),
            agent_id: "child-agent".into(),
            status: awaken_agent_contract::agent::delegation::DelegationStatus::Open,
        }],
        coordinated_thread_ids: vec![awaken_agent_contract::agent::thread::Id(
            "coordinated-after-fence".into(),
        )],
        watermark: 23,
        runtime_commit_cursor: 29,
    };
    runtime.quiesce_release.notify_one();

    let completed = cleanup
        .await
        .expect("cleanup task")
        .expect("cleanup succeeds")
        .expect("archived Session remains");
    assert!(
        completed.terminal_cleanup.is_completed(),
        "terminal cleanup must be completed"
    );
    assert_eq!(completed.terminal_cleanup.delegation_watermark(), Some(23));
    let thread_ids = completed
        .terminal_cleanup
        .thread_ids()
        .cloned()
        .expect("completed cleanup retains frozen targets");
    assert_eq!(
        thread_ids,
        BTreeSet::from([
            "cleanup-concurrent-child".to_string(),
            "child-after-fence".to_string(),
            "coordinated-after-fence".to_string(),
        ])
    );
    let cleaned = runtime
        .intents
        .lock()
        .unwrap()
        .iter()
        .map(|intent| intent.thread_id.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(cleaned, thread_ids);
}

#[tokio::test]
async fn terminal_cleanup_restart_soak_preserves_authority_and_effect_identity() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    /* Soak cause/effect model. For each of 64 independent Sessions: C1 a
     * synchronous delegated Run and an asynchronous coordinated Thread exist in
     * the one Runtime snapshot; C2 terminal cleanup completes; C3 the
     * Coordinator/Store is reopened and the command is replayed. Effects: E1
     * root+both child kinds are frozen exactly once in the existing cleanup
     * operation; E2 one stable effective cleanup per thread; E3 restart replay
     * performs no new I/O; E4 no pending/quarantined Session authority leaks.
     * The blocked-quiescence test owns the concurrent interleaving oracle; this
     * loop owns repetition and restart leakage.
     *
     * | Rule | Sync child | Async Thread | Restart | Effects |
     * | S1 | yes | yes | no | E1+E2 |
     * | S2 | frozen | frozen | yes | E3+E4 | */
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cleanup-soak.db");
    let path = path.to_string_lossy().to_string();
    for iteration in 0..64 {
        let session_id = format!("cleanup-soak-{iteration}");
        let child_id = format!("cleanup-soak-child-{iteration}");
        let coordinated_id = format!("cleanup-soak-thread-{iteration}");
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(&path)
                .expect("open Session authority"),
        );
        create(repo.as_ref(), persisted(&session_id, false, "terminated")).await;
        let runtime = Arc::new(RecordingCleanupRuntime::default());
        *runtime.delegated_snapshot.lock().unwrap() =
            awaken_session_contract::DelegatedRunSnapshot {
                delegated_runs: vec![awaken_session_contract::DelegatedRun {
                    run_id: awaken_agent_contract::agent::run::Id(child_id.clone()),
                    parent_call_id: format!("delegate-{iteration}"),
                    agent_id: "child-agent".into(),
                    status: awaken_agent_contract::agent::delegation::DelegationStatus::Open,
                }],
                coordinated_thread_ids: vec![awaken_agent_contract::agent::thread::Id(
                    coordinated_id.clone(),
                )],
                watermark: iteration + 1,
                runtime_commit_cursor: iteration + 11,
            };
        let application = SessionApplication::new_with_configuration(
            runtime.clone(),
            Arc::new(NoopMcpRealizer),
            repo,
            Arc::new(RecordingEnvironmentSource::default()),
            SessionApplicationConfiguration::default(),
        );
        let completed = application
            .release_terminal_resources("workspace", &session_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            completed.terminal_cleanup.thread_ids().unwrap(),
            &BTreeSet::from([session_id.clone(), child_id, coordinated_id]),
            "E1 iteration {iteration}",
        );
        assert_eq!(runtime.effective_ids.lock().unwrap().len(), 3, "E2");

        drop(application);
        let reopened = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(&path)
                .expect("restart Session authority"),
        );
        let replay_runtime = Arc::new(RecordingCleanupRuntime::default());
        let replay_application = SessionApplication::new_with_configuration(
            replay_runtime.clone(),
            Arc::new(NoopMcpRealizer),
            reopened.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
            SessionApplicationConfiguration::default(),
        );
        replay_application
            .release_terminal_resources("workspace", &session_id)
            .await
            .unwrap();
        assert!(replay_runtime.intents.lock().unwrap().is_empty(), "E3");
        let scan = reopened.reconcilable_sessions().await.unwrap();
        assert!(scan.quarantined.is_empty(), "E4");
        assert!(
            scan.sessions
                .iter()
                .all(|row| row.session.session_id != session_id),
            "E4",
        );
    }
}

/// Cause/effect graph: C1 a baseline is frozen; C2 its explicit Runtime
/// placement is Local or Worker; C3 the process startup is local or registered.
/// The realization lease is intentionally absent from the causes: it is an
/// assignment fence, never placement policy. Effects are E1 local physical
/// realization or E2 dispatch-only Coordinator projection.
///
/// | Rule | Frozen placement | Process placement | Effect |
/// |---|---|---|---|
/// | P1 | preparing | any | E1 (not yet realizable) |
/// | P2 | local | local/registered | E1 |
/// | P3 | worker | local/registered | E2 |
/// | P4 | legacy | local | E1 |
/// | P5 | legacy | registered | E2 |
///
/// P4/P5 are the one-way upgrade interpretation for rows serialized before
/// placement existed. New creation is separately asserted never to emit the
/// legacy value.
#[test]
fn realization_owner_follows_the_frozen_placement_decision_table() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let local = application_with_configuration(
        repo.clone(),
        environments.clone(),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::LocalWorker,
            ..Default::default()
        },
    );
    let registered = application_with_configuration(
        repo,
        environments,
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );

    let frozen = |placement: SessionRuntimePlacement| {
        let mut value = persisted("placement", false, "idle");
        let awaken_session_contract::SessionBaselineState::Frozen(baseline) = &mut value.baseline
        else {
            unreachable!("fixture is frozen")
        };
        baseline.runtime_placement = placement;
        value
    };
    let preparing = {
        let mut value = frozen(SessionRuntimePlacement::Local);
        let baseline = value.frozen_baseline().expect("frozen").clone();
        value.baseline = awaken_session_contract::SessionBaselineState::Preparing(
            awaken_session_contract::SessionCreationIntent {
                control: awaken_session_contract::ControlSessionCreationInputs {
                    mutation_policy: awaken_session_contract::SessionMutationPolicy::Managed,
                    environment: baseline.environment,
                    runtime_placement: SessionRuntimePlacement::Local,
                    agent_id: baseline.agent_id,
                    agent_revision: baseline.agent_revision,
                    model_override: baseline.model_override,
                    system_prompt: *baseline.system_prompt,
                    model: baseline.model,
                    execution_model_ref: baseline.execution_model_ref,
                    runtime: baseline.runtime,
                    mcp_authoring: baseline.mcp_authoring,
                    delegate_ids: baseline.delegate_ids,
                    toolsets: baseline.toolsets,
                    mounts: baseline.mounts,
                    env: baseline.env,
                    prompts: baseline.prompts,
                    transcript_prefix: baseline.transcript_prefix,
                    resources: Default::default(),
                    initial_mcp: Vec::new(),
                },
            },
        );
        value
    };
    for (rule, value, local_expected, registered_expected) in [
        ("P1", preparing, false, false),
        ("P2", frozen(SessionRuntimePlacement::Local), false, false),
        ("P3", frozen(SessionRuntimePlacement::Worker), true, true),
    ] {
        assert_eq!(
            local.requires_external_realization(&value),
            local_expected,
            "{rule}/local"
        );
        assert_eq!(
            registered.requires_external_realization(&value),
            registered_expected,
            "{rule}/registered"
        );
    }
    assert_eq!(local.runtime_placement(), SessionRuntimePlacement::Local);
    assert_eq!(
        registered.runtime_placement(),
        SessionRuntimePlacement::Worker
    );
}

#[tokio::test]
async fn publication_acknowledgement_follows_the_renewal_decision_table() {
    // Cause/effect graph: C1 acknowledgement names the exact active
    // generation; C2 it names the same owner/incarnation/epoch under a
    // shorter expiry; C3 any immutable generation coordinate differs.
    // Effects: E1 acknowledge and make the Session idle; E2 commit nothing
    // and return the current Stage for canonical catch-up; E3 fail closed
    // without changing publication state.
    //
    // | Rule | exact | monotonic predecessor | replacement | Effect |
    // |---|---|---|---|---|
    // | A1 | yes | no | no | E1 |
    // | A2 | no | yes | no | E2 |
    // | A3 | no | no | yes | E3 |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let application = application(repo.clone(), environments);
    let current_expiry = u64::MAX - 1;

    let fixture = |id: &str| {
        let mut session = persisted(id, false, "activating");
        let lease = awaken_session_contract::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/boot-1".into(),
            epoch: 7,
            expires_at_unix_ms: current_expiry,
        };
        let attachment_id = awaken_session_contract::McpAttachmentId("browser".into());
        let generation = awaken_session_contract::McpGeneration(1);
        session.realization = Some(lease.clone());
        session
            .mcp
            .attachments
            .push(awaken_session_contract::SessionMcpAttachment {
                attachment_id: attachment_id.clone(),
                name: "browser".into(),
                generation,
                target: awaken_session_contract::McpTarget::parse_http(
                    "https://browser.example.test/mcp",
                )
                .expect("MCP target"),
                prompts_as_skills: false,
                origin: awaken_session_contract::McpAttachmentOrigin::Agent,
                credential: None,
                selected_plaintext_holder: None,
                state: awaken_session_contract::McpAttachmentState::Active,
                publication_acknowledged: false,
                realization: Some(awaken_session_contract::McpRealizationClaim {
                    realization_id: "realize-browser-1".into(),
                    runtime_incarnation: lease.runtime_incarnation.clone(),
                    lease_epoch: lease.epoch,
                    lease_expires_at_unix_ms: current_expiry,
                    stage_idempotency_key: "renew-browser-1".into(),
                }),
                attempts: 1,
                last_error: None,
            });
        let generation_ref = awaken_session_contract::McpGenerationRef {
            session_id: id.into(),
            attachment_id,
            generation,
            runtime_incarnation: lease.runtime_incarnation.clone(),
            lease_epoch: lease.epoch,
            lease_expires_at_unix_ms: current_expiry,
        };
        (session, lease, generation_ref)
    };

    let (exact, exact_lease, exact_generation) = fixture("ack-exact");
    create(repo.as_ref(), exact).await;
    let exact_result =
        awaken_session_contract::SessionRealizationControl::acknowledge_session_realization(
            &application,
            awaken_session_contract::AcknowledgeSessionRealization {
                session_id: "ack-exact".into(),
                lease: exact_lease,
                published: vec![exact_generation],
                drained: Vec::new(),
            },
        )
        .await
        .expect("A1 exact acknowledgement");
    assert!(
        matches!(
            exact_result.action,
            awaken_session_contract::SessionRealizationAction::Complete
        ),
        "A1/E1"
    );
    assert_eq!(
        repo.get("ack-exact").await.unwrap().execution.as_str(),
        "idle",
        "A1/E1"
    );

    let (renewed, renewed_lease, mut predecessor) = fixture("ack-renewed");
    create(repo.as_ref(), renewed).await;
    predecessor.lease_expires_at_unix_ms -= 1;
    let mut predecessor_lease = renewed_lease.clone();
    predecessor_lease.expires_at_unix_ms -= 1;
    let renewed_result =
        awaken_session_contract::SessionRealizationControl::acknowledge_session_realization(
            &application,
            awaken_session_contract::AcknowledgeSessionRealization {
                session_id: "ack-renewed".into(),
                lease: predecessor_lease,
                published: vec![predecessor],
                drained: Vec::new(),
            },
        )
        .await
        .expect("A2 monotonic predecessor");
    assert!(
        matches!(
            renewed_result.action,
            awaken_session_contract::SessionRealizationAction::Stage { .. }
        ),
        "A2/E2"
    );
    let renewed_truth = repo.get("ack-renewed").await.unwrap();
    assert_eq!(renewed_truth.execution.as_str(), "activating", "A2/E2");
    assert!(
        !renewed_truth.mcp.attachments[0].publication_acknowledged,
        "A2/E2"
    );

    let (replacement, replacement_lease, mut wrong_generation) = fixture("ack-replaced");
    create(repo.as_ref(), replacement).await;
    wrong_generation.attachment_id = awaken_session_contract::McpAttachmentId("other".into());
    let error =
        awaken_session_contract::SessionRealizationControl::acknowledge_session_realization(
            &application,
            awaken_session_contract::AcknowledgeSessionRealization {
                session_id: "ack-replaced".into(),
                lease: replacement_lease,
                published: vec![wrong_generation],
                drained: Vec::new(),
            },
        )
        .await
        .expect_err("A3 replacement must fail closed");
    assert!(
        matches!(
            error,
            awaken_session_contract::SessionRealizationControlFailure::Invalid(_)
        ),
        "A3/E3: {error}"
    );
    assert!(
        !repo.get("ack-replaced").await.unwrap().mcp.attachments[0].publication_acknowledged,
        "A3/E3"
    );
}

#[tokio::test]
async fn realization_preserves_or_closes_the_activity_interval_by_terminal_outcome() {
    /* Cause/effect graph: C1 an admitted activity is Running with one open
     * aggregate interval; C2 realization activates and acknowledges exact
     * Resource/publication generations; C3 realization fails retryably below
     * budget; C4 realization fails permanently or after budget. Effects: E1
     * activation/acknowledgement preserve Running and the exact interval; E2
     * activity settlement alone transitions Idle and emits one interval fact;
     * E3 a retry retains Running plus the open interval for recovery; E4 a
     * terminal failure atomically transitions ActivationFailed, closes the
     * interval, accumulates runtime, and emits one fact. Constraints: the root
     * CAS owns execution and interval together; realization never authors a
     * second billing path; stale lease cases are covered by the realization
     * authority table.
     *
     * | Rule | Running | Realization outcome | Retry budget | Effect |
     * |---|---|---|---|---|
     * | R1 | yes | activate + acknowledge | n/a | E1 |
     * | R2 | yes | activity settles | n/a | E2 |
     * | R3 | yes | retryable failure | available | E3 |
     * | R4 | yes | permanent failure | n/a | E4 |
     *
     * This test covers R1, R2, and R4. R3's durable retry/lease-expiry rule is
     * covered by `retryable_initial_realization_is_fenced_and_budgeted_durably`;
     * the production branch is identical for Running except it deliberately
     * retains the already-open interval. */
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let application = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let lease = awaken_session_contract::SessionRealizationLease {
        owner: "worker-a".into(),
        runtime_incarnation: "worker-a/boot-2".into(),
        epoch: 2,
        expires_at_unix_ms: u64::MAX,
    };
    let interval = |id: &str| awaken_session_contract::SessionRuntimeIntervalStart {
        interval_id: format!("interval:{id}"),
        activity_epoch: 1,
        started_at_unix_ms: 1,
        opened_revision: Default::default(),
        observations: Vec::new(),
    };

    let mut recovering = persisted("running-realization", false, "running");
    recovering.activity_epoch = 1;
    recovering.running_interval = Some(interval("running-realization"));
    recovering.realization = Some(lease.clone());
    recovering
        .resources
        .prepare("running-realization", file_resources("running-realization"))
        .expect("R1 pending Resource generation");
    let prepared_resource_revision = recovering.resources.revision;
    create(repo.as_ref(), recovering).await;

    let activated =
        awaken_session_contract::SessionRealizationControl::activate_session_realization(
            &application,
            awaken_session_contract::ActivateSessionRealization {
                session_id: "running-realization".into(),
                lease: lease.clone(),
                prepared_resource_revision: Some(prepared_resource_revision),
                mcp_receipts: Vec::new(),
            },
        )
        .await
        .expect("R1 activation");
    assert!(
        matches!(
            activated.action,
            awaken_session_contract::SessionRealizationAction::Publish { .. }
        ),
        "R1/E1"
    );
    let activated_truth = repo.get("running-realization").await.unwrap();
    assert_eq!(
        activated_truth.execution,
        SessionExecutionState::Running,
        "R1/E1"
    );
    assert_eq!(
        activated_truth.running_interval,
        Some(interval("running-realization")),
        "R1/E1"
    );

    awaken_session_contract::SessionRealizationControl::acknowledge_session_realization(
        &application,
        awaken_session_contract::AcknowledgeSessionRealization {
            session_id: "running-realization".into(),
            lease: lease.clone(),
            published: Vec::new(),
            drained: Vec::new(),
        },
    )
    .await
    .expect("R1 acknowledgement");
    let acknowledged = repo.get("running-realization").await.unwrap();
    assert_eq!(
        acknowledged.execution,
        SessionExecutionState::Running,
        "R1/E1"
    );
    assert_eq!(
        acknowledged.running_interval,
        Some(interval("running-realization")),
        "R1/E1"
    );

    let settled = application
        .settle_activity("running-realization", 1)
        .await
        .expect("R2 activity settlement");
    assert_eq!(settled.execution, SessionExecutionState::Idle, "R2/E2");
    assert!(settled.running_interval.is_none(), "R2/E2");

    let mut failed = persisted("running-realization-failed", false, "running");
    failed.activity_epoch = 1;
    failed.running_interval = Some(interval("running-realization-failed"));
    failed.realization = Some(lease.clone());
    create(repo.as_ref(), failed).await;
    application
        .fail_session_realization(awaken_session_contract::FailSessionRealization {
            session_id: "running-realization-failed".into(),
            lease,
            prepared_resource_revision: None,
            retryable: false,
            source_run_id: Some(awaken_agent_contract::agent::run::Id(
                "running-realization-run".into(),
            )),
            reason: "permanent realization failure".into(),
        })
        .await
        .expect("R4 terminal realization failure");
    let failed_truth = repo.get("running-realization-failed").await.unwrap();
    assert_eq!(
        failed_truth.execution,
        SessionExecutionState::ActivationFailed,
        "R4/E4"
    );
    assert_eq!(
        failed_truth
            .realization_progress
            .failure_source_run_id
            .as_deref(),
        Some(&awaken_agent_contract::agent::run::Id(
            "running-realization-run".into()
        )),
        "R4/E4 retains the exact Run failure owner"
    );
    assert!(failed_truth.running_interval.is_none(), "R4/E4");
    assert!(failed_truth.runtime_active_millis > 0, "R4/E4");
    let facts = repo.pending_lifecycle().await.expect("R2/R4 outbox");
    for session_id in ["running-realization", "running-realization-failed"] {
        assert_eq!(
            facts
                .iter()
                .filter(|fact| fact.object_id == session_id && fact.runtime_interval.is_some())
                .count(),
            1,
            "{session_id} emits exactly one interval fact"
        );
    }
}
