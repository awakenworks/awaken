use super::*;
use awaken_session_contract::SessionRealizationControl;

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
    async fn apply_session_inputs(
        &self,
        _thread: &str,
        _workspace_id: &str,
        resource_revision: u64,
        inputs: &awaken_session_contract::ResolvedSessionResources,
    ) -> Result<(), RunError> {
        self.applied
            .lock()
            .unwrap()
            .push((resource_revision, inputs.clone()));
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
                expected_session_revision: None,
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
        expected_session_revision: None,
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
        expected_session_revision: None,
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
            awaken_agent_contract::RedactedString::new("never-consumed"),
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
        renew_existing_lease: false,
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
                renew_existing_lease: false,
                reassign_existing_lease: true,
            },
        },
    )
    .await
    .expect("L4 claim-authorized reassignment");
    assert_eq!(reassigned.lease.epoch, replaced_lease.epoch + 1, "L4/E4");
    assert_eq!(reassigned.lease.owner, "other-worker", "L4/E4");

    let invalid = awaken_session_contract::SessionRealizationControl::begin_session_realization(
        &other,
        awaken_session_contract::BeginSessionRealization {
            session_id: "local-restart".into(),
            target: awaken_session_contract::SessionRealizationTarget {
                owner: "other-worker".into(),
                runtime_incarnation: "other-worker/boot-2".into(),
                lease_expires_at_unix_ms: u64::MAX,
                renew_existing_lease: true,
                reassign_existing_lease: true,
            },
        },
    )
    .await;
    assert!(
        matches!(
            invalid,
            Err(awaken_session_contract::SessionRealizationControlFailure::Invalid(_))
        ),
        "L5"
    );
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
/// | W2 | Worker | terminal | Releasing, no receipt | E2 remain pending |
/// | W3 | Worker | terminal | exact Worker receipt | E3 release durably |
#[tokio::test]
async fn worker_placement_defers_live_effects_but_not_terminal_cleanup() {
    // Constraint/Invariant: Worker placement owns live realization, while the
    // Session repository still owns terminal cleanup progress. Decision rule:
    // execute W1-W3 and distinguish deferred live effects, pending cleanup, and
    // exact-receipt completion.
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
    let terminal_lease = awaken_session_contract::SessionRealizationLease {
        owner: "worker-terminal-owner".into(),
        runtime_incarnation: "worker-terminal-incarnation".into(),
        epoch: 1,
        expires_at_unix_ms: 1,
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
    assert_eq!(report.failures.len(), 1, "W2/E2: {:#?}", report.failures);
    assert!(report.settled.is_empty(), "W2/E2");

    let live = repo.get("worker-live").await.expect("W1 durable truth");
    assert!(live.resources.pending.is_some(), "W1/E1");
    assert_eq!(live.resources.activations[0].attempts, 0, "W1/E1");
    let pending = repo.get("worker-terminal").await.expect("W2 durable truth");
    assert!(pending.terminal_cleanup.is_requested(), "W2/E2");
    let commands = application
        .terminal_cleanup_commands("worker-terminal", &terminal_lease)
        .await
        .unwrap()
        .expect("W2 terminal fence");
    assert_eq!(commands.len(), 1, "W2/E2");
    application
        .record_terminal_cleanup_completion(
            &terminal_lease,
            awaken_session_contract::SessionCleanupCompletion::new(&commands[0], Vec::new()),
        )
        .await
        .expect("W3 exact Worker receipt");
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
async fn remote_terminal_cleanup_uses_durable_commands_and_cold_receipt_replay() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 a terminal Worker-placed Session retains one exact
    // realization lease; C2 the frozen target set contains root and child; C3
    // no Worker receipt exists, one receipt exists, or every receipt exists; C4
    // a stale lease or exact post-completion replay is submitted. Effects: E1
    // Coordinator performs no Runtime teardown; E2 Worker polls only missing
    // canonical commands; E3 verified receipts complete the existing operation;
    // E4 stale authority fails closed; E5 cold/exact replay performs no effect.
    //
    // | Rule | lease | receipts | Effect |
    // | W1 | exact | none | E1 + E2(child before root) |
    // | W2 | stale | any | E4 |
    // | W3 | exact | partial | E2(one command) |
    // | W4 | exact | complete/replay | E3 + E5 |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("remote cleanup Session repository"),
    );
    let lease = awaken_session_contract::SessionRealizationLease {
        owner: "worker-a".into(),
        runtime_incarnation: "worker-a:incarnation-1".into(),
        epoch: 7,
        expires_at_unix_ms: 1,
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

    application
        .release_terminal_resources("workspace", "remote-cleanup")
        .await
        .expect_err("W1 waits for Worker receipts");
    assert!(runtime.intents.lock().unwrap().is_empty(), "W1/E1");
    let commands = application
        .terminal_cleanup_commands("remote-cleanup", &lease)
        .await
        .expect("W1 exact lease")
        .expect("W1 terminal fence");
    assert_eq!(commands.len(), 1, "W1/E2 child phase");
    assert_eq!(commands[0].thread_id, "remote-cleanup-child", "W1/E2");

    let mut stale = lease.clone();
    stale.epoch += 1;
    assert!(
        matches!(
            application
                .terminal_cleanup_commands("remote-cleanup", &stale)
                .await,
            Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "W2/E4"
    );

    let first = awaken_session_contract::SessionCleanupCompletion::new(&commands[0], Vec::new());
    application
        .record_terminal_cleanup_completion(&lease, first)
        .await
        .expect("W3 first receipt");
    let remaining = application
        .terminal_cleanup_commands("remote-cleanup", &lease)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(remaining.len(), 1, "W3/E2");
    let last = awaken_session_contract::SessionCleanupCompletion::new(&remaining[0], Vec::new());
    application
        .record_terminal_cleanup_completion(&lease, last.clone())
        .await
        .expect("W4 completes cleanup");
    let completed = repo
        .get("remote-cleanup")
        .await
        .expect("W4 durable Session");
    assert!(completed.terminal_cleanup.is_completed(), "W4/E3");
    assert!(runtime.intents.lock().unwrap().is_empty(), "W4/E1");

    application
        .record_terminal_cleanup_completion(&lease, last)
        .await
        .expect("W4/E5 exact response-loss replay");
    assert!(runtime.intents.lock().unwrap().is_empty(), "W4/E5");
}

/// Repository-publication control cause/effect graph. C1 a Requested terminal
/// operation has an exact publication intent; C2 a child completion is absent
/// or durable; C3 the asserted lease is exact or stale; C4 the publication
/// receipt is mismatched, exact, or an exact response-loss replay; C5 ordinary
/// cleanup commands may be empty while publication is pending; C6 the durable
/// Session row has one immutable Workspace owner. Effects: E1 the
/// child is the sole first command; E2 publication remains hidden behind the
/// child; E3 an empty cleanup vector remains cold-claimable; E4 stale/wrong
/// evidence fails without mutation; E5 the exact receipt is durably replayable;
/// E6 only then is the root finalizer exposed and completion becomes absorbing;
/// E7 the command projection carries that canonical Workspace beside the
/// command so transports need not accept a Worker-selected tenant.
///
/// | Rule | child | lease | publication receipt | Effect |
/// |---|---|---|---|---|
/// | P1 | pending | exact | none | E1 + E2 |
/// | P2 | complete | stale | none | E4 |
/// | P3 | none | unclaimed | none, cleanup=[] | E3 + E7 |
/// | P4 | complete | exact | wrong | E4 |
/// | P5 | complete | exact | exact/replay | E5 + E6 |
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
    let requested =
        |session_id: &str,
         child: Option<&str>,
         lease: Option<awaken_session_contract::SessionRealizationLease>| {
            let resources = repository_resources("source", "repo-1");
            let intent = repository_publication_intent(&resources);
            let mut session = persisted(session_id, true, "terminated");
            session.resources =
                awaken_session_contract::SessionResourceState::from_active(resources);
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
        };
    create(
        repo.as_ref(),
        requested(
            "publication-child-barrier",
            Some("publication-child"),
            Some(exact_lease.clone()),
        ),
    )
    .await;
    create(
        repo.as_ref(),
        requested("publication-cold-claim", None, None),
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

    let child_commands = application
        .terminal_cleanup_commands("publication-child-barrier", &exact_lease)
        .await
        .expect("P1 exact lease")
        .expect("P1 terminal fence");
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
    application
        .record_terminal_cleanup_completion(
            &exact_lease,
            awaken_session_contract::SessionCleanupCompletion::new(&child_commands[0], Vec::new()),
        )
        .await
        .expect("P2 child receipt");
    assert!(
        application
            .terminal_cleanup_commands("publication-child-barrier", &exact_lease)
            .await
            .unwrap()
            .unwrap()
            .is_empty(),
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
        runtime_incarnation: "worker-b:publication".into(),
        lease_expires_at_unix_ms: u64::MAX,
        renew_existing_lease: false,
        reassign_existing_lease: false,
    };
    let assignment = application
        .claim_next_terminal_cleanup(target)
        .await
        .expect("P3 claim")
        .expect("P3 publication-only assignment");
    assert_eq!(assignment.session_id, "publication-cold-claim", "P3/E3");
    assert!(
        application
            .terminal_cleanup_commands(&assignment.session_id, &assignment.lease)
            .await
            .unwrap()
            .unwrap()
            .is_empty(),
        "P3/E3"
    );
    let publication = application
        .terminal_repository_publication_command(&assignment.session_id, &assignment.lease)
        .await
        .expect("P3 publication poll")
        .expect("P3 publication projection");
    assert_eq!(publication.workspace_id, "workspace", "P3/E7");
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
            .repository_publication_receipt()
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
            .repository_publication_receipt(),
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
    let root = application
        .terminal_cleanup_commands(&assignment.session_id, &assignment.lease)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(root.len(), 1, "P5/E6");
    assert_eq!(root[0].thread_id, assignment.session_id, "P5/E6");
    application
        .record_terminal_cleanup_completion(
            &assignment.lease,
            awaken_session_contract::SessionCleanupCompletion::new(&root[0], Vec::new()),
        )
        .await
        .expect("P5 root finalizer");
    assert!(
        repo.get(&assignment.session_id)
            .await
            .unwrap()
            .terminal_cleanup
            .is_completed(),
        "P5/E6"
    );
}

#[tokio::test]
async fn cold_terminal_cleanup_claim_uses_the_existing_scan_and_fences_one_assignment() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 placement is Worker-owned; C2 cleanup has an
    // immutable Requested command; C3 the realization lease is absent, expired,
    // live under the same logical owner's predecessor incarnation, live under
    // this incarnation, or live under another owner; C4 the root CAS response is
    // lost after commit. Effects: E1 allocate epoch N+1 and return only the
    // frozen projection/lease; E2 skip ineligible/Fenced/nonterminal/local rows;
    // E3 skip an already-current assignment so a failed candidate cannot starve
    // the scan; E4 replay this invocation's exact committed assignment after an
    // ambiguous CAS response. Commands remain exclusively in terminal_cleanup.
    //
    // | Rule | C1+C2 | lease | Effect |
    // | C1 | yes | same owner, predecessor incarnation | E1 |
    // | C2 | yes | absent | E1 after C1 is current/skipped |
    // | C3 | yes | expired foreign | E1 after C2 is current/skipped |
    // | C4 | yes | exact current incarnation | E3 |
    // | C5 | yes | live foreign owner | E2 |
    // | C6 | no / Fenced | any | E2 |
    // | C7 | yes + ambiguous committed CAS | exact attempted | E4 |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("cold cleanup Session repository"),
    );
    let target = awaken_session_contract::SessionRealizationTarget {
        owner: "worker-a".into(),
        runtime_incarnation: "worker-a:2:boot-new".into(),
        lease_expires_at_unix_ms: u64::MAX,
        renew_existing_lease: false,
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
            Some(lease("worker-b", "worker-b:1:boot", 9, 1)),
        ),
    )
    .await;
    let mut fenced = persisted("claim-f-fenced", true, "terminated");
    assert!(fenced.terminal_cleanup.request("claim-f-fenced"));
    create(repo.as_ref(), fenced).await;
    let mut local = requested("claim-g-local", None);
    let awaken_session_contract::SessionBaselineState::Frozen(baseline) = &mut local.baseline
    else {
        unreachable!("fixture is frozen")
    };
    baseline.runtime_placement = SessionRuntimePlacement::Local;
    create(repo.as_ref(), local).await;
    create(
        repo.as_ref(),
        persisted("claim-h-nonterminal", true, "idle"),
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
    for (rule, expected_id, expected_epoch) in [
        ("C1", "claim-c-predecessor", 8),
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
            .expect("C4-C6")
            .is_none(),
        "C4-C6/E2+E3"
    );
    let invalid = application
        .claim_next_terminal_cleanup(awaken_session_contract::SessionRealizationTarget {
            renew_existing_lease: true,
            ..target.clone()
        })
        .await;
    assert!(
        matches!(
            invalid,
            Err(awaken_session_contract::SessionRealizationControlFailure::Invalid(_))
        ),
        "claim-next never doubles as renewal"
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
    let replayed = application
        .claim_next_terminal_cleanup(target)
        .await
        .expect("C7 ambiguous CAS replay")
        .expect("C7 assignment");
    assert_eq!(replayed.session_id, "claim-conflict", "C7/E4");
    assert_eq!(replayed.lease.epoch, 1, "C7/E4 does not allocate twice");
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

/// Legacy no-publication crash-window graph. C1 the terminal fence carries no
/// Repository publication intent; C2 cleanup fails before or after its effect;
/// C3 recovery/replay observes Requested or Completed. Effects: E1 legacy wire
/// and cleanup behavior remain unchanged; E2 no publication command/effect is
/// invented; E3 recovery reuses the exact cleanup effect id and commits the
/// receipt plus Environment removal atomically; E4 Completed is absorbing.
///
/// | Rule | publication intent | cleanup phase | Effect |
/// |---|---|---|---|
/// | L1 | absent | first effect fails | E1 + E2, Requested retained |
/// | L2 | absent | retry succeeds | E3 |
/// | L3 | absent | Completed replay | E2 + E4 |
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
/// C4=idempotent external effect, C5=exact receipt, and C6=completion CAS.
/// Effects are E1 no pre-intent I/O, E2 recover from the last committed phase,
/// E3 reuse one effect identity, E4 keep Environment authority until completion,
/// and E5 replay Completed without another effect.
///
/// | Rule | Crash point | Durable phase | Recovery effect |
/// |---|---|---|---|
/// | F0 | before C1 | NotRequested | E1; normal fence begins later |
/// | F1 | after C1/before C2 | Fenced | E2 quiesces before targets |
/// | F2 | after C2/before C3 | Fenced | E2 repeats quiesce, freezes once |
/// | F3 | after C3/before C4 | Requested | E2 executes exact frozen targets |
/// | F4 | after C4/before C5 | Requested | E3 idempotent effect replay |
/// | F5 | after C5/before C6 | Requested | E3 receipt replay, E4 retained |
/// | F6 | after C6/before response | Completed | E5 no second effect |
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

    repo.fail_once("resource-release-complete");
    application
        .release_terminal_resources("workspace", "cleanup-f0-f6")
        .await
        .expect_err("F5 completion CAS outage");
    let f5 = repo.get("cleanup-f0-f6").await.expect("F5 truth");
    assert!(f5.terminal_cleanup.is_requested(), "F5/E2");
    assert_eq!(runtime.effective_ids.lock().unwrap().len(), 1, "F5/E3");
    assert_eq!(
        f5.environment.binding(),
        Some("opaque-environment"),
        "F5/E4"
    );

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
    let attempts = runtime.intents.lock().unwrap().len();
    application
        .release_terminal_resources("workspace", "cleanup-f0-f6")
        .await
        .expect("F6 response-loss replay");
    assert_eq!(runtime.intents.lock().unwrap().len(), attempts, "F6/E5");
    assert_eq!(runtime.effective_ids.lock().unwrap().len(), 1, "F6/E5");
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
    let awaken_session_contract::SessionCleanupOperation::Completed {
        thread_ids,
        delegation_watermark,
        ..
    } = completed.terminal_cleanup
    else {
        panic!("terminal cleanup must be completed");
    };
    assert_eq!(delegation_watermark, 23);
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
