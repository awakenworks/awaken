// @formal surface exclude=test_support_crate
// @formal owner crates/stores/awaken-session-store/tests/session_repo_conformance.rs
// @formal reason: Trait-generic SessionStore (SessionRepository + LifecycleOutbox) and
// @formal reason: DeploymentRepository conformance suite (ADR-0081 §7). It is a tooling-only test library
// @formal reason: every backend's test target calls; no product crate links or invokes it, and it admits
// @formal reason: nothing at runtime.
//! Trait-generic conformance suite for the `SessionStore` port — the
//! `SessionRepository` + `LifecycleOutbox` pair — and the `DeploymentRepository`
//! decision tables that share its root CAS (ADR-0081 §7).
//!
//! Every backend runs [`run_suite`] with its own `fresh` factory and the
//! `Arc`-based race tables with its own shared handles. Root-revision CAS,
//! create receipts, scan queries, credential-source dependencies, lifecycle
//! outbox tracking, tombstones and deleted-event admissions are the port's
//! contract; a divergence is a parity bug in the backend that shows it.

use awaken_deployment_contract::{
    DeploymentAgent, DeploymentLifecycleFact, DeploymentRecord, DeploymentRepository,
    DeploymentRunRecord, DeploymentRunView, DeploymentSchedule, DeploymentStatus,
    DeploymentTrigger, DeploymentView, DeploymentWriteOutcome, ScheduledRunClaimOutcome,
};
use awaken_session_event_contract::SessionEventProjectionAnchor;
use awaken_session_mcp_contract::{McpAttachmentDraft, McpAttachmentOrigin, McpTarget};
use awaken_session_state_contract::{
    IdempotencyRecord, ManagedLifecycleFact, PersistedSession, ScopedPersistedSession,
    SessionCreateResult, SessionEnvironmentPhase, SessionExecutionState, SessionMutation,
    SessionMutationPayload, SessionMutationResult, SessionRepositoryConflict,
    SessionRepositoryError, SessionTombstone,
};
use awaken_session_types::SessionId;
use awaken_session_types::SessionRevision;
use serde_json::json;

/// Observation timestamp for a fixture record, derived from an epoch reading
/// rather than written as a date literal, so every fixture time is one value
/// on the same clock axis the stores stamp with.
#[must_use]
pub fn at(unix_seconds: u64) -> String {
    awaken_lease_contract::utc::epoch_millis_to_rfc3339(unix_seconds * 1000)
}

pub fn session(id: &str, title: &str) -> PersistedSession {
    let holder = awaken_credential_contract::PlaintextHolder::new(
        awaken_credential_contract::PlaintextBoundary::Workload,
        "awaken.workload.acp",
    );
    let environment = awaken_session_configuration_contract::EnvironmentSnapshot {
        environment_id: "env".into(),
        revision: awaken_environment_contract::EnvironmentRevision(4),
        self_hosted: false,
        config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
            "env-4".into(),
        ),
        sandbox: awaken_provisioning_contract::SandboxOverride {
            isolation: Some(awaken_provisioning_contract::IsolationClass::Namespace),
            ..Default::default()
        },
        sandbox_provisioning: Default::default(),
        idle_retention: Default::default(),
        packages: Default::default(),
        prepared_image: None,
        network: awaken_session_configuration_contract::SessionNetworkPolicy::None,
        credential_realization: awaken_credential_contract::CredentialRealizationProfile {
            inference_holder: holder.clone(),
            mcp_holder: holder.clone(),
            resource_holder: holder.clone(),
        },
    };
    let access = awaken_credential_contract::CredentialAccess::new(
        awaken_credential_contract::CredentialRef {
            id: "cred-1".into(),
            revision: 3,
        },
        awaken_credential_contract::CredentialMaterialSource::ControlPlaneReference,
        awaken_credential_contract::CredentialUsage::HttpHeader {
            name: "authorization".into(),
            scheme: Some("Bearer".into()),
        },
        awaken_credential_contract::CredentialExecutionPolicy::self_hosted_provider(),
    );
    PersistedSession {
        created_at_unix_ms: None,
        updated_at_unix_ms: None,
        session_id: SessionId::from(id),
        revision: Default::default(),
        baseline: awaken_session_configuration_contract::SessionBaselineState::Frozen(
            awaken_session_configuration_contract::SessionBaseline::compile(
                awaken_session_configuration_contract::SessionBaselineInputs {
                    environment,
                    runtime_placement:
                        awaken_session_configuration_contract::SessionRuntimePlacement::Local,
                    mcp_authoring: Default::default(),
                    agent_id: "assistant".into(),
                    agent_revision: None,
                    model_override: None,
                    model: "kimi".into(),
                    runtime: Some("acp:custom".into()),
                    delegate_ids: vec!["researcher".into()],
                    toolsets: Vec::new(),
                    mounts: Vec::new(),
                    env: Vec::new(),
                    prompts: Vec::new(),
                    transcript_prefix: None,
                },
            ),
        ),
        title: Some(title.to_string()),
        metadata: std::collections::BTreeMap::from([("k".into(), "v".into())]),
        tools: Default::default(),
        budget: Default::default(),
        event_batches: Vec::new(),
        activity_epoch: 0,
        active_activity_epochs: Default::default(),
        running_interval: None,
        closed_runtime_intervals: Vec::new(),
        runtime_active_millis: 0,
        usage_cursor: Default::default(),
        environment: Default::default(),
        mcp: awaken_session_mcp_contract::SessionMcpAttachmentSet::from_initial(
            vec![McpAttachmentDraft {
                name: "github".into(),
                target: McpTarget::parse_http("https://mcp.example").unwrap(),
                credential: Some(access),
                prompts_as_skills: false,
                origin: McpAttachmentOrigin::Agent,
            }],
            Some(holder),
        )
        .unwrap(),
        resources: awaken_session_state_contract::SessionResourceState::from_active(
            serde_json::from_value(json!({
                "inputs": [{
                    "binding_id": "input-file",
                    "source": { "kind": "file", "file_id": "file-1" },
                    "mount_path": "/mnt/input",
                    "access": "read_only"
                }]
            }))
            .unwrap(),
        ),
        realization: None,
        realization_progress: Default::default(),
        execution: SessionExecutionState::Idle,
        disposition: Default::default(),
        terminal_cleanup: Default::default(),
    }
}

