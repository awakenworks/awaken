//! Trait-generic **conformance suite** for the `ManagedSessionRepository` port — the
//! shared behavioural contract every backend must satisfy (ADR-0059, the
//! `awaken-store-conformance` pattern).
//!
//! Every save also persists an owner scope in the same repository operation. The generic
//! suite checks that universal invariant for both backends; durable restart persistence
//! remains in the backend-specific suite.

use awaken_deployment_contract::{
    DeploymentAgent, DeploymentLifecycleFact, DeploymentRecord, DeploymentRepository,
    DeploymentRunRecord, DeploymentRunView, DeploymentSchedule, DeploymentStatus,
    DeploymentTrigger, DeploymentView, DeploymentWriteOutcome, ScheduledRunClaimOutcome,
};
use awaken_session_contract::{
    IdempotencyRecord, ManagedLifecycleFact, ManagedSessionRepository, McpAttachmentDraft,
    McpAttachmentOrigin, McpTarget, PersistedSession, ScopedPersistedSession, SessionCreateResult,
    SessionEnvironmentPhase, SessionEventProjectionAnchor, SessionExecutionState, SessionMutation,
    SessionMutationPayload, SessionMutationResult, SessionRepositoryConflict,
    SessionRepositoryError, SessionRevision, SessionTombstone,
};
use awaken_session_store::SqliteManagedSessionRepository;
use serde_json::json;

fn block<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime")
        .block_on(f)
}