pub fn hibernated_environment(
    workspace_id: &str,
    id: &str,
) -> awaken_session_state_contract::SessionEnvironmentState {
    let generation = awaken_session_state_contract::SandboxGeneration::new(
        &awaken_session_types::SessionId::from(id),
        1,
        100_000,
        "environment",
        "base-image",
    );
    let suspend = awaken_session_state_contract::SessionEnvironmentOperation::new(
        workspace_id,
        &awaken_session_types::SessionId::from(id),
        "suspend",
        &generation,
        0,
        None,
        None,
    );
    awaken_session_state_contract::SessionEnvironmentState::Hibernated {
        checkpoint: awaken_session_state_contract::SandboxCheckpointRef {
            id: format!("checkpoint-{id}"),
            format: "awaken-fs-tar-v1".into(),
            digest: format!("digest-{id}"),
            size_bytes: 1,
            created_at_unix_ms: 1,
            expires_at_unix_ms: 100_000,
            environment_fingerprint: generation.environment_fingerprint.clone(),
            base_image_fingerprint: generation.base_image_fingerprint.clone(),
            excluded_mounts: Vec::new(),
            suspend_effect_id: suspend.effect_id,
        },
        generation,
    }
}

pub fn fact(id: &str, session_id: &str, event_type: &str) -> ManagedLifecycleFact {
    ManagedLifecycleFact {
        id: id.into(),
        object_id: session_id.into(),
        workspace_id: Some("ws_a".into()),
        event_type: event_type.into(),
        timestamp: 1_700_000_000,
        runtime_interval: None,
    }
}

pub async fn create_session<R: awaken_session_repository_contract::SessionStore>(
    repo: &R,
    owner: &str,
    mut value: PersistedSession,
    facts: Vec<ManagedLifecycleFact>,
) -> PersistedSession {
    value.revision = SessionRevision(0);
    let payload = SessionMutationPayload::Replace(value.clone());
    match repo
        .create(
            owner,
            value.clone(),
            record(&format!("test:create:{}", value.session_id), &payload),
            facts,
        )
        .await
        .expect("create Session fixture")
    {
        awaken_session_state_contract::SessionCreateResult::Applied(value)
        | awaken_session_state_contract::SessionCreateResult::Replayed(value) => value,
    }
}

pub async fn replace_session<R: awaken_session_repository_contract::SessionStore>(
    repo: &R,
    owner: &str,
    mut value: PersistedSession,
    key: &str,
    facts: Vec<ManagedLifecycleFact>,
) -> PersistedSession {
    value.revision = repo
        .get(&value.session_id)
        .await
        .expect("replace Session fixture exists")
        .revision;
    let payload = SessionMutationPayload::Replace(value.clone());
    let result = repo
        .commit(
            owner,
            SessionMutation {
                expected_revision: value.revision,
                idempotency: record(key, &payload),
                payload,
                lifecycle_facts: facts,
            },
        )
        .await
        .expect("replace Session fixture");
    let revision = match result {
        SessionMutationResult::Applied { new_revision }
        | SessionMutationResult::Replayed { new_revision } => new_revision,
        other => panic!("replace Session fixture failed: {other:?}"),
    };
    value.revision = revision;
    value
}

pub fn complete_terminal_cleanup(value: &mut PersistedSession) {
    let session_id = value.session_id.clone();
    if !value.terminal_cleanup.is_fenced() && !value.terminal_cleanup.is_requested() {
        assert!(
            value.terminal_cleanup.request(&session_id),
            "cleanup operation starts exactly once"
        );
    }
    if value.terminal_cleanup.is_fenced() {
        assert!(
            value
                .terminal_cleanup
                .freeze_targets(&session_id, [], 0, 0)
                .expect("freeze root cleanup target")
        );
    }
    let command = value
        .terminal_cleanup
        .command_for(&session_id, session_id.as_str())
        .expect("requested cleanup exposes its exact root command");
    install_legacy_completion_wire(value, &command);
    assert!(
        value
            .normalize_legacy_terminal_cleanup("conformance fixture released")
            .expect("complete cleanup operation")
    );
}

/// Storage conformance needs a historical Completed aggregate, not a second
/// Runtime authoring port. Cause/effect decision rule L1: exact frozen command
/// plus canonical persisted v1 bytes may normalize; malformed/foreign bytes are
/// rejected by the contract decoder/normalizer and cannot make a row deletable.
pub fn install_legacy_completion_wire(
    session: &mut PersistedSession,
    command: &awaken_session_state_contract::SessionCleanupCommand,
) {
    let receipt_fingerprint = awaken_agent_contract::stable_fingerprint(&(
        "session-terminal-cleanup-thread-receipt-v1",
        command.session_id.as_str(),
        command.thread_id.as_str(),
        command.effect_id.as_str(),
        Vec::<(&str, &str)>::new(),
    ));
    let completion = json!({
        "session_id": command.session_id,
        "thread_id": command.thread_id,
        "effect_id": command.effect_id,
        "artifact_receipts": [],
        "receipt_fingerprint": receipt_fingerprint,
    });
    let mut encoded = serde_json::to_value(&*session).unwrap();
    let cleanup = encoded
        .get_mut("terminal_cleanup")
        .expect("PersistedSession wire contains terminal_cleanup");
    let cleanup = if cleanup.get("state").and_then(serde_json::Value::as_str)
        == Some("repository_publication")
    {
        cleanup
            .get_mut("cleanup")
            .expect("publication wire contains its cleanup")
    } else {
        cleanup
    };
    cleanup
        .as_object_mut()
        .expect("cleanup wire is an object")
        .entry("completions")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("legacy completions wire is an object")
        .insert(command.thread_id.clone(), completion);
    *session = serde_json::from_value(encoded).unwrap();
}

// ── The universal port contract, trait-generic over any backend ──────────────────

/// Round-trip: a saved aggregate reads back byte-for-byte (every field persists).
pub async fn save_get_round_trips<R: awaken_session_repository_contract::SessionStore>(r: &R) {
    let want = create_session(r, "default", session("sesn_1", "hello"), Vec::new()).await;
    assert_eq!(
        r.get(&SessionId::from("sesn_1")).await,
        Ok(want),
        "the full aggregate must round-trip"
    );
}