fn session(id: &str, title: &str) -> PersistedSession {
    let holder = awaken_credential_contract::PlaintextHolder::new(
        awaken_credential_contract::PlaintextBoundary::Workload,
        "awaken.workload.acp",
    );
    let environment = awaken_session_contract::EnvironmentSnapshot {
        environment_id: "env".into(),
        revision: awaken_session_contract::EnvironmentRevision(4),
        self_hosted: false,
        config_fingerprint: awaken_session_contract::EnvironmentFingerprint("env-4".into()),
        sandbox: awaken_provisioning_contract::SandboxOverride {
            isolation: Some(awaken_provisioning_contract::IsolationClass::Namespace),
            ..Default::default()
        },
        sandbox_provisioning: Default::default(),
        idle_retention: Default::default(),
        packages: Default::default(),
        prepared_image: None,
        network: awaken_session_contract::SessionNetworkPolicy::None,
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
        session_id: id.to_string(),
        revision: Default::default(),
        baseline: awaken_session_contract::SessionBaselineState::Frozen(
            awaken_session_contract::SessionBaseline::compile(
                awaken_session_contract::SessionBaselineInputs {
                    environment,
                    runtime_placement: awaken_session_contract::SessionRuntimePlacement::Local,
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
        mcp: awaken_session_contract::SessionMcpAttachmentSet::from_initial(
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
        resources: awaken_session_contract::SessionResourceState::from_active(
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

fn hibernated_environment(
    workspace_id: &str,
    id: &str,
) -> awaken_session_contract::SessionEnvironmentState {
    let generation = awaken_session_contract::SandboxGeneration::new(
        id,
        1,
        100_000,
        "environment",
        "base-image",
    );
    let suspend = awaken_session_contract::SessionEnvironmentOperation::new(
        workspace_id,
        id,
        "suspend",
        &generation,
        0,
        None,
        None,
    );
    awaken_session_contract::SessionEnvironmentState::Hibernated {
        checkpoint: awaken_session_contract::SandboxCheckpointRef {
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

fn fact(id: &str, session_id: &str, event_type: &str) -> ManagedLifecycleFact {
    ManagedLifecycleFact {
        id: id.into(),
        object_id: session_id.into(),
        workspace_id: Some("ws_a".into()),
        event_type: event_type.into(),
        timestamp: 1_700_000_000,
        runtime_interval: None,
    }
}

async fn create_session<R: ManagedSessionRepository>(
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
        awaken_session_contract::SessionCreateResult::Applied(value)
        | awaken_session_contract::SessionCreateResult::Replayed(value) => value,
    }
}

async fn replace_session<R: ManagedSessionRepository>(
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
        .commit_mutation(
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

fn complete_terminal_cleanup(value: &mut PersistedSession) {
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
        .command_for(&session_id, &session_id)
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
fn install_legacy_completion_wire(
    session: &mut PersistedSession,
    command: &awaken_session_contract::SessionCleanupCommand,
) {
    let receipt_fingerprint = awaken_session_contract::stable_fingerprint(&(
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
async fn save_get_round_trips<R: ManagedSessionRepository>(r: &R) {
    let want = create_session(r, "default", session("sesn_1", "hello"), Vec::new()).await;
    assert_eq!(
        r.get("sesn_1").await,
        Ok(want),
        "the full aggregate must round-trip"
    );
}

/// Absent id → typed NotFound (no fabrication, fail-closed read).
async fn absent_id_reads_none<R: ManagedSessionRepository>(r: &R) {
    assert_eq!(
        r.get("never-saved").await,
        Err(SessionRepositoryError::NotFound)
    );
}

/// Idempotent upsert: saving the same id twice keeps the latest, not two rows.
async fn save_is_idempotent_upsert<R: ManagedSessionRepository>(r: &R) {
    create_session(r, "default", session("sesn_1", "first"), Vec::new()).await;
    replace_session(
        r,
        "default",
        session("sesn_1", "second"),
        "test:replace:second",
        Vec::new(),
    )
    .await;
    assert_eq!(r.get("sesn_1").await.unwrap().title, Some("second".into()));
}

/// A visible row and its owner are one write: no backend may expose the row with
/// a missing or stale scope after aggregate creation or replacement returns.
async fn ownership_is_one_atomic_repository_fact<R: ManagedSessionRepository>(r: &R) {
    create_session(r, "ws_a", session("sesn_owned", "owned"), Vec::new()).await;
    assert!(r.get("sesn_owned").await.is_ok());
    assert_eq!(r.owner("sesn_owned").await.as_deref(), Ok("ws_a"));

    replace_session(
        r,
        "ws_a",
        session("sesn_owned", "updated"),
        "test:replace:owned",
        Vec::new(),
    )
    .await;
    assert_eq!(r.owner("sesn_owned").await.as_deref(), Ok("ws_a"));
    assert_eq!(
        r.get("sesn_owned").await.unwrap().title,
        Some("updated".into())
    );
}

/// Binding is a narrow update: it fails closed for an unknown id and changes no
/// other aggregate field for a known Session.
async fn environment_state_is_atomic_and_non_destructive<R: ManagedSessionRepository>(r: &R) {
    assert_eq!(
        r.get("unknown").await,
        Err(SessionRepositoryError::NotFound)
    );
    let want = session("sesn_bound", "unchanged");
    create_session(r, "ws_a", want.clone(), Vec::new()).await;
    let mut bound = want.clone();
    bound
        .environment
        .set_resident(r#"{"provider_kind":"bwrap"}"#);
    replace_session(r, "ws_a", bound, "test:bind", Vec::new()).await;
    let mut got = r.get("sesn_bound").await.expect("bound Session");
    assert_eq!(
        got.environment.binding(),
        Some(r#"{"provider_kind":"bwrap"}"#)
    );
    got.environment = awaken_session_contract::SessionEnvironmentState::Unmaterialized;
    got.revision = Default::default();
    assert_eq!(got, want, "binding update preserves every other field");
    assert_eq!(r.owner("sesn_bound").await.as_deref(), Ok("ws_a"));
}

async fn global_environment_phase_count_has_no_recovery_batch_limit<R: ManagedSessionRepository>(
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
    last.environment = hibernated_environment("ws_a", &last_id);
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

async fn concurrent_phase_transition_has_one_count_snapshot<R>(repo: std::sync::Arc<R>)
where
    R: ManagedSessionRepository + 'static,
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
        .begin_restore("ws_a", id, 0, None, 1)
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

async fn concurrent_root_cas_has_one_authoritative_winner<R>(repo: std::sync::Arc<R>)
where
    R: ManagedSessionRepository + 'static,
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
            (title, repo.commit_mutation("ws_a", mutation).await)
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
        .get("sesn_multi_instance_cas")
        .await
        .expect("M1 durable winner");
    assert_eq!(durable.title.as_ref(), Some(winner), "M1/E3+E4");
    assert_eq!(durable.revision, SessionRevision(2), "M1/E3");
}

async fn vault_reference_index_returns_only_live_scoped_sessions<R: ManagedSessionRepository>(
    r: &R,
) {
    let with_vault = |id: &str, execution| {
        let mut value = session(id, id);
        value.execution = execution;
        let awaken_session_contract::SessionBaselineState::Frozen(baseline) = &mut value.baseline
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
        r.sessions_referencing_vault("ws_a", "vlt-live")
            .await
            .unwrap(),
        vec![live],
        "rollout targets only live Sessions in the exact Workspace"
    );
    assert!(
        r.sessions_referencing_vault("ws_a", "vlt-other")
            .await
            .unwrap()
            .is_empty()
    );
}

fn with_mcp_credential_source(mut value: PersistedSession, source_id: &str) -> PersistedSession {
    value.mcp.attachments[0]
        .credential
        .as_mut()
        .expect("fixture MCP credential")
        .credential
        .id = source_id.to_string();
    value
}

async fn credential_source_dependency_decision_table<R: ManagedSessionRepository>(r: &R) {
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
        r.sessions_referencing_credential_source("ws_a", &source("source-a"))
            .await
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
        r.sessions_referencing_credential_source("ws_a", &source("source-a"))
            .await
            .unwrap(),
        vec![deleted.clone()],
        "S4/E3 old dependency removed"
    );
    assert_eq!(
        r.sessions_referencing_credential_source("ws_a", &source("source-c"))
            .await
            .unwrap(),
        vec![updated.clone()],
        "S4/E3 new dependency added"
    );

    let mut archived = updated;
    archived.archive("2026-08-27T00:00:00Z").unwrap();
    replace_session(r, "ws_a", archived, "test:source:archive", Vec::new()).await;
    assert!(
        r.sessions_referencing_credential_source("ws_a", &source("source-c"))
            .await
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
        r.commit_mutation("ws_a", delete_mutation(&deleting, "test:source:delete"))
            .await
            .unwrap(),
        SessionMutationResult::Applied { .. }
    ));
    assert!(
        r.sessions_referencing_credential_source("ws_a", &source("source-a"))
            .await
            .unwrap()
            .is_empty(),
        "S6/E5"
    );
    assert_eq!(
        r.sessions_referencing_credential_source("ws_a", &source("source-b"))
            .await
            .unwrap(),
        vec![unrelated],
        "S3/E2"
    );
}

/// The lifecycle fact is committed in the same repository transaction as the
/// aggregate and owner. Notification may crash afterwards without losing the fact.
async fn lifecycle_outbox_tracks_every_committed_transition<R: ManagedSessionRepository>(r: &R) {
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
    created.runtime_interval = Some(awaken_session_contract::SessionRuntimeInterval {
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
    assert!(r.get("sesn_lifecycle").await.is_ok());
    assert_eq!(r.owner("sesn_lifecycle").await.as_deref(), Ok("ws_a"));
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
    let mut archive = r.get("sesn_lifecycle").await.unwrap();
    archive.archive("2026-07-19T00:00:00Z").unwrap();
    replace_session(r, "ws_a", archive, "test:archive", vec![archived.clone()]).await;
    let durable = r.get("sesn_lifecycle").await.expect("archived session");
    assert_eq!(durable.execution, SessionExecutionState::Terminated);
    assert_eq!(durable.archived_at(), Some("2026-07-19T00:00:00Z"));
    assert_exact_outbox_delta(
        &outbox_before,
        &r.pending_lifecycle().await.unwrap(),
        &archived,
        "archive",
    );
    r.complete_lifecycle(&archived.id).await.unwrap();

    let deleted = fact("evt:delete", "sesn_lifecycle", "session.deleted");
    let mut current = r.get("sesn_lifecycle").await.unwrap();
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
        r.commit_mutation(
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
        r.get("sesn_lifecycle").await,
        Err(SessionRepositoryError::NotFound)
    );
    assert_exact_outbox_delta(
        &outbox_before,
        &r.pending_lifecycle().await.unwrap(),
        &deleted,
        "delete",
    );
}

async fn lifecycle_outbox_insert_failure_rolls_back_root_paths<R, Arm, Armed>(
    repo: &R,
    arm_outbox_failure: Arm,
) where
    R: ManagedSessionRepository,
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
            Err(SessionRepositoryError::Unavailable(_))
        ),
        "A1 injected outbox failure reaches the transaction boundary"
    );
    assert_eq!(
        repo.get("sesn_atomic_create").await,
        Err(SessionRepositoryError::NotFound),
        "A1/E1"
    );
    assert_eq!(
        repo.idempotency_receipt("sesn_atomic_create", create_key)
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
            repo.commit_mutation(
                "ws_a",
                SessionMutation {
                    expected_revision: replace_predecessor.revision,
                    idempotency: record(replace_key, &replace_payload),
                    payload: replace_payload,
                    lifecycle_facts: vec![fact(
                        "evt:atomic:replace",
                        &replace_predecessor.session_id,
                        "session.archived",
                    )],
                },
            )
            .await,
            Err(SessionRepositoryError::Unavailable(_))
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
        deleted_at: "2026-09-10T00:00:00Z".into(),
    });
    let delete_key = "test:atomic:delete";
    assert!(
        matches!(
            repo.commit_mutation(
                "ws_a",
                SessionMutation {
                    expected_revision: delete_predecessor.revision,
                    idempotency: record(delete_key, &delete_payload),
                    payload: delete_payload,
                    lifecycle_facts: vec![fact(
                        "evt:atomic:delete",
                        &delete_predecessor.session_id,
                        "session.deleted",
                    )],
                },
            )
            .await,
            Err(SessionRepositoryError::Unavailable(_))
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
async fn pending_resource_activation_index_is_durable<R: ManagedSessionRepository>(r: &R) {
    let mut pending = session("sesn_pending", "pending");
    let desired = pending.resources.active.clone();
    pending.resources = Default::default();
    pending
        .resources
        .prepare(&pending.session_id, desired)
        .unwrap();
    pending = create_session(r, "ws_a", pending, Vec::new()).await;

    assert_eq!(
        r.reconcilable_sessions().await.unwrap().sessions,
        vec![ScopedPersistedSession {
            workspace_id: "ws_a".into(),
            session: pending.clone(),
        }]
    );

    pending.resources.start_attempt().unwrap();
    pending.resources.commit().unwrap();
    pending = replace_session(r, "ws_a", pending, "test:resource-active", Vec::new()).await;
    let indexed = r.reconcilable_sessions().await.unwrap();
    assert_eq!(indexed.sessions.len(), 1, "active MCP remains restart work");
    assert!(indexed.sessions[0].session.mcp.needs_reconciliation());

    pending.mcp.attachments[0].state = awaken_session_contract::McpAttachmentState::Failed;
    pending = replace_session(r, "ws_a", pending, "test:mcp-failed", Vec::new()).await;
    assert_eq!(
        r.reconcilable_sessions().await.unwrap().sessions,
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
    assert!(r.reconcilable_sessions().await.unwrap().sessions.is_empty());

    pending.environment.set_resident("worker-owned-binding");
    pending = replace_session(r, "ws_a", pending, "test:resident-environment", Vec::new()).await;
    assert_eq!(
        r.reconcilable_sessions().await.unwrap().sessions,
        vec![ScopedPersistedSession {
            workspace_id: "ws_a".into(),
            session: pending,
        }],
        "C3/E3: restore durable Environment identity without persisting Hand activity"
    );
}

#[derive(Clone, Copy)]
enum CasRule {
    Create,
    CreateReplay,
    CreateIdempotencyMismatch,
    CreateReceiptAsMutation,
    Replace,
    ReplaceReplay,
    ReplaceReplayWrongExpected,
    ReplaceReplayWrongOwner,
    ReplaceIdempotencyMismatch,
    StaleRevision,
    WrongOwner,
    Delete,
    DeleteReplay,
}

fn record(key: &str, payload: &SessionMutationPayload) -> IdempotencyRecord {
    IdempotencyRecord {
        key: key.into(),
        payload_hash: payload.stable_hash(),
    }
}

fn delete_mutation(value: &PersistedSession, key: &str) -> SessionMutation {
    let payload = SessionMutationPayload::Delete(SessionTombstone {
        session_id: value.session_id.clone(),
        deleted_revision: SessionRevision(value.revision.0 + 1),
        deleted_at: "2026-08-15T00:00:00Z".into(),
    });
    SessionMutation {
        expected_revision: value.revision,
        idempotency: record(key, &payload),
        payload,
        lifecycle_facts: Vec::new(),
    }
}

async fn tombstone_requires_hidden_completed_cleanup<R: ManagedSessionRepository>(repo: &R) {
    // Every rejected delete must be an exact stutter: the aggregate, command
    // receipt and lifecycle outbox remain unchanged. The cases isolate the two
    // durable admission axes: hidden disposition and verified cleanup completion.
    let mut active = session("sesn_delete_active", "active");
    active.execution = SessionExecutionState::Terminated;
    complete_terminal_cleanup(&mut active);
    let active = create_session(repo, "ws_a", active, Vec::new()).await;

    let mut archived = session("sesn_delete_archived", "archived");
    archived.archive("2026-08-15T00:00:00Z").unwrap();
    complete_terminal_cleanup(&mut archived);
    let archived = create_session(repo, "ws_a", archived, Vec::new()).await;

    let mut pending = session("sesn_delete_pending", "pending");
    assert!(pending.request_delete("ws_a", 0).unwrap().is_some());
    pending
        .terminal_cleanup
        .freeze_targets(&pending.session_id.clone(), [], 0, 0)
        .expect("pending delete freezes its root target");
    let pending = create_session(repo, "ws_a", pending, Vec::new()).await;

    for (rule, value) in [
        ("active", active),
        ("archived", archived),
        ("deleting-pending", pending),
    ] {
        let key = format!("test:tombstone:{rule}");
        let before = repo.get(&value.session_id).await.unwrap();
        let outbox_before = repo.pending_lifecycle().await.unwrap();
        assert!(
            matches!(
                repo.commit_mutation("ws_a", delete_mutation(&before, &key))
                    .await,
                Err(SessionRepositoryError::InvalidMutation(_))
            ),
            "{rule}: tombstone admission fails closed"
        );
        assert_eq!(
            repo.get(&value.session_id).await,
            Ok(before.clone()),
            "{rule}: rejected tombstone preserves the complete aggregate"
        );
        assert_eq!(
            repo.idempotency_receipt(&value.session_id, &key).await,
            Ok(None),
            "{rule}: rejection writes no command receipt"
        );
        assert_eq!(
            repo.pending_lifecycle().await.unwrap(),
            outbox_before,
            "{rule}: rejection writes no lifecycle fact"
        );
    }

    let mut completed = session("sesn_delete_completed", "completed");
    assert!(completed.request_delete("ws_a", 0).unwrap().is_some());
    complete_terminal_cleanup(&mut completed);
    let completed = create_session(repo, "ws_a", completed, Vec::new()).await;
    let mutation = delete_mutation(&completed, "test:tombstone:completed");
    assert_eq!(
        repo.commit_mutation("ws_a", mutation.clone())
            .await
            .unwrap(),
        SessionMutationResult::Applied {
            new_revision: SessionRevision(completed.revision.0 + 1)
        },
        "deleting plus verified completed cleanup is tombstone-admissible"
    );
    assert_eq!(
        repo.get(&completed.session_id).await,
        Err(SessionRepositoryError::NotFound)
    );
    assert_eq!(
        repo.commit_mutation("ws_a", mutation).await.unwrap(),
        SessionMutationResult::Replayed {
            new_revision: SessionRevision(completed.revision.0 + 1)
        },
        "an admitted tombstone has one stable replay receipt"
    );
}

async fn deleted_event_admission_decision_table<R: ManagedSessionRepository>(repo: &R) {
    use awaken_session_contract::{SessionEventAdmission, SessionEventBatch, SessionEventInput};
    // Delete CAS is the sole proof writer. D1 accepted key=>same Accepted;
    // D2 closed key=>same Closed; D3 absent key=>Deleted fence; D4 changed
    // fingerprint=>conflict; D5 foreign/missing Session=>no disclosure; D6
    // stale delete=>no tombstone. D7 mixed keyed/unkeyed batches=>retain only
    // keyed receipts in original order, including an accepted second key with
    // 12 operations (numeric order differs from lexical order at 10). D8 every
    // lookup/error/replay=>the complete stored proof remains unchanged. The
    // expected operation order is frozen from the original commands, without
    // calling the production operation_ids projection used by the encoder.
    // Payload is absent from retained projection.
    for kind in ["accepted", "closed", "empty", "mixed"] {
        let id = format!("sesn_retained_{kind}");
        let mut value = session(&id, "private title");
        let mut expected_receipts = vec![];
        let mut expected_operations = vec![];
        if kind != "empty" {
            let batch = if kind == "closed" {
                SessionEventBatch::closed_admission(
                    "batch".into(),
                    SessionRevision(1),
                    "key".into(),
                    "fp".into(),
                )
                .unwrap()
            } else {
                let input = SessionEventInput::UserMessage {
                    content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                        "private message",
                    )],
                };
                let inputs = vec![input; if kind == "mixed" { 12 } else { 1 }];
                let mut batch = SessionEventBatch::compile(&id, "batch", inputs).unwrap();
                batch.admitted_revision = SessionRevision(1);
                batch
                    .anchor_admission_projection(SessionEventProjectionAnchor {
                        source_commit_cursor: 0,
                    })
                    .unwrap();
                batch.bind_idempotency("key", "fp").unwrap();
                batch.resolve_terminally(None, 1);
                batch
            };
            expected_operations = batch
                .events
                .iter()
                .map(|entry| entry.event.operation_id().to_owned())
                .collect();
            expected_receipts.push(
                json!({"key":"key","request_fingerprint":"fp","batch_id":"batch",
                "closed":kind == "closed", "operation_ids":expected_operations}),
            );
            value.event_batches.push(batch);
        }
        if kind == "mixed" {
            let closed = SessionEventBatch::closed_admission(
                "prior:batch".into(),
                SessionRevision(1),
                "prior-key".into(),
                "prior-fp".into(),
            )
            .unwrap();
            value.event_batches.insert(0, closed);
            expected_receipts.insert(
                0,
                json!({"key":"prior-key", "request_fingerprint":"prior-fp",
                "batch_id":"prior:batch", "closed":true, "operation_ids":[]}),
            );
            let mut unkeyed = SessionEventBatch::compile(
                &id,
                "unkeyed",
                vec![SessionEventInput::UserMessage {
                    content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                        "unretained private message",
                    )],
                }],
            )
            .unwrap();
            unkeyed.admitted_revision = SessionRevision(1);
            unkeyed
                .anchor_admission_projection(SessionEventProjectionAnchor {
                    source_commit_cursor: 0,
                })
                .unwrap();
            unkeyed.resolve_terminally(None, 1);
            value.event_batches.push(unkeyed);
        }
        assert!(value.request_delete("ws_a", 0).unwrap().is_some());
        complete_terminal_cleanup(&mut value);
        let value = create_session(repo, "ws_a", value, vec![]).await;
        assert_eq!(
            repo.deleted_event_admissions("ws_a", &id).await.unwrap(),
            None
        );
        let mut stale = delete_mutation(&value, &format!("delete-stale-{kind}"));
        stale.expected_revision = SessionRevision(0);
        assert!(repo.commit_mutation("ws_a", stale).await.is_err(), "D6");
        assert_eq!(
            repo.deleted_event_admissions("ws_a", &id).await.unwrap(),
            None,
            "D6"
        );
        repo.commit_mutation("ws_a", delete_mutation(&value, &format!("delete-{kind}")))
            .await
            .unwrap();
        let proof = repo
            .deleted_event_admissions("ws_a", &id)
            .await
            .unwrap()
            .unwrap();
        let deleted = SessionEventAdmission::Deleted {
            deleted_revision: SessionRevision(value.revision.0 + 1),
        };
        assert_eq!(proof.resolve("unseen", "fp").unwrap(), deleted, "D3");
        if kind == "empty" {
            assert_eq!(proof.admissions, Some(vec![]));
            assert_eq!(proof.resolve("key", "fp").unwrap(), deleted);
        } else {
            assert_eq!(
                proof.resolve("key", "fp").unwrap(),
                if kind != "closed" {
                    SessionEventAdmission::Accepted {
                        batch_id: "batch".into(),
                        operation_ids: Some(expected_operations.clone()),
                    }
                } else {
                    SessionEventAdmission::Closed {
                        batch_id: "batch".into(),
                    }
                },
                "D1/D2"
            );
            assert_eq!(
                proof.resolve("key", "changed").unwrap_err().code,
                "idempotency_conflict",
                "D4"
            );
            let retained = serde_json::to_value(proof.admissions.as_ref().unwrap()).unwrap();
            assert_eq!(retained, json!(expected_receipts));
        }
        if kind == "mixed" {
            assert_eq!(
                proof.resolve("prior-key", "prior-fp").unwrap(),
                SessionEventAdmission::Closed {
                    batch_id: "prior:batch".into(),
                },
                "D7 first key remains distinct from accepted second key"
            );
            assert_eq!(
                proof.resolve("prior-key", "fp").unwrap_err().code,
                "idempotency_conflict",
                "D7 fingerprint belongs to its exact key"
            );
        }
        assert_eq!(
            repo.deleted_event_admissions("ws_a", &id).await.unwrap(),
            Some(proof.clone()),
            "D8 reads preserve full proof"
        );
        assert!(
            matches!(repo.commit_mutation("ws_a", delete_mutation(&value, &format!("delete-{kind}"))).await.unwrap(),
            SessionMutationResult::Replayed { new_revision } if new_revision == proof.deleted_revision),
            "D8 stable delete receipt"
        );
        assert_eq!(
            repo.deleted_event_admissions("ws_a", &id).await.unwrap(),
            Some(proof),
            "D8 deletion replay preserves full proof"
        );
        assert_eq!(
            repo.deleted_event_admissions("foreign", &id).await.unwrap(),
            None,
            "D5"
        );
        assert!(matches!(
            repo.get(&id).await,
            Err(SessionRepositoryError::NotFound)
        ));
        let mut recreated = value;
        recreated.revision = SessionRevision(0);
        let payload = SessionMutationPayload::Replace(recreated.clone());
        assert!(
            matches!(
                repo.create(
                    "ws_a",
                    recreated,
                    record(&format!("recreate-{kind}"), &payload),
                    vec![]
                )
                .await,
                Err(SessionRepositoryError::Conflict(
                    SessionRepositoryConflict::Tombstoned
                ))
            ),
            "late recreation is fenced"
        );
    }
    assert_eq!(
        repo.deleted_event_admissions("ws_a", "missing")
            .await
            .unwrap(),
        None,
        "D5"
    );
}

async fn root_cas_decision_table<R: ManagedSessionRepository>(repo: &R) {
    // Cause-effect graph:
    // C1 valid command -> C2 idempotency absent-or-equal -> C3 live row exists
    // -> C4 owner exact -> C5 revision exact -> E1 atomic applied/replayed.
    // Delete additionally yields E2 tombstone read-as-not-found. Any failed
    // cause yields its typed result and changes neither aggregate nor outbox.
    //
    // | Rule | Operation | C2 | C3 | C4 | C5 | Result |
    // |---|---|---|---|---|---|---|
    // | R1 | create | absent | - | - | new | revision 1 |
    // | R2 | create retry | equal | - | - | - | revision 1 |
    // | R3 | create retry | mismatch | - | - | - | idempotency error |
    // | R4 | replace | absent | T | T | T | applied revision 2 |
    // | R5 | replace retry | equal | T | T | T | replayed revision 2 |
    // | R6 | replace retry | equal | T | F | T | conflict revision 2 |
    // | R7 | replace retry | mismatch | T | T | T | idempotency mismatch |
    // | R8 | replace | absent | T | T | F | conflict revision 1 |
    // | R9 | replace | absent | T | F | T | conflict revision 1 |
    // | R10 | delete | absent | T | T | T | applied revision 2 + hidden |
    // | R11 | delete retry | equal | tombstone | T | old | replayed revision 2 |
    // | R12 | mutation with create receipt | equal | T | T | next differs | mismatch |
    // | R13 | replace retry | equal | T | T | wrong expected | mismatch |
    let rules = [
        CasRule::Create,
        CasRule::CreateReplay,
        CasRule::CreateIdempotencyMismatch,
        CasRule::CreateReceiptAsMutation,
        CasRule::Replace,
        CasRule::ReplaceReplay,
        CasRule::ReplaceReplayWrongExpected,
        CasRule::ReplaceReplayWrongOwner,
        CasRule::ReplaceIdempotencyMismatch,
        CasRule::StaleRevision,
        CasRule::WrongOwner,
        CasRule::Delete,
        CasRule::DeleteReplay,
    ];
    for (index, rule) in rules.into_iter().enumerate() {
        let id = format!("sesn_cas_{index}");
        let mut initial = session(&id, "initial");
        if matches!(rule, CasRule::Delete | CasRule::DeleteReplay) {
            assert!(initial.request_delete("ws_a", 0).unwrap().is_some());
            complete_terminal_cleanup(&mut initial);
        }
        let create_payload = SessionMutationPayload::Replace(initial.clone());
        let create_record = record("create", &create_payload);
        let created = repo
            .create("ws_a", initial, create_record.clone(), Vec::new())
            .await
            .expect("initial CAS create");
        assert_eq!(
            created,
            awaken_session_contract::SessionCreateResult::Applied({
                let mut expected = session(&id, "initial");
                if matches!(rule, CasRule::Delete | CasRule::DeleteReplay) {
                    assert!(expected.request_delete("ws_a", 0).unwrap().is_some());
                    complete_terminal_cleanup(&mut expected);
                }
                expected.revision = SessionRevision(1);
                expected
            })
        );
        assert_eq!(
            repo.idempotency_receipt(&id, "create").await,
            Ok(Some(awaken_session_contract::SessionIdempotencyReceipt {
                payload_hash: create_record.payload_hash.clone(),
                committed_revision: SessionRevision(1),
            })),
            "the decision table reads the same atomic receipt it writes"
        );

        match rule {
            CasRule::Create => {}
            CasRule::CreateReplay => {
                let mut replay = session(&id, "initial");
                replay.revision = SessionRevision(0);
                let mut expected = replay.clone();
                expected.revision = SessionRevision(1);
                assert_eq!(
                    repo.create("ws_a", replay, create_record, Vec::new())
                        .await
                        .unwrap(),
                    awaken_session_contract::SessionCreateResult::Replayed(expected)
                );
            }
            CasRule::CreateIdempotencyMismatch => {
                let error = repo
                    .create(
                        "ws_a",
                        session(&id, "different"),
                        IdempotencyRecord {
                            key: "create".into(),
                            payload_hash: "different".into(),
                        },
                        Vec::new(),
                    )
                    .await
                    .unwrap_err();
                assert_eq!(
                    error,
                    SessionRepositoryError::Conflict(
                        awaken_session_contract::SessionRepositoryConflict::IdempotencyMismatch
                    )
                );
            }
            CasRule::CreateReceiptAsMutation => {
                let mut replacement = repo.get(&id).await.unwrap();
                replacement.revision = SessionRevision(0);
                assert_eq!(
                    repo.commit_mutation(
                        "ws_a",
                        SessionMutation {
                            expected_revision: SessionRevision(0),
                            idempotency: create_record,
                            payload: SessionMutationPayload::Replace(replacement),
                            lifecycle_facts: Vec::new(),
                        },
                    )
                    .await
                    .unwrap(),
                    SessionMutationResult::IdempotencyMismatch,
                    "R12 a revision-one create receipt cannot replay even a forged revision-zero mutation"
                );
            }
            CasRule::Replace
            | CasRule::ReplaceReplay
            | CasRule::ReplaceReplayWrongExpected
            | CasRule::ReplaceReplayWrongOwner
            | CasRule::ReplaceIdempotencyMismatch
            | CasRule::StaleRevision
            | CasRule::WrongOwner => {
                let mut replacement = repo.get(&id).await.unwrap();
                replacement.title = Some("replacement".into());
                if matches!(rule, CasRule::StaleRevision) {
                    replacement.revision = SessionRevision(0);
                }
                let payload = SessionMutationPayload::Replace(replacement);
                let key = if matches!(rule, CasRule::ReplaceIdempotencyMismatch) {
                    "replace-mismatch"
                } else {
                    "replace"
                };
                let mutation = SessionMutation {
                    expected_revision: if matches!(rule, CasRule::StaleRevision) {
                        SessionRevision(0)
                    } else {
                        SessionRevision(1)
                    },
                    idempotency: record(key, &payload),
                    payload: payload.clone(),
                    lifecycle_facts: Vec::new(),
                };
                let owner = if matches!(rule, CasRule::WrongOwner) {
                    "ws_b"
                } else {
                    "ws_a"
                };
                let first = repo.commit_mutation(owner, mutation.clone()).await.unwrap();
                let expected = if matches!(rule, CasRule::StaleRevision | CasRule::WrongOwner) {
                    SessionMutationResult::Conflict {
                        current_revision: SessionRevision(1),
                    }
                } else {
                    SessionMutationResult::Applied {
                        new_revision: SessionRevision(2),
                    }
                };
                assert_eq!(first, expected);
                if matches!(rule, CasRule::ReplaceReplay) {
                    assert_eq!(
                        repo.commit_mutation(owner, mutation.clone()).await.unwrap(),
                        SessionMutationResult::Replayed {
                            new_revision: SessionRevision(2)
                        }
                    );
                }
                if matches!(rule, CasRule::ReplaceReplayWrongExpected) {
                    let mut wrong_expected = mutation.clone();
                    wrong_expected.expected_revision = SessionRevision(0);
                    if let SessionMutationPayload::Replace(session) = &mut wrong_expected.payload {
                        session.revision = SessionRevision(0);
                    }
                    assert_eq!(
                        repo.commit_mutation(owner, wrong_expected).await.unwrap(),
                        SessionMutationResult::IdempotencyMismatch,
                        "R13 a receipt cannot replay under another expected revision"
                    );
                }
                if matches!(rule, CasRule::ReplaceReplayWrongOwner) {
                    assert_eq!(
                        repo.commit_mutation("ws_b", mutation.clone())
                            .await
                            .unwrap(),
                        SessionMutationResult::Conflict {
                            current_revision: SessionRevision(2)
                        },
                        "a foreign owner cannot adopt an exact mutation receipt"
                    );
                }
                if matches!(rule, CasRule::ReplaceIdempotencyMismatch) {
                    let mut mismatched = mutation;
                    mismatched.idempotency.payload_hash = "different".into();
                    assert_eq!(
                        repo.commit_mutation(owner, mismatched).await.unwrap(),
                        SessionMutationResult::IdempotencyMismatch
                    );
                }
            }
            CasRule::Delete | CasRule::DeleteReplay => {
                let payload = SessionMutationPayload::Delete(SessionTombstone {
                    session_id: id.clone(),
                    deleted_revision: SessionRevision(2),
                    deleted_at: "2026-07-25T00:00:00Z".into(),
                });
                let mutation = SessionMutation {
                    expected_revision: SessionRevision(1),
                    idempotency: record("delete", &payload),
                    payload,
                    lifecycle_facts: Vec::new(),
                };
                assert_eq!(
                    repo.commit_mutation("ws_a", mutation.clone())
                        .await
                        .unwrap(),
                    SessionMutationResult::Applied {
                        new_revision: SessionRevision(2)
                    }
                );
                assert_eq!(repo.get(&id).await, Err(SessionRepositoryError::NotFound));
                if matches!(rule, CasRule::DeleteReplay) {
                    assert_eq!(
                        repo.commit_mutation("ws_a", mutation).await.unwrap(),
                        SessionMutationResult::Replayed {
                            new_revision: SessionRevision(2)
                        }
                    );
                }
            }
        }
    }
}

async fn create_receipt_decision_table<R: ManagedSessionRepository>(repo: &R) {
    // Create-receipt cause/effect table. C1 receipt absent/present; C2 durable
    // identity absent/live/tombstoned; C3 owner exact/foreign; C4 hash exact/
    // mismatched; C5 the live aggregate advanced after create. Effects: E1 a
    // new root is Applied once; E2 exact replay returns the current durable
    // aggregate, never the caller's newly lowered candidate; E3 wrong owner or
    // another key conflicts; E4 hash reuse mismatches; E5 tombstone is terminal;
    // E6 absent/whitespace owner is rejected before writes.
    //
    // | Rule | Receipt | Identity | Owner | Hash | Effect |
    // |---|---|---|---|---|---|
    // | C1 | absent | absent | exact | valid | E1 |
    // | C2 | exact | live+advanced | exact | exact | E2 |
    // | C3 | exact/absent | live | foreign/exact | exact/other key | E3 |
    // | C4 | present | live | exact | mismatch | E4 |
    // | C5 | exact | tombstone | exact | exact | E5 |
    // | C6 | any | any | empty/space | any | E6 |
    let id = "sesn_create_receipt_table";
    let candidate = session(id, "original lowering");
    let create_record = IdempotencyRecord {
        key: "create-receipt-table".into(),
        payload_hash: "stable-request-fingerprint".into(),
    };
    let applied = repo
        .create("ws_a", candidate.clone(), create_record.clone(), Vec::new())
        .await
        .expect("C1/E1");
    let SessionCreateResult::Applied(mut durable) = applied else {
        panic!("C1/E1 must apply")
    };
    durable.title = Some("durable revision two".into());
    durable = replace_session(repo, "ws_a", durable, "advance-after-create", Vec::new()).await;

    assert_eq!(
        repo.replay_create("ws_a", id, &create_record).await,
        Ok(Some(durable.clone())),
        "C2/E2 atomic preflight returns current durable aggregate"
    );
    let mut changed_lowering = candidate;
    changed_lowering.title = Some("new lowering must not escape".into());
    assert_eq!(
        repo.create("ws_a", changed_lowering, create_record.clone(), Vec::new(),)
            .await,
        Ok(SessionCreateResult::Replayed(durable.clone())),
        "C2/E2 create replay returns repository truth"
    );
    assert_eq!(
        repo.replay_create("ws_b", id, &create_record).await,
        Err(SessionRepositoryError::Conflict(
            SessionRepositoryConflict::AlreadyExists
        )),
        "C3/E3 owner-bound receipt"
    );
    assert_eq!(
        repo.replay_create(
            "ws_a",
            id,
            &IdempotencyRecord {
                payload_hash: "another-request".into(),
                ..create_record.clone()
            },
        )
        .await,
        Err(SessionRepositoryError::Conflict(
            SessionRepositoryConflict::IdempotencyMismatch
        )),
        "C4/E4"
    );
    assert_eq!(
        repo.replay_create(
            "ws_a",
            id,
            &IdempotencyRecord {
                key: "another-key".into(),
                payload_hash: create_record.payload_hash.clone(),
            },
        )
        .await,
        Err(SessionRepositoryError::Conflict(
            SessionRepositoryConflict::AlreadyExists
        )),
        "C3/E3 another key cannot adopt an occupied identity"
    );

    let tombstoned_id = "sesn_create_receipt_tombstone";
    let mut tombstoned = session(tombstoned_id, "terminal");
    assert!(tombstoned.request_delete("ws_a", 0).unwrap().is_some());
    complete_terminal_cleanup(&mut tombstoned);
    let tombstone_record = IdempotencyRecord {
        key: "create-receipt-tombstone".into(),
        payload_hash: "terminal-request".into(),
    };
    let SessionCreateResult::Applied(tombstoned) = repo
        .create("ws_a", tombstoned, tombstone_record.clone(), Vec::new())
        .await
        .expect("C5 setup")
    else {
        panic!("C5 setup applies")
    };
    repo.commit_mutation(
        "ws_a",
        delete_mutation(&tombstoned, "delete-create-receipt-tombstone"),
    )
    .await
    .expect("C5 tombstone");
    assert_eq!(
        repo.replay_create("ws_a", tombstoned_id, &tombstone_record)
            .await,
        Err(SessionRepositoryError::Conflict(
            SessionRepositoryConflict::Tombstoned
        )),
        "C5/E5"
    );
    for (owner, invalid_id) in [("", "sesn_empty_owner"), (" \t", "sesn_whitespace_owner")] {
        assert!(matches!(
            repo.create(
                owner,
                session(invalid_id, "invalid"),
                IdempotencyRecord {
                    key: "invalid-owner".into(),
                    payload_hash: "invalid-owner".into(),
                },
                Vec::new(),
            )
            .await,
            Err(SessionRepositoryError::InvalidMutation(_))
        ));
        assert_eq!(
            repo.get(invalid_id).await,
            Err(SessionRepositoryError::NotFound),
            "C6/E6"
        );
    }
}

async fn create_receipt_race_decision_table<R: ManagedSessionRepository>(repo: std::sync::Arc<R>) {
    // Concurrent create cause/effect table. C1 same key+hash with different
    // lowerings; C2 same key with different hashes; C3 delete races a create on
    // the same durable identity. Effects: E1 exactly one Applied plus one
    // Replayed carrying the winner; E2 exactly one Applied plus one mismatch;
    // E3 delete never leaves both a live aggregate and tombstone and the create
    // cannot resurrect the identity. A two-party barrier makes every row enter
    // the repository concurrently; the repository transaction is the sole arbiter.
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let same_id = "sesn_create_race_same";
    let same_record = IdempotencyRecord {
        key: "race-same".into(),
        payload_hash: "same-request".into(),
    };
    let left = {
        let repo = repo.clone();
        let barrier = barrier.clone();
        let record = same_record.clone();
        async move {
            barrier.wait().await;
            repo.create("ws_a", session(same_id, "left"), record, Vec::new())
                .await
        }
    };
    let right = {
        let repo = repo.clone();
        let barrier = barrier.clone();
        let record = same_record;
        async move {
            barrier.wait().await;
            repo.create("ws_a", session(same_id, "right"), record, Vec::new())
                .await
        }
    };
    let (left, right) = tokio::join!(left, right);
    let outcomes = [left.expect("C1 left"), right.expect("C1 right")];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, SessionCreateResult::Applied(_)))
            .count(),
        1,
        "C1/E1 one Applied"
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, SessionCreateResult::Replayed(_)))
            .count(),
        1,
        "C1/E1 one Replayed"
    );
    let durable = repo.get(same_id).await.expect("C1 durable winner");
    assert!(outcomes.iter().all(|outcome| match outcome {
        SessionCreateResult::Applied(session) | SessionCreateResult::Replayed(session) =>
            session == &durable,
    }));

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let different_id = "sesn_create_race_different";
    let create = |title: &'static str, hash: &'static str| {
        let repo = repo.clone();
        let barrier = barrier.clone();
        async move {
            barrier.wait().await;
            repo.create(
                "ws_a",
                session(different_id, title),
                IdempotencyRecord {
                    key: "race-different".into(),
                    payload_hash: hash.into(),
                },
                Vec::new(),
            )
            .await
        }
    };
    let (left, right) = tokio::join!(create("left", "left-hash"), create("right", "right-hash"));
    let outcomes = [left, right];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, Ok(SessionCreateResult::Applied(_))))
            .count(),
        1,
        "C2/E2 one Applied"
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| {
                matches!(
                    outcome,
                    Err(SessionRepositoryError::Conflict(
                        SessionRepositoryConflict::IdempotencyMismatch
                    ))
                )
            })
            .count(),
        1,
        "C2/E2 one mismatch"
    );

    let delete_id = "sesn_delete_create_race";
    let mut deletable = session(delete_id, "delete winner");
    assert!(deletable.request_delete("ws_a", 0).unwrap().is_some());
    complete_terminal_cleanup(&mut deletable);
    let deletable = create_session(repo.as_ref(), "ws_a", deletable, Vec::new()).await;
    let delete = delete_mutation(&deletable, "delete-create-race");
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let delete_future = {
        let repo = repo.clone();
        let barrier = barrier.clone();
        async move {
            barrier.wait().await;
            repo.commit_mutation("ws_a", delete).await
        }
    };
    let create_future = {
        let repo = repo.clone();
        let barrier = barrier;
        async move {
            barrier.wait().await;
            repo.create(
                "ws_a",
                session(delete_id, "must not resurrect"),
                IdempotencyRecord {
                    key: "create-after-delete-race".into(),
                    payload_hash: "create-after-delete-race".into(),
                },
                Vec::new(),
            )
            .await
        }
    };
    let (deleted, created) = tokio::join!(delete_future, create_future);
    assert!(
        matches!(deleted, Ok(SessionMutationResult::Applied { .. })),
        "C3/E3"
    );
    assert!(
        matches!(
            created,
            Err(SessionRepositoryError::Conflict(
                SessionRepositoryConflict::AlreadyExists | SessionRepositoryConflict::Tombstoned
            ))
        ),
        "C3/E3"
    );
    assert_eq!(
        repo.get(delete_id).await,
        Err(SessionRepositoryError::NotFound),
        "C3/E3"
    );
}

async fn run_suite<R: ManagedSessionRepository + 'static>(fresh: impl Fn() -> R) {
    save_get_round_trips(&fresh()).await;
    absent_id_reads_none(&fresh()).await;
    save_is_idempotent_upsert(&fresh()).await;
    ownership_is_one_atomic_repository_fact(&fresh()).await;
    environment_state_is_atomic_and_non_destructive(&fresh()).await;
    global_environment_phase_count_has_no_recovery_batch_limit(&fresh()).await;
    vault_reference_index_returns_only_live_scoped_sessions(&fresh()).await;
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

fn deployment_record(id: &str, revision: u64, scheduled: bool) -> DeploymentView {
    DeploymentView {
        id: id.to_string(),
        record: DeploymentRecord {
            revision,
            created_at: "1970-01-01T00:00:00Z".into(),
            updated_at: "1970-01-01T00:00:00Z".into(),
            workspace_id: "ws_a".into(),
            agent: DeploymentAgent::new("agent_a", 1),
            environment_id: "env_a".into(),
            name: id.to_string(),
            description: None,
            metadata: Default::default(),
            initial_events: Vec::new(),
            resources: Vec::new(),
            schedule: scheduled.then(|| DeploymentSchedule::Cron {
                expression: "0 * * * *".into(),
                timezone: "UTC".into(),
            }),
            vault_ids: Vec::new(),
            budget_max_list_cost_minor: None,
            status: DeploymentStatus::Active,
            paused_reason: None,
            archived_at: None,
            last_run_at: None,
            next_fire_ms: scheduled.then_some(60_000),
        },
    }
}

fn deployment_fact(id: &str, object_id: &str) -> DeploymentLifecycleFact {
    DeploymentLifecycleFact {
        id: id.into(),
        object_id: object_id.into(),
        workspace_id: Some("ws_a".into()),
        event_type: "deployment.updated".into(),
        timestamp: 1,
        runtime_interval: None,
    }
}

async fn deployment_cas_decision_table<R: DeploymentRepository + ManagedSessionRepository>(
    repo: &R,
    replicas: [&dyn DeploymentRepository; 3],
) {
    // Deployment repository cause/effect graph:
    // C1=create or three writers share revision r; C2=three independent clients
    // race distinct scheduled rows at capacity one; C3=scheduler presents current
    // or stale revision; C4=a current scheduler reuses the rejected stale claim
    // identity; C5=a duplicate run row forces the claim transaction to fail.
    // Effects: E1=one CAS write advances r exactly once; E2=the loser
    // conflicts without a lifecycle fact; E3=one exact capacity winner and its
    // fact commit while both rejected candidates leave no row or fact; E4=claim,
    // run, cursor and fact commit atomically; E5=stale claim preserves the winner
    // frame and creates no claim/run/fact; E6=C4 claims, proving that E5 did not
    // reserve the durable identity; E7=C5 preserves the complete deployment,
    // creates no fact or claim, and an exact retry of that claim identity commits.
    //
    // | Rule | C1                | C2 | C3 | C4      | Effect       |
    // | D1   | create            | F  | F  | -       | applied      |
    // | D2   | jump/reuse r      | F  | F  | -       | E2 reject   |
    // | D3   | three writers at r| F  | F  | -       | E1 + E2     |
    // | D4   | three creates     | T  | limit 1 | -  | E3          |
    // | D5a  | duplicate run     | -  | -  | current | E7          |
    // | D5b  | retry D5a claim id| -  | -  | current | E4 + E7     |
    // | D6   | scheduler at r-1  | -  | -  | stale   | E5          |
    // | D7   | reuse D6 claim id | -  | -  | current | E4 + E6     |
    let baseline_facts = repo.pending_lifecycle().await.unwrap().len();
    let created = deployment_record("depl_cas", 0, false);
    assert_eq!(
        repo.write_deployment(
            created.clone(),
            None,
            1,
            Some(deployment_fact("deployment-create", "depl_cas")),
        )
        .await
        .unwrap(),
        DeploymentWriteOutcome::Applied,
        "D1"
    );
    assert_eq!(
        repo.write_deployment(
            deployment_record("depl_cas", 2, false),
            Some(0),
            1,
            Some(deployment_fact("invalid-jump", "depl_cas")),
        )
        .await
        .unwrap(),
        DeploymentWriteOutcome::Conflict,
        "D2 exact successor"
    );
    let mut left = created.clone();
    left.record.revision = 1;
    left.record.name = "left".into();
    let mut right = left.clone();
    right.record.name = "right".into();
    let mut third = right.clone();
    third.record.name = "third".into();
    let (left_outcome, right_outcome, third_outcome) = tokio::join!(
        replicas[0].write_deployment(
            left,
            Some(0),
            1,
            Some(deployment_fact("left-writer", "depl_cas")),
        ),
        replicas[1].write_deployment(
            right,
            Some(0),
            1,
            Some(deployment_fact("right-writer", "depl_cas")),
        ),
        replicas[2].write_deployment(
            third,
            Some(0),
            1,
            Some(deployment_fact("third-writer", "depl_cas")),
        )
    );
    let outcomes = [
        left_outcome.unwrap(),
        right_outcome.unwrap(),
        third_outcome.unwrap(),
    ];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == DeploymentWriteOutcome::Applied)
            .count(),
        1,
        "D3/E1"
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == DeploymentWriteOutcome::Conflict)
            .count(),
        2,
        "D3/E2"
    );
    let committed = replicas[0]
        .deployments()
        .await
        .unwrap()
        .into_iter()
        .find(|deployment| deployment.id == "depl_cas")
        .expect("D3 committed Deployment");
    assert_eq!(committed.record.revision, 1, "D3/E1 exact successor");
    assert!(
        ["left", "right", "third"].contains(&committed.record.name.as_str()),
        "D3/E1 exact candidate"
    );
    for (index, replica) in replicas.iter().enumerate() {
        assert_eq!(
            replica
                .deployments()
                .await
                .unwrap()
                .into_iter()
                .find(|deployment| deployment.id == "depl_cas"),
            Some(committed.clone()),
            "D3 replica {index} exact winner"
        );
    }
    let facts = repo.pending_lifecycle().await.unwrap();
    assert_eq!(facts.len(), baseline_facts + 2, "D1/D3 lifecycle atomicity");
    assert!(
        facts.iter().all(|fact| fact.id != "invalid-jump"),
        "D2 no fact"
    );
    assert_eq!(
        usize::from(facts.iter().any(|fact| fact.id == "left-writer"))
            + usize::from(facts.iter().any(|fact| fact.id == "right-writer"))
            + usize::from(facts.iter().any(|fact| fact.id == "third-writer")),
        1,
        "D3/E2 only winner fact"
    );

    let capacity_left = deployment_record("depl_capacity_left", 0, true);
    let capacity_right = deployment_record("depl_capacity_right", 0, true);
    let capacity_third = deployment_record("depl_capacity_third", 0, true);
    let (left_capacity, right_capacity, third_capacity) = tokio::join!(
        replicas[0].write_deployment(
            capacity_left,
            None,
            1,
            Some(deployment_fact("capacity-left", "depl_capacity_left")),
        ),
        replicas[1].write_deployment(
            capacity_right,
            None,
            1,
            Some(deployment_fact("capacity-right", "depl_capacity_right")),
        ),
        replicas[2].write_deployment(
            capacity_third,
            None,
            1,
            Some(deployment_fact("capacity-third", "depl_capacity_third")),
        )
    );
    let capacity_outcomes = [
        (
            "depl_capacity_left",
            "capacity-left",
            left_capacity.unwrap(),
        ),
        (
            "depl_capacity_right",
            "capacity-right",
            right_capacity.unwrap(),
        ),
        (
            "depl_capacity_third",
            "capacity-third",
            third_capacity.unwrap(),
        ),
    ];
    assert_eq!(
        capacity_outcomes
            .iter()
            .filter(|(_, _, outcome)| *outcome == DeploymentWriteOutcome::Applied)
            .count(),
        1,
        "D4/E3 one admitted transaction"
    );
    assert_eq!(
        capacity_outcomes
            .iter()
            .filter(|(_, _, outcome)| {
                *outcome == DeploymentWriteOutcome::ScheduledCapacityReached
            })
            .count(),
        2,
        "D4/E3 both excess transactions rejected"
    );
    let capacity_rows = repo
        .deployments()
        .await
        .unwrap()
        .into_iter()
        .filter(|deployment| deployment.id.starts_with("depl_capacity_"))
        .collect::<Vec<_>>();
    assert_eq!(capacity_rows.len(), 1, "D4/E3 no rejected candidate row");
    let capacity_winner = capacity_rows.into_iter().next().unwrap();
    let applied_capacity = capacity_outcomes
        .iter()
        .find(|(_, _, outcome)| *outcome == DeploymentWriteOutcome::Applied)
        .expect("D4/E3 applied capacity identity");
    assert_eq!(
        capacity_winner.id.as_str(),
        applied_capacity.0,
        "D4/E3 Applied identifies the durable winner"
    );
    for (index, replica) in replicas.iter().enumerate() {
        assert_eq!(
            replica
                .deployments()
                .await
                .unwrap()
                .into_iter()
                .filter(|deployment| deployment.id.starts_with("depl_capacity_"))
                .collect::<Vec<_>>(),
            vec![capacity_winner.clone()],
            "D4 replica {index} exact capacity winner"
        );
    }
    let capacity_facts = repo
        .pending_lifecycle()
        .await
        .unwrap()
        .into_iter()
        .filter(|fact| fact.id.starts_with("capacity-"))
        .collect::<Vec<_>>();
    assert_eq!(capacity_facts.len(), 1, "D4/E3 no rejected candidate fact");
    assert_eq!(
        capacity_facts[0].object_id, capacity_winner.id,
        "D4/E3 exact winner row/fact pair"
    );
    assert_eq!(
        capacity_facts[0].id.as_str(),
        applied_capacity.1,
        "D4/E3 Applied identifies the durable fact"
    );

    let collision_run = DeploymentRunView {
        id: "drun_collision".into(),
        record: DeploymentRunRecord {
            created_at: "1970-01-01T00:00:00Z".into(),
            deployment_id: "depl_cas".into(),
            workspace_id: "ws_a".into(),
            agent: DeploymentAgent::new("agent_a", 1),
            trigger: DeploymentTrigger::Manual,
            session_id: None,
            error: None,
        },
    };
    repo.upsert_deployment_run(collision_run.clone(), None)
        .await
        .expect("D5a collision setup");
    let failed_claim_id = "depl_cas:failed-then-retried";
    let mut advanced = committed.clone();
    advanced.record.revision = 2;
    advanced.record.name = "advanced".into();
    assert!(
        repo.claim_scheduled_run(
            failed_claim_id,
            1,
            advanced.clone(),
            collision_run,
            DeploymentLifecycleFact {
                id: "failed-claim-fact".into(),
                object_id: "drun_collision".into(),
                workspace_id: Some("ws_a".into()),
                event_type: "deployment_run.started".into(),
                timestamp: 1,
                runtime_interval: None,
            },
        )
        .await
        .is_err(),
        "D5a/E7 duplicate run rejects the transaction"
    );
    assert_eq!(
        repo.deployments()
            .await
            .unwrap()
            .into_iter()
            .find(|deployment| deployment.id == "depl_cas"),
        Some(committed),
        "D5a/E7 deployment update rolled back"
    );
    assert!(
        repo.pending_lifecycle()
            .await
            .unwrap()
            .iter()
            .all(|fact| fact.id != "failed-claim-fact"),
        "D5a/E7 lifecycle fact rolled back"
    );
    let run = DeploymentRunView {
        id: "drun_current".into(),
        record: DeploymentRunRecord {
            created_at: "1970-01-01T00:00:01Z".into(),
            deployment_id: "depl_cas".into(),
            workspace_id: "ws_a".into(),
            agent: DeploymentAgent::new("agent_a", 1),
            trigger: DeploymentTrigger::Schedule {
                scheduled_at: "1970-01-01T00:00:01Z".into(),
            },
            session_id: None,
            error: None,
        },
    };
    let fact = DeploymentLifecycleFact {
        id: "deployment-run-started".into(),
        object_id: run.id.clone(),
        workspace_id: Some("ws_a".into()),
        event_type: "deployment_run.started".into(),
        timestamp: 1,
        runtime_interval: None,
    };
    assert_eq!(
        repo.claim_scheduled_run(failed_claim_id, 1, advanced.clone(), run, fact)
            .await
            .unwrap(),
        ScheduledRunClaimOutcome::Claimed,
        "D5b/E4+E7"
    );
    assert_eq!(
        repo.deployments()
            .await
            .unwrap()
            .into_iter()
            .find(|deployment| deployment.id == "depl_cas"),
        Some(advanced.clone()),
        "D5b/E4 exact deployment successor"
    );
    let runs = repo.deployment_runs().await.unwrap();
    assert_eq!(runs.len(), 2, "D5b/E4 only setup and committed run");
    assert!(runs.iter().any(|run| run.id == "drun_current"), "D5b/E4");
    assert!(
        repo.pending_lifecycle().await.unwrap().iter().any(|fact| {
            fact.id == "deployment-run-started" && fact.object_id == "drun_current"
        }),
        "D5b/E4 exact run/fact pair"
    );
    let stale_claim_id = "depl_cas:stale-then-current";
    let mut stale_overwrite = advanced.clone();
    stale_overwrite.record.name = "stale-overwrite-must-not-commit".into();
    assert_eq!(
        replicas[1]
            .claim_scheduled_run(
                stale_claim_id,
                1,
                stale_overwrite,
                DeploymentRunView {
                    id: "drun_stale".into(),
                    record: DeploymentRunRecord {
                        created_at: "1970-01-01T00:00:02Z".into(),
                        deployment_id: "depl_cas".into(),
                        workspace_id: "ws_a".into(),
                        agent: DeploymentAgent::new("agent_a", 1),
                        trigger: DeploymentTrigger::Manual,
                        session_id: None,
                        error: None,
                    },
                },
                DeploymentLifecycleFact {
                    id: "stale-fact".into(),
                    object_id: "drun_stale".into(),
                    workspace_id: Some("ws_a".into()),
                    event_type: "deployment_run.started".into(),
                    timestamp: 2,
                    runtime_interval: None,
                },
            )
            .await
            .unwrap(),
        ScheduledRunClaimOutcome::StaleDeployment,
        "D6/E5"
    );
    for (index, replica) in replicas.iter().enumerate() {
        assert_eq!(
            replica
                .deployments()
                .await
                .unwrap()
                .into_iter()
                .find(|deployment| deployment.id == "depl_cas"),
            Some(advanced.clone()),
            "D6/E5 replica {index} preserves the exact winner frame"
        );
    }
    let runs = repo.deployment_runs().await.unwrap();
    assert_eq!(runs.len(), 2, "D6/E5");
    assert!(runs.iter().all(|run| run.id != "drun_stale"), "D6/E5");
    assert!(
        repo.pending_lifecycle()
            .await
            .unwrap()
            .iter()
            .all(|fact| fact.id != "stale-fact"),
        "D6/E5"
    );

    let mut after_stale = advanced;
    after_stale.record.revision = 3;
    after_stale.record.name = "fresh-after-stale".into();
    assert_eq!(
        replicas[2]
            .claim_scheduled_run(
                stale_claim_id,
                2,
                after_stale.clone(),
                DeploymentRunView {
                    id: "drun_after_stale".into(),
                    record: DeploymentRunRecord {
                        created_at: "1970-01-01T00:00:03Z".into(),
                        deployment_id: "depl_cas".into(),
                        workspace_id: "ws_a".into(),
                        agent: DeploymentAgent::new("agent_a", 1),
                        trigger: DeploymentTrigger::Schedule {
                            scheduled_at: "1970-01-01T00:00:03Z".into(),
                        },
                        session_id: None,
                        error: None,
                    },
                },
                DeploymentLifecycleFact {
                    id: "fresh-after-stale-fact".into(),
                    object_id: "drun_after_stale".into(),
                    workspace_id: Some("ws_a".into()),
                    event_type: "deployment_run.started".into(),
                    timestamp: 3,
                    runtime_interval: None,
                },
            )
            .await
            .unwrap(),
        ScheduledRunClaimOutcome::Claimed,
        "D7/E6 stale rejection did not reserve the claim identity"
    );
    for (index, replica) in replicas.iter().enumerate() {
        assert_eq!(
            replica
                .deployments()
                .await
                .unwrap()
                .into_iter()
                .find(|deployment| deployment.id == "depl_cas"),
            Some(after_stale.clone()),
            "D7/E4 replica {index} reads the fresh complete frame"
        );
    }
    let runs = repo.deployment_runs().await.unwrap();
    assert_eq!(runs.len(), 3, "D7/E4");
    assert!(runs.iter().any(|run| run.id == "drun_current"), "D7/E4");
    assert!(runs.iter().any(|run| run.id == "drun_after_stale"), "D7/E4");
    assert!(runs.iter().all(|run| run.id != "drun_stale"), "D7/E5");
    let facts = repo.pending_lifecycle().await.unwrap();
    assert!(facts.iter().all(|fact| fact.id != "stale-fact"), "D7/E5");
    assert!(
        facts.iter().any(|fact| fact.id == "fresh-after-stale-fact"),
        "D7/E4"
    );
}

// ── Backend rows: each must pass the identical universal suite ───────────────────

#[test]
fn sqlite_backend_conforms() {
    block(async {
        run_suite(|| {
            SqliteManagedSessionRepository::open_in_memory().expect("sqlite in-memory repo")
        })
        .await;
        create_receipt_race_decision_table(std::sync::Arc::new(
            SqliteManagedSessionRepository::open_in_memory().expect("sqlite create-race repo"),
        ))
        .await;
        concurrent_phase_transition_has_one_count_snapshot(std::sync::Arc::new(
            SqliteManagedSessionRepository::open_in_memory().expect("sqlite phase-count repo"),
        ))
        .await;
        // SQLite refinement rule D8: C6 three independently opened adapters
        // share one file and execute D3-D7; C7 every handle then closes.
        // Effects: E7 D3/D4 still have one exact CAS/capacity winner; E8 reopen
        // retains the capacity winner/fact, exact revision-three frame and both
        // valid runs, excluding every rejected D4/D6 effect.
        let directory = tempfile::tempdir().expect("SQLite Deployment CAS directory");
        let database = directory.path().join("deployment-cas.db");
        let database_path = database.to_str().expect("UTF-8 Deployment CAS path");
        let left = SqliteManagedSessionRepository::open(database_path)
            .expect("left SQLite Deployment replica");
        let right = SqliteManagedSessionRepository::open(database_path)
            .expect("right SQLite Deployment replica");
        let third = SqliteManagedSessionRepository::open(database_path)
            .expect("third SQLite Deployment replica");
        deployment_cas_decision_table(&left, [&left, &right, &third]).await;
        drop(third);
        drop(right);
        drop(left);
        let reopened = SqliteManagedSessionRepository::open(database_path)
            .expect("reopen SQLite Deployment authority");
        let deployment = reopened
            .deployments()
            .await
            .expect("read reopened Deployments")
            .into_iter()
            .find(|deployment| deployment.id == "depl_cas")
            .expect("reopened CAS winner");
        assert_eq!(deployment.record.revision, 3, "D8 persisted successor");
        assert_eq!(
            deployment.record.name, "fresh-after-stale",
            "D8 persisted frame"
        );
        assert_eq!(
            reopened
                .deployment_runs()
                .await
                .expect("read reopened DeploymentRuns")
                .len(),
            3,
            "D8 only valid claim/runs survived reopen"
        );
        let reopened_facts = reopened
            .pending_lifecycle()
            .await
            .expect("read reopened Deployment lifecycle facts");
        assert!(
            reopened_facts
                .iter()
                .all(|fact| fact.id != "failed-claim-fact"),
            "D8 failed claim fact remained absent"
        );
        assert!(
            reopened_facts.iter().any(|fact| {
                fact.id == "deployment-run-started" && fact.object_id == "drun_current"
            }),
            "D8 retried claim's exact run/fact pair survived reopen"
        );
        let capacity_rows = reopened
            .deployments()
            .await
            .expect("read reopened capacity winner")
            .into_iter()
            .filter(|deployment| deployment.id.starts_with("depl_capacity_"))
            .collect::<Vec<_>>();
        let capacity_facts = reopened_facts
            .into_iter()
            .filter(|fact| fact.id.starts_with("capacity-"))
            .collect::<Vec<_>>();
        assert_eq!(capacity_rows.len(), 1, "D8 one durable capacity winner");
        assert_eq!(capacity_facts.len(), 1, "D8 no rejected capacity fact");
        assert_eq!(
            capacity_facts[0].object_id, capacity_rows[0].id,
            "D8 exact durable capacity row/fact pair"
        );
    });
}

#[test]
fn sqlite_lifecycle_outbox_insert_failure_rolls_back_every_root_path() {
    block(async {
        // Backend rule S1: run A1-A3 against a file-backed production adapter;
        // a SQLite BEFORE INSERT abort is the C3 database failure, and every
        // subsequent assertion reads through the same repository contract.
        let directory = tempfile::tempdir().expect("SQLite atomicity directory");
        let database = directory.path().join("lifecycle-atomicity.db");
        let database_path = database.to_str().expect("UTF-8 lifecycle atomicity path");
        let repo = SqliteManagedSessionRepository::open(database_path)
            .expect("open SQLite lifecycle atomicity repository");
        lifecycle_outbox_insert_failure_rolls_back_root_paths(&repo, || {
            rusqlite::Connection::open(&database)
                .expect("open SQLite failure injector")
                .execute_batch(
                    "CREATE TRIGGER reject_lifecycle_outbox_insert \
                     BEFORE INSERT ON managed_lifecycle_outbox \
                     BEGIN \
                       SELECT RAISE(ABORT, 'injected lifecycle outbox failure'); \
                     END;",
                )
                .expect("install SQLite lifecycle outbox rejection");
            std::future::ready(())
        })
        .await;
    });
}

#[test]
fn sqlite_restores_and_canonically_rewrites_the_pre_typed_deployment_record() {
    block(async {
        // This fixture is frozen old-release output, not JSON produced by the
        // current Rust types. It covers the upgrade edge that ordinary
        // write-then-read repository tests cannot exercise.
        let directory = tempfile::tempdir().expect("temporary compatibility database");
        let database = directory.path().join("managed.db");
        let database_path = database.to_str().expect("UTF-8 test path");
        let repo = SqliteManagedSessionRepository::open(database_path)
            .expect("initialize old-record compatibility database");
        let old_record = include_str!("fixtures/deployment-record-v0-managed-wire.json");

        rusqlite::Connection::open(&database)
            .expect("open fixture database")
            .execute(
                "INSERT INTO managed_deployment \
                 (deployment_id, workspace_id, revision, data) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    "depl_upgrade_fixture",
                    "workspace_upgrade_fixture",
                    1_i64,
                    old_record
                ],
            )
            .expect("insert exact old-release Deployment document");

        let mut restored = repo
            .deployments()
            .await
            .expect("old official Managed event spelling must restore")
            .pop()
            .expect("fixture Deployment");
        assert_eq!(restored.record.revision, 1);
        assert_eq!(restored.record.initial_events.len(), 1);

        restored.record.revision = 2;
        restored.record.updated_at = "2026-08-25T00:00:00Z".into();
        assert_eq!(
            repo.write_deployment(restored, Some(1), 10, None)
                .await
                .expect("rewrite restored Deployment"),
            DeploymentWriteOutcome::Applied
        );
        drop(repo);

        let stored: String = rusqlite::Connection::open(&database)
            .expect("inspect rewritten database")
            .query_row(
                "SELECT data FROM managed_deployment WHERE deployment_id=?1",
                ["depl_upgrade_fixture"],
                |row| row.get(0),
            )
            .expect("rewritten Deployment document");
        let stored: serde_json::Value =
            serde_json::from_str(&stored).expect("valid rewritten Deployment JSON");
        assert_eq!(stored["initial_events"][0]["type"], "user_message");

        let reopened = SqliteManagedSessionRepository::open(database_path)
            .expect("reopen compatibility database");
        assert_eq!(
            reopened
                .deployments()
                .await
                .expect("rewritten Deployment survives restart")[0]
                .record
                .revision,
            2
        );
    });
}