/// Absent id → typed NotFound (no fabrication, fail-closed read).
pub async fn absent_id_reads_none<R: awaken_session_repository_contract::SessionStore>(r: &R) {
    assert_eq!(
        r.get(&SessionId::from("never-saved")).await,
        Err(SessionRepositoryError::NotFound)
    );
}

/// Idempotent upsert: saving the same id twice keeps the latest, not two rows.
pub async fn save_is_idempotent_upsert<R: awaken_session_repository_contract::SessionStore>(r: &R) {
    create_session(r, "default", session("sesn_1", "first"), Vec::new()).await;
    replace_session(
        r,
        "default",
        session("sesn_1", "second"),
        "test:replace:second",
        Vec::new(),
    )
    .await;
    assert_eq!(
        r.get(&SessionId::from("sesn_1")).await.unwrap().title,
        Some("second".into())
    );
}

/// A visible row and its owner are one write: no backend may expose the row with
/// a missing or stale scope after aggregate creation or replacement returns.
pub async fn ownership_is_one_atomic_repository_fact<
    R: awaken_session_repository_contract::SessionStore,
>(
    r: &R,
) {
    create_session(r, "ws_a", session("sesn_owned", "owned"), Vec::new()).await;
    assert!(r.get(&SessionId::from("sesn_owned")).await.is_ok());
    assert_eq!(
        r.owner(&SessionId::from("sesn_owned")).await.as_deref(),
        Ok("ws_a")
    );

    replace_session(
        r,
        "ws_a",
        session("sesn_owned", "updated"),
        "test:replace:owned",
        Vec::new(),
    )
    .await;
    assert_eq!(
        r.owner(&SessionId::from("sesn_owned")).await.as_deref(),
        Ok("ws_a")
    );
    assert_eq!(
        r.get(&SessionId::from("sesn_owned")).await.unwrap().title,
        Some("updated".into())
    );
}

/// Binding is a narrow update: it fails closed for an unknown id and changes no
/// other aggregate field for a known Session.
pub async fn environment_state_is_atomic_and_non_destructive<
    R: awaken_session_repository_contract::SessionStore,
>(
    r: &R,
) {
    assert_eq!(
        r.get(&SessionId::from("unknown")).await,
        Err(SessionRepositoryError::NotFound)
    );
    let want = session("sesn_bound", "unchanged");
    create_session(r, "ws_a", want.clone(), Vec::new()).await;
    let mut bound = want.clone();
    bound
        .environment
        .set_resident(r#"{"provider_kind":"bwrap"}"#);
    replace_session(r, "ws_a", bound, "test:bind", Vec::new()).await;
    let mut got = r
        .get(&SessionId::from("sesn_bound"))
        .await
        .expect("bound Session");
    assert_eq!(
        got.environment.binding(),
        Some(r#"{"provider_kind":"bwrap"}"#)
    );
    got.environment = awaken_session_state_contract::SessionEnvironmentState::Unmaterialized;
    got.revision = Default::default();
    assert_eq!(got, want, "binding update preserves every other field");
    assert_eq!(
        r.owner(&SessionId::from("sesn_bound")).await.as_deref(),
        Ok("ws_a")
    );
}

pub async fn global_environment_phase_count_has_no_recovery_batch_limit<
    R: awaken_session_repository_contract::SessionStore,
>(
    repo: &R,
) {
    // Cause/effect graph: C1 the canonical store has 257 live Session roots;
    // C2 the lexically last root moves from Hibernated to Restoring; C3 the
    // recovery work scan is bounded to 256. Effects: E1 the global count reads
    // all 257 canonical roots rather than the recovery batch; E2 it reports the
    // exact typed Restoring phase and no JSON/tag-specific parallel query.
    //
    // | Rule | Live roots | Last root | Selected phase | Effect |
    // |---|---:|---|---|---|
    // | P1 | 257 | Hibernated | Unmaterialized | 256 |
    // | P2 | 257 | Restoring | Restoring | 1 |
    let mut last = None;
    for index in 0..257 {
        let id = format!("phase-{index:03}");
        let created = create_session(repo, "ws_a", session(&id, &id), Vec::new()).await;
        if index == 256 {
            last = Some(created);
        }
    }
    let mut last = last.expect("P1 lexically last root");
    let last_id = last.session_id.clone();
    last.environment = hibernated_environment("ws_a", last_id.as_str());
    last.environment
        .begin_restore("ws_a", &last_id, 0, None, 1)
        .expect("P2 valid restore intent");
    replace_session(repo, "ws_a", last, "test:phase-count:restoring", Vec::new()).await;

    assert_eq!(
        repo.count_environment_phase(SessionEnvironmentPhase::Unmaterialized)
            .await
            .unwrap(),
        256,
        "P1/E1"
    );
    assert_eq!(
        repo.count_environment_phase(SessionEnvironmentPhase::Restoring)
            .await
            .unwrap(),
        1,
        "P2/E2"
    );
}

pub async fn concurrent_phase_transition_has_one_count_snapshot<R>(repo: std::sync::Arc<R>)
where
    R: awaken_session_repository_contract::SessionStore + 'static,
{
    // Causes: C1 one Hibernated root; C2 its root-CAS transition to Restoring
    // races one global count statement. Effects: E1 the count observes either
    // the complete before or after snapshot (baseline or baseline+1), never a
    // mixed/error result; E2 the later count observes the committed increment.
    // Decision rules: S1=count<CAS => baseline; S2=CAS<count => baseline+1;
    // both => E2 final baseline+1.
    let baseline = repo
        .count_environment_phase(SessionEnvironmentPhase::Restoring)
        .await
        .unwrap();
    let id = "phase-concurrent";
    let mut current = session(id, id);
    current.environment = hibernated_environment("ws_a", id);
    let current = create_session(repo.as_ref(), "ws_a", current, Vec::new()).await;
    let mut restoring = current.clone();
    restoring
        .environment
        .begin_restore(
            "ws_a",
            &awaken_session_types::SessionId::from(id),
            0,
            None,
            1,
        )
        .expect("C2 valid restore intent");
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let count = {
        let repo = repo.clone();
        let barrier = barrier.clone();
        async move {
            barrier.wait().await;
            repo.count_environment_phase(SessionEnvironmentPhase::Restoring)
                .await
        }
    };
    let transition = {
        let repo = repo.clone();
        async move {
            barrier.wait().await;
            replace_session(
                repo.as_ref(),
                "ws_a",
                restoring,
                "test:phase-count:concurrent",
                Vec::new(),
            )
            .await
        }
    };
    let (observed, _) = tokio::join!(count, transition);
    assert!(
        matches!(observed, Ok(count) if count == baseline || count == baseline + 1),
        "S1/S2/E1: {observed:?}"
    );
    assert_eq!(
        repo.count_environment_phase(SessionEnvironmentPhase::Restoring)
            .await
            .unwrap(),
        baseline + 1,
        "E2"
    );
}

pub async fn concurrent_root_cas_has_one_authoritative_winner<R>(repo: std::sync::Arc<R>)
where
    R: awaken_session_repository_contract::SessionStore + 'static,
{
    /* Multi-instance cause/effect graph: C1 two independent callers read the
     * same Session revision; C2 they submit different replacement commands at
     * one barrier; C3 the shared repository owns the compare-and-swap. Effects:
     * E1 exactly one command commits; E2 the loser observes RevisionMismatch;
     * E3 the durable aggregate equals the complete winning candidate; E4 the
     * losing candidate is never partially merged. This is the cross-process
     * authority boundary; a Worker-local Mutex is deliberately absent here.
     *
     * | Rule | snapshots | payloads | repository | Effect |
     * |---|---|---|---|---|
     * | M1 | same revision | different | shared CAS | E1 + E2 + E3 + E4 |
     */
    let current = create_session(
        repo.as_ref(),
        "ws_a",
        session("sesn_multi_instance_cas", "before"),
        Vec::new(),
    )
    .await;
    let mutation = |title: &str, key: &str| {
        let mut replacement = current.clone();
        replacement.title = Some(title.into());
        let payload = SessionMutationPayload::Replace(replacement);
        SessionMutation {
            expected_revision: current.revision,
            idempotency: record(key, &payload),
            payload,
            lifecycle_facts: Vec::new(),
        }
    };
    let left_mutation = mutation("left", "test:multi-instance-cas:left");
    let right_mutation = mutation("right", "test:multi-instance-cas:right");
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let submit = |mutation: SessionMutation| {
        let repo = repo.clone();
        let barrier = barrier.clone();
        async move {
            barrier.wait().await;
            let title = match &mutation.payload {
                SessionMutationPayload::Replace(session) => session.title.clone().unwrap(),
                SessionMutationPayload::Delete(_) => unreachable!("M1 replacement only"),
            };
            (title, repo.commit("ws_a", mutation).await)
        }
    };
    let (left, right) = tokio::join!(submit(left_mutation), submit(right_mutation));
    let outcomes = [left, right];
    assert_eq!(
        outcomes
            .iter()
            .filter(|(_, result)| matches!(result, Ok(SessionMutationResult::Applied { .. })))
            .count(),
        1,
        "M1/E1"
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|(_, result)| matches!(result, Ok(SessionMutationResult::Conflict { .. })))
            .count(),
        1,
        "M1/E2"
    );
    let winner = outcomes
        .iter()
        .find_map(|(title, result)| {
            matches!(result, Ok(SessionMutationResult::Applied { .. })).then_some(title)
        })
        .expect("M1 one winning candidate");
    let durable = repo
        .get(&SessionId::from("sesn_multi_instance_cas"))
        .await
        .expect("M1 durable winner");
    assert_eq!(durable.title.as_ref(), Some(winner), "M1/E3+E4");
    assert_eq!(durable.revision, SessionRevision(2), "M1/E3");
}

pub async fn vault_reference_index_returns_only_live_scoped_sessions<
    R: awaken_session_repository_contract::SessionStore,
>(
    r: &R,
) {
    let with_vault = |id: &str, execution| {
        let mut value = session(id, id);
        value.execution = execution;
        let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
            &mut value.baseline
        else {
            unreachable!("fixture baseline is frozen")
        };
        baseline.mcp_authoring.ordered_vault_ids = vec!["vlt-live".into()];
        value
    };
    let live = create_session(
        r,
        "ws_a",
        with_vault("sesn_vault_live", SessionExecutionState::Idle),
        Vec::new(),
    )
    .await;
    create_session(
        r,
        "ws_a",
        with_vault("sesn_vault_terminal", SessionExecutionState::Terminated),
        Vec::new(),
    )
    .await;
    create_session(
        r,
        "ws_b",
        with_vault("sesn_vault_other_workspace", SessionExecutionState::Idle),
        Vec::new(),
    )
    .await;

    assert_eq!(
        r.scan(
            awaken_session_state_contract::SessionScanQuery::VaultReferences {
                workspace_id: "ws_a",
                vault_id: "vlt-live"
            }
        )
        .await
        .map(awaken_session_state_contract::SessionScan::into_sessions)
        .unwrap(),
        vec![live],
        "rollout targets only live Sessions in the exact Workspace"
    );
    assert!(
        r.scan(
            awaken_session_state_contract::SessionScanQuery::VaultReferences {
                workspace_id: "ws_a",
                vault_id: "vlt-other"
            }
        )
        .await
        .map(awaken_session_state_contract::SessionScan::into_sessions)
        .unwrap()
        .is_empty()
    );
}

pub async fn scan_query_decision_table<R: awaken_session_repository_contract::SessionStore>(r: &R) {
    // Scan query cause/effect graph. C1 the query is the Workspace listing,
    // a reference index, or the reconciliation index; C2 a row belongs to the
    // queried Workspace or another one; C3 the row is live or terminal.
    // Effects: E1 a Workspace listing returns every row of exactly that
    // Workspace (terminal included) in id order; E2 reference indexes return
    // only live rows; E3 every unpaged query carries its Workspace, no
    // quarantine evidence and no cursor; E4 the reconciliation index scopes
    // each row by its persisted owner; E5 the lifecycle outbox is the same
    // store's drain port, not a second queue.
    //
    // | Rule | C1          | C2      | C3       | Effect  |
    // |------|-------------|---------|----------|---------|
    // | Q1   | Workspace   | same    | any      | E1+E3   |
    // | Q2   | Workspace   | other   | any      | absent  |
    // | Q3   | Workspace   | none    | -        | empty   |
    // | Q4   | Vault index | same    | terminal | E2+E3   |
    // | Q5   | Reconcile   | any     | any      | E4      |
    // | Q6   | outbox view | -       | -        | E5      |
    use awaken_session_repository_contract::LifecycleOutbox;
    use awaken_session_state_contract::SessionScanQuery;
    let with_vault = |id: &str, execution| {
        let mut value = session(id, id);
        value.execution = execution;
        let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
            &mut value.baseline
        else {
            unreachable!("fixture baseline is frozen")
        };
        baseline.mcp_authoring.ordered_vault_ids = vec!["vlt-scan".into()];
        value
    };
    let live = create_session(
        r,
        "ws_scan",
        with_vault("sesn_scan_a", SessionExecutionState::Idle),
        vec![fact("fact_scan_a", "sesn_scan_a", "session.created")],
    )
    .await;
    let terminal = create_session(
        r,
        "ws_scan",
        with_vault("sesn_scan_b", SessionExecutionState::Terminated),
        Vec::new(),
    )
    .await;
    create_session(
        r,
        "ws_scan_other",
        with_vault("sesn_scan_c", SessionExecutionState::Idle),
        Vec::new(),
    )
    .await;

    let listing = r
        .scan(SessionScanQuery::Workspace {
            workspace_id: "ws_scan",
        })
        .await
        .expect("Q1 Workspace listing");
    assert_eq!(listing.quarantined, Vec::new(), "Q1/E3");
    assert_eq!(listing.next_cursor, None, "Q1/E3");
    assert!(
        listing
            .sessions
            .iter()
            .all(|scoped| scoped.workspace_id == "ws_scan"),
        "Q1/E3 scoped by the queried Workspace"
    );
    assert_eq!(
        listing.into_sessions(),
        vec![live.clone(), terminal],
        "Q1/Q2/E1 exact Workspace rows, terminal included, in id order"
    );
    assert!(
        r.scan(SessionScanQuery::Workspace {
            workspace_id: "ws_scan_absent",
        })
        .await
        .expect("Q3 empty Workspace listing")
        .sessions
        .is_empty(),
        "Q3"
    );

    let referenced = r
        .scan(SessionScanQuery::VaultReferences {
            workspace_id: "ws_scan",
            vault_id: "vlt-scan",
        })
        .await
        .expect("Q4 Vault index");
    assert_eq!(referenced.next_cursor, None, "Q4/E3");
    assert!(referenced.quarantined.is_empty(), "Q4/E3");
    assert_eq!(referenced.into_sessions(), vec![live], "Q4/E2");

    let mut cursor = None;
    loop {
        let page = r
            .scan(SessionScanQuery::Reconcilable {
                after: cursor.as_ref(),
            })
            .await
            .expect("Q5 reconciliation page");
        for scoped in &page.sessions {
            assert_eq!(
                r.owner(&scoped.session.session_id).await.unwrap(),
                scoped.workspace_id,
                "Q5/E4"
            );
        }
        let Some(next) = page.next_cursor else {
            break;
        };
        cursor = Some(next);
    }

    let outbox: &dyn LifecycleOutbox = r;
    let pending = outbox.pending_lifecycle().await.expect("Q6 outbox view");
    assert!(
        pending.iter().any(|fact| fact.id == "fact_scan_a"),
        "Q6/E5 the create transaction's fact is visible through the drain port"
    );
    outbox
        .complete_lifecycle("fact_scan_a")
        .await
        .expect("Q6 retire");
    assert!(
        !outbox
            .pending_lifecycle()
            .await
            .unwrap()
            .iter()
            .any(|fact| fact.id == "fact_scan_a"),
        "Q6/E5 retirement is durable"
    );
    assert_eq!(
        r.get(&"sesn_scan_a".into()).await.unwrap().revision,
        SessionRevision(1),
        "Q6 draining the outbox never touches the aggregate"
    );
}

pub fn with_mcp_credential_source(
    mut value: PersistedSession,
    source_id: &str,
) -> PersistedSession {
    value.mcp.attachments[0]
        .credential
        .as_mut()
        .expect("fixture MCP credential")
        .credential
        .id = source_id.to_string();
    value
}

pub async fn credential_source_dependency_decision_table<
    R: awaken_session_repository_contract::SessionStore,
>(
    r: &R,
) {
    // Actual-source dependency cause/effect graph. C1 root is live and its
    // current desired MCP generation pins source A; C2 the same source belongs
    // to another Workspace; C3 a same-Workspace root pins unrelated source B;
    // C4 a root CAS replaces A with C; C5 that root becomes archived; C6 a
    // second A root is tombstoned. Effects: E1 discovery returns only exact
    // Workspace+source live roots; E2 unrelated roots are unchanged; E3 replace
    // removes A and adds C in the same root transaction; E4 archive is excluded;
    // E5 delete cascades its dependency. The immutable Vault-authoring index is
    // deliberately absent from every cause and retains its separate contract.
    //
    // | Rule | Workspace | desired source | root transition | Effect |
    // | S1 | match | A | live | E1 include |
    // | S2 | other | A | live | E1 exclude |
    // | S3 | match | B | live | E1 exclude A + E2 preserve B |
    // | S4 | match | A -> C | replace | E3 |
    // | S5 | match | C | archive | E4 |
    // | S6 | match | A | delete | E5 |
    let source = |id: &str| awaken_credential_contract::CredentialSourceId(id.into());
    let updated = create_session(
        r,
        "ws_a",
        with_mcp_credential_source(session("sesn_source_update", "update"), "source-a"),
        Vec::new(),
    )
    .await;
    let deleted = create_session(
        r,
        "ws_a",
        with_mcp_credential_source(session("sesn_source_delete", "delete"), "source-a"),
        Vec::new(),
    )
    .await;
    create_session(
        r,
        "ws_b",
        with_mcp_credential_source(session("sesn_source_other_ws", "other ws"), "source-a"),
        Vec::new(),
    )
    .await;
    let unrelated = create_session(
        r,
        "ws_a",
        with_mcp_credential_source(session("sesn_source_unrelated", "unrelated"), "source-b"),
        Vec::new(),
    )
    .await;
    assert_eq!(
        r.scan(
            awaken_session_state_contract::SessionScanQuery::CredentialSourceReferences {
                workspace_id: "ws_a",
                source_id: &source("source-a")
            }
        )
        .await
        .map(awaken_session_state_contract::SessionScan::into_sessions)
        .unwrap(),
        vec![deleted.clone(), updated.clone()],
        "S1-S3/E1"
    );

    let updated = replace_session(
        r,
        "ws_a",
        with_mcp_credential_source(updated, "source-c"),
        "test:source:a-to-c",
        Vec::new(),
    )
    .await;
    assert_eq!(
        r.scan(
            awaken_session_state_contract::SessionScanQuery::CredentialSourceReferences {
                workspace_id: "ws_a",
                source_id: &source("source-a")
            }
        )
        .await
        .map(awaken_session_state_contract::SessionScan::into_sessions)
        .unwrap(),
        vec![deleted.clone()],
        "S4/E3 old dependency removed"
    );
    assert_eq!(
        r.scan(
            awaken_session_state_contract::SessionScanQuery::CredentialSourceReferences {
                workspace_id: "ws_a",
                source_id: &source("source-c")
            }
        )
        .await
        .map(awaken_session_state_contract::SessionScan::into_sessions)
        .unwrap(),
        vec![updated.clone()],
        "S4/E3 new dependency added"
    );

    let mut archived = updated;
    archived.archive(at(1_787_788_800)).unwrap();
    replace_session(r, "ws_a", archived, "test:source:archive", Vec::new()).await;
    assert!(
        r.scan(
            awaken_session_state_contract::SessionScanQuery::CredentialSourceReferences {
                workspace_id: "ws_a",
                source_id: &source("source-c")
            }
        )
        .await
        .map(awaken_session_state_contract::SessionScan::into_sessions)
        .unwrap()
        .is_empty(),
        "S5/E4"
    );

    let mut deleting = deleted;
    assert!(deleting.request_delete("ws_a", 0).unwrap().is_some());
    complete_terminal_cleanup(&mut deleting);
    let deleting =
        replace_session(r, "ws_a", deleting, "test:source:delete-ready", Vec::new()).await;
    assert!(matches!(
        r.commit("ws_a", delete_mutation(&deleting, "test:source:delete"))
            .await
            .unwrap(),
        SessionMutationResult::Applied { .. }
    ));
    assert!(
        r.scan(
            awaken_session_state_contract::SessionScanQuery::CredentialSourceReferences {
                workspace_id: "ws_a",
                source_id: &source("source-a")
            }
        )
        .await
        .map(awaken_session_state_contract::SessionScan::into_sessions)
        .unwrap()
        .is_empty(),
        "S6/E5"
    );
    assert_eq!(
        r.scan(
            awaken_session_state_contract::SessionScanQuery::CredentialSourceReferences {
                workspace_id: "ws_a",
                source_id: &source("source-b")
            }
        )
        .await
        .map(awaken_session_state_contract::SessionScan::into_sessions)
        .unwrap(),
        vec![unrelated],
        "S3/E2"
    );
}

/// The lifecycle fact is committed in the same repository transaction as the
/// aggregate and owner. Notification may crash afterwards without losing the fact.
pub async fn lifecycle_outbox_tracks_every_committed_transition<
    R: awaken_session_repository_contract::SessionStore,
>(
    r: &R,
) {
    // Cause/effect rule: C1=root mutation and typed interval fact share one
    // transaction; C2=process/adapter reload. E1=the aggregate and exact
    // millisecond interval appear together; E2=the typed payload round-trips
    // without the store maintaining a parallel event schema.
    fn assert_exact_outbox_delta(
        before: &[ManagedLifecycleFact],
        after: &[ManagedLifecycleFact],
        added: &ManagedLifecycleFact,
        rule: &str,
    ) {
        let mut remaining = after.to_vec();
        let position = remaining
            .iter()
            .position(|candidate| candidate == added)
            .unwrap_or_else(|| panic!("{rule}: exact lifecycle fact is present"));
        remaining.remove(position);
        assert_eq!(remaining, before, "{rule}: no unrelated outbox change");
    }

    let outbox_before = r.pending_lifecycle().await.unwrap();
    let mut created = fact(
        "evt:create",
        "sesn_lifecycle",
        "session.runtime_interval_closed",
    );
    created.runtime_interval = Some(awaken_session_state_contract::SessionRuntimeInterval {
        interval_id: "evt:create".into(),
        activity_epoch: 7,
        started_at_unix_ms: 1_700_000_000_100,
        ended_at_unix_ms: 1_700_000_000_900,
        opened_revision: Default::default(),
        closed_revision: Default::default(),
        observations: Vec::new(),
        usage: Default::default(),
        max_list_cost_minor: None,
    });
    create_session(
        r,
        "ws_a",
        session("sesn_lifecycle", "lifecycle"),
        vec![created.clone()],
    )
    .await;
    assert!(r.get(&SessionId::from("sesn_lifecycle")).await.is_ok());
    assert_eq!(
        r.owner(&SessionId::from("sesn_lifecycle")).await.as_deref(),
        Ok("ws_a")
    );
    assert_exact_outbox_delta(
        &outbox_before,
        &r.pending_lifecycle().await.unwrap(),
        &created,
        "create",
    );

    // Stable event identity makes an enqueue retry a no-op.
    r.append_lifecycle(created.clone()).await.unwrap();
    assert_exact_outbox_delta(
        &outbox_before,
        &r.pending_lifecycle().await.unwrap(),
        &created,
        "create retry",
    );
    r.complete_lifecycle(&created.id).await.unwrap();
    assert_eq!(r.pending_lifecycle().await.unwrap(), outbox_before);

    let archived = fact("evt:archive", "sesn_lifecycle", "session.archived");
    let mut archive = r.get(&SessionId::from("sesn_lifecycle")).await.unwrap();
    archive.archive(at(1_784_419_200)).unwrap();
    replace_session(r, "ws_a", archive, "test:archive", vec![archived.clone()]).await;
    let durable = r
        .get(&SessionId::from("sesn_lifecycle"))
        .await
        .expect("archived session");
    assert_eq!(durable.execution, SessionExecutionState::Terminated);
    assert_eq!(durable.archived_at(), Some(at(1_784_419_200).as_str()));
    assert_exact_outbox_delta(
        &outbox_before,
        &r.pending_lifecycle().await.unwrap(),
        &archived,
        "archive",
    );
    r.complete_lifecycle(&archived.id).await.unwrap();

    let deleted = fact("evt:delete", "sesn_lifecycle", "session.deleted");
    let mut current = r.get(&SessionId::from("sesn_lifecycle")).await.unwrap();
    assert!(
        current.request_delete("ws_a", 0).unwrap().is_some(),
        "archive remains deletable"
    );
    complete_terminal_cleanup(&mut current);
    let current = replace_session(
        r,
        "ws_a",
        current,
        "test:delete:cleanup-complete",
        Vec::new(),
    )
    .await;
    let payload = SessionMutationPayload::Delete(SessionTombstone {
        session_id: current.session_id.clone(),
        deleted_revision: SessionRevision(current.revision.0 + 1),
        deleted_at: deleted.timestamp.to_string(),
    });
    assert!(matches!(
        r.commit(
            "ws_a",
            SessionMutation {
                expected_revision: current.revision,
                idempotency: record("test:delete", &payload),
                payload,
                lifecycle_facts: vec![deleted.clone()],
            },
        )
        .await
        .unwrap(),
        SessionMutationResult::Applied { .. }
    ));
    assert_eq!(
        r.get(&SessionId::from("sesn_lifecycle")).await,
        Err(SessionRepositoryError::NotFound)
    );
    assert_exact_outbox_delta(
        &outbox_before,
        &r.pending_lifecycle().await.unwrap(),
        &deleted,
        "delete",
    );
}

pub async fn lifecycle_outbox_insert_failure_rolls_back_root_paths<R, Arm, Armed>(
    repo: &R,
    arm_outbox_failure: Arm,
) where
    R: awaken_session_repository_contract::SessionStore,
    Arm: FnOnce() -> Armed,
    Armed: std::future::Future<Output = ()>,
{
    // Cause/effect graph: C1 one of create/replace/delete changes the Session
    // root; C2 the same command carries one exact lifecycle fact; C3 the
    // database rejects that outbox INSERT after the root write; C4 the adapter
    // returns the transaction error. Effects: E1 no aggregate/tombstone change;
    // E2 no idempotency receipt; E3 no outbox change. Decision table:
    // | Rule | root path | C2 | C3 | Effect |
    // |---|---|---|---|---|
    // | A1 | create | T | T | E1 absent + E2 + E3 |
    // | A2 | replace | T | T | E1 exact predecessor + E2 + E3 |
    // | A3 | delete | T | T | E1 live predecessor + E2 + E3 |
    // The rejection is installed in the real database, after fixtures commit;
    // neither adapter nor repository statement is stubbed. Engine transaction
    // atomicity remains TCB, while these rules cross-check its actual rollback.
    let replace_predecessor = create_session(
        repo,
        "ws_a",
        session("sesn_atomic_replace", "replace predecessor"),
        Vec::new(),
    )
    .await;
    let mut delete_predecessor = session("sesn_atomic_delete", "delete predecessor");
    assert!(
        delete_predecessor
            .request_delete("ws_a", 0)
            .unwrap()
            .is_some(),
        "A3 fixture enters deleting"
    );
    complete_terminal_cleanup(&mut delete_predecessor);
    let delete_predecessor = create_session(repo, "ws_a", delete_predecessor, Vec::new()).await;
    let outbox_before = repo.pending_lifecycle().await.unwrap();

    arm_outbox_failure().await;

    let create_candidate = session("sesn_atomic_create", "create candidate");
    let create_payload = SessionMutationPayload::Replace(create_candidate.clone());
    let create_key = "test:atomic:create";
    assert!(
        matches!(
            repo.create(
                "ws_a",
                create_candidate,
                record(create_key, &create_payload),
                vec![fact(
                    "evt:atomic:create",
                    "sesn_atomic_create",
                    "session.status_idled",
                )],
            )
            .await,
            Err(SessionRepositoryError::AuthorityUnavailable(_))
        ),
        "A1 injected outbox failure reaches the transaction boundary"
    );
    assert_eq!(
        repo.get(&SessionId::from("sesn_atomic_create")).await,
        Err(SessionRepositoryError::NotFound),
        "A1/E1"
    );
    assert_eq!(
        repo.idempotency_receipt(&SessionId::from("sesn_atomic_create"), create_key)
            .await,
        Ok(None),
        "A1/E2"
    );
    assert_eq!(
        repo.pending_lifecycle().await.unwrap(),
        outbox_before,
        "A1/E3"
    );

    let mut replacement = replace_predecessor.clone();
    replacement.title = Some("rejected replacement".into());
    let replace_payload = SessionMutationPayload::Replace(replacement);
    let replace_key = "test:atomic:replace";
    assert!(
        matches!(
            repo.commit(
                "ws_a",
                SessionMutation {
                    expected_revision: replace_predecessor.revision,
                    idempotency: record(replace_key, &replace_payload),
                    payload: replace_payload,
                    lifecycle_facts: vec![fact(
                        "evt:atomic:replace",
                        replace_predecessor.session_id.as_str(),
                        "session.archived",
                    )],
                },
            )
            .await,
            Err(SessionRepositoryError::AuthorityUnavailable(_))
        ),
        "A2 injected outbox failure reaches the transaction boundary"
    );
    assert_eq!(
        repo.get(&replace_predecessor.session_id).await,
        Ok(replace_predecessor.clone()),
        "A2/E1"
    );
    assert_eq!(
        repo.idempotency_receipt(&replace_predecessor.session_id, replace_key)
            .await,
        Ok(None),
        "A2/E2"
    );
    assert_eq!(
        repo.pending_lifecycle().await.unwrap(),
        outbox_before,
        "A2/E3"
    );

    let delete_payload = SessionMutationPayload::Delete(SessionTombstone {
        session_id: delete_predecessor.session_id.clone(),
        deleted_revision: SessionRevision(delete_predecessor.revision.0 + 1),
        deleted_at: at(1_788_998_400),
    });
    let delete_key = "test:atomic:delete";
    assert!(
        matches!(
            repo.commit(
                "ws_a",
                SessionMutation {
                    expected_revision: delete_predecessor.revision,
                    idempotency: record(delete_key, &delete_payload),
                    payload: delete_payload,
                    lifecycle_facts: vec![fact(
                        "evt:atomic:delete",
                        delete_predecessor.session_id.as_str(),
                        "session.deleted",
                    )],
                },
            )
            .await,
            Err(SessionRepositoryError::AuthorityUnavailable(_))
        ),
        "A3 injected outbox failure reaches the transaction boundary"
    );
    assert_eq!(
        repo.get(&delete_predecessor.session_id).await,
        Ok(delete_predecessor.clone()),
        "A3/E1"
    );
    assert_eq!(
        repo.deleted_event_admissions("ws_a", &delete_predecessor.session_id)
            .await,
        Ok(None),
        "A3/E1 no tombstone"
    );
    assert_eq!(
        repo.idempotency_receipt(&delete_predecessor.session_id, delete_key)
            .await,
        Ok(None),
        "A3/E2"
    );
    assert_eq!(
        repo.pending_lifecycle().await.unwrap(),
        outbox_before,
        "A3/E3"
    );
}

/// Reconciliation-index cause/effect rules: C1=Resource/MCP durable work is
/// pending -> E1=index the Session; C2=all work and retention references are
/// terminal -> E2=remove it; C3=only the durable Environment identity is
/// resident -> E3=index it through the same aggregate scan for environment
/// restoration; C4=an active Resource manifest still owns retention references
/// -> E4=keep it indexed even after MCP work becomes terminal, so a lost
/// ResourceReference projection can be repaired. Constraint: this opaque binding
/// is not Hand activity/process state; Hand inactivity remains solely in the
/// Worker-local Runtime Host and adds no second repository predicate.
///
/// | Rule | Resource work | MCP work | References | Environment | Effect |
/// |---|---|---|---|---|---|
/// | I1 | pending | active | yes | absent | E1 indexed |
/// | I2 | settled | failed | yes | absent | E4 indexed for reference repair |
/// | I3 | terminal | failed | no | absent | E2 absent |
/// | I4 | terminal | failed | no | resident | E3 indexed for restoration |
pub async fn pending_resource_activation_index_is_durable<
    R: awaken_session_repository_contract::SessionStore,
>(
    r: &R,
) {
    let mut pending = session("sesn_pending", "pending");
    let desired = pending.resources.active.clone();
    pending.resources = Default::default();
    pending
        .resources
        .prepare(&pending.session_id, desired)
        .unwrap();
    pending = create_session(r, "ws_a", pending, Vec::new()).await;

    assert_eq!(
        r.scan(awaken_session_state_contract::SessionScanQuery::Reconcilable { after: None })
            .await
            .unwrap()
            .sessions,
        vec![ScopedPersistedSession {
            workspace_id: "ws_a".into(),
            session: pending.clone(),
        }]
    );

    pending.resources.start_attempt().unwrap();
    pending.resources.commit().unwrap();
    pending = replace_session(r, "ws_a", pending, "test:resource-active", Vec::new()).await;
    let indexed = r
        .scan(awaken_session_state_contract::SessionScanQuery::Reconcilable { after: None })
        .await
        .unwrap();
    assert_eq!(indexed.sessions.len(), 1, "active MCP remains restart work");
    assert!(indexed.sessions[0].session.mcp.needs_reconciliation());

    pending.mcp.attachments[0].state = awaken_session_mcp_contract::McpAttachmentState::Failed;
    pending = replace_session(r, "ws_a", pending, "test:mcp-failed", Vec::new()).await;
    assert_eq!(
        r.scan(awaken_session_state_contract::SessionScanQuery::Reconcilable { after: None })
            .await
            .unwrap()
            .sessions,
        vec![ScopedPersistedSession {
            workspace_id: "ws_a".into(),
            session: pending.clone(),
        }],
        "I2/E4: active Resource retention remains recoverable"
    );

    pending.execution = SessionExecutionState::Terminated;
    pending
        .resources
        .complete_terminal_release("conformance cleanup");
    pending = replace_session(r, "ws_a", pending, "test:resource-released", Vec::new()).await;
    assert!(
        r.scan(awaken_session_state_contract::SessionScanQuery::Reconcilable { after: None })
            .await
            .unwrap()
            .sessions
            .is_empty()
    );

    pending.environment.set_resident("worker-owned-binding");
    pending = replace_session(r, "ws_a", pending, "test:resident-environment", Vec::new()).await;
    assert_eq!(
        r.scan(awaken_session_state_contract::SessionScanQuery::Reconcilable { after: None })
            .await
            .unwrap()
            .sessions,
        vec![ScopedPersistedSession {
            workspace_id: "ws_a".into(),
            session: pending,
        }],
        "C3/E3: restore durable Environment identity without persisting Hand activity"
    );
}

mod cas;
pub use cas::*;
mod deployment;
pub use deployment::*;

pub async fn run_suite<R: awaken_session_repository_contract::SessionStore + 'static>(
    fresh: impl Fn() -> R,
) {
    save_get_round_trips(&fresh()).await;
    absent_id_reads_none(&fresh()).await;
    save_is_idempotent_upsert(&fresh()).await;
    ownership_is_one_atomic_repository_fact(&fresh()).await;
    environment_state_is_atomic_and_non_destructive(&fresh()).await;
    global_environment_phase_count_has_no_recovery_batch_limit(&fresh()).await;
    vault_reference_index_returns_only_live_scoped_sessions(&fresh()).await;
    scan_query_decision_table(&fresh()).await;
    credential_source_dependency_decision_table(&fresh()).await;
    lifecycle_outbox_tracks_every_committed_transition(&fresh()).await;
    pending_resource_activation_index_is_durable(&fresh()).await;
    tombstone_requires_hidden_completed_cleanup(&fresh()).await;
    deleted_event_admission_decision_table(&fresh()).await;
    let cas_repo = fresh();
    root_cas_decision_table(&cas_repo).await;
    create_receipt_decision_table(&fresh()).await;
    concurrent_root_cas_has_one_authoritative_winner(std::sync::Arc::new(fresh())).await;
}

// ── Backend rows: each must pass the identical universal suite ───────────────────