#[tokio::test]
async fn postgres_root_cas_conforms_to_the_same_decision_table() {
    use awaken_session_store::PostgresManagedSessionRepository;
    use sqlx::Executor;
    use sqlx::postgres::{PgPool, PgPoolOptions};

    // Fixture admission table: no configured URL + unreachable local default
    // may skip; configured URL + unreachable database must fail the gate.
    let configured = match std::env::var("AWAKEN_TEST_DATABASE_URL") {
        Ok(url) => Some(url),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            panic!("AWAKEN_TEST_DATABASE_URL is not valid Unicode")
        }
    };
    let url = configured.clone().unwrap_or_else(|| {
        "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_owned()
    });
    let admin = match PgPool::connect(&url).await {
        Ok(pool) => pool,
        Err(error) if configured.is_none() => {
            println!("[skip] no Postgres reachable: {error}");
            return;
        }
        Err(error) => panic!("configured AWAKEN_TEST_DATABASE_URL is unreachable: {error}"),
    };
    let _ = admin
        .execute("DROP SCHEMA IF EXISTS t_session_root_cas CASCADE")
        .await;
    admin
        .execute("CREATE SCHEMA t_session_root_cas")
        .await
        .expect("create Session CAS schema");
    admin.close().await;
    let pool = PgPoolOptions::new()
        .after_connect(|connection, _| {
            Box::pin(async move {
                connection
                    .execute("SET search_path = t_session_root_cas")
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .expect("connect Session CAS schema");
    let repo = std::sync::Arc::new(
        PostgresManagedSessionRepository::with_pool(pool.clone())
            .await
            .expect("open Postgres Session repository"),
    );
    // PostgreSQL refinement rule D9: C8 three repository clients share one
    // migrated pool/schema and execute D3-D7. Effect E9 includes the same exact
    // capacity winner with rejected-row/fact absence, stale zero-effect and
    // reusable-claim result as SQLite. An unavailable endpoint skips this
    // entire rule and is never reported as live PostgreSQL evidence.
    let second_deployment_replica = PostgresManagedSessionRepository::with_pool(pool.clone())
        .await
        .expect("open second Postgres Deployment replica");
    let third_deployment_replica = PostgresManagedSessionRepository::with_pool(pool.clone())
        .await
        .expect("open third Postgres Deployment replica");
    global_environment_phase_count_has_no_recovery_batch_limit(repo.as_ref()).await;
    tombstone_requires_hidden_completed_cleanup(repo.as_ref()).await;
    deleted_event_admission_decision_table(repo.as_ref()).await;
    root_cas_decision_table(repo.as_ref()).await;
    create_receipt_decision_table(repo.as_ref()).await;
    create_receipt_race_decision_table(repo.clone()).await;
    concurrent_root_cas_has_one_authoritative_winner(repo.clone()).await;
    concurrent_phase_transition_has_one_count_snapshot(repo.clone()).await;
    credential_source_dependency_decision_table(repo.as_ref()).await;
    deployment_cas_decision_table(
        repo.as_ref(),
        [
            repo.as_ref(),
            &second_deployment_replica,
            &third_deployment_replica,
        ],
    )
    .await;
    lifecycle_outbox_tracks_every_committed_transition(repo.as_ref()).await;
    lifecycle_outbox_insert_failure_rolls_back_root_paths(repo.as_ref(), || {
        let pool = pool.clone();
        async move {
            // Backend rule P1: a NOT VALID constraint preserves successful
            // fixture rows but rejects every new outbox INSERT. A1-A3 then
            // observe PostgreSQL's real transaction rollback through the port.
            sqlx::query(
                "ALTER TABLE managed_lifecycle_outbox \
                 ADD CONSTRAINT reject_lifecycle_outbox_insert CHECK (false) NOT VALID",
            )
            .execute(&pool)
            .await
            .expect("install PostgreSQL lifecycle outbox rejection");
        }
    })
    .await;
}
