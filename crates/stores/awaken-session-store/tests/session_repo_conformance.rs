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
    SessionExecutionState, SessionMutation, SessionMutationPayload, SessionMutationResult,
    SessionRepositoryConflict, SessionRepositoryError, SessionRevision, SessionTombstone,
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
    let completion = awaken_session_contract::SessionCleanupCompletion::new(&command, Vec::new());
    let verified = completion
        .verify(&command)
        .expect("exact completion becomes a verified receipt");
    assert!(
        value
            .terminal_cleanup
            .complete(&session_id, &[verified])
            .expect("complete cleanup operation")
    );
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
    assert!(deleting.request_delete());
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
    assert_eq!(r.pending_lifecycle().await.unwrap(), vec![created.clone()]);

    // Stable event identity makes an enqueue retry a no-op.
    r.append_lifecycle(created.clone()).await.unwrap();
    assert_eq!(r.pending_lifecycle().await.unwrap(), vec![created.clone()]);
    r.complete_lifecycle(&created.id).await.unwrap();
    assert!(r.pending_lifecycle().await.unwrap().is_empty());

    let archived = fact("evt:archive", "sesn_lifecycle", "session.archived");
    let mut archive = r.get("sesn_lifecycle").await.unwrap();
    archive.archive("2026-07-19T00:00:00Z").unwrap();
    replace_session(r, "ws_a", archive, "test:archive", vec![archived.clone()]).await;
    let durable = r.get("sesn_lifecycle").await.expect("archived session");
    assert_eq!(durable.execution, SessionExecutionState::Terminated);
    assert_eq!(durable.archived_at(), Some("2026-07-19T00:00:00Z"));
    assert_eq!(r.pending_lifecycle().await.unwrap(), vec![archived.clone()]);
    r.complete_lifecycle(&archived.id).await.unwrap();

    let deleted = fact("evt:delete", "sesn_lifecycle", "session.deleted");
    let mut current = r.get("sesn_lifecycle").await.unwrap();
    assert!(current.request_delete(), "archive remains deletable");
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
    assert_eq!(r.pending_lifecycle().await.unwrap(), vec![deleted]);
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
    assert!(pending.request_delete());
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
    assert!(completed.request_delete());
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
            assert!(initial.request_delete());
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
                    assert!(expected.request_delete());
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
    // E6 invalid owner is rejected before writes.
    //
    // | Rule | Receipt | Identity | Owner | Hash | Effect |
    // |---|---|---|---|---|---|
    // | C1 | absent | absent | exact | valid | E1 |
    // | C2 | exact | live+advanced | exact | exact | E2 |
    // | C3 | exact/absent | live | foreign/exact | exact/other key | E3 |
    // | C4 | present | live | exact | mismatch | E4 |
    // | C5 | exact | tombstone | exact | exact | E5 |
    // | C6 | any | any | empty | any | E6 |
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
    assert!(tombstoned.request_delete());
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
    assert!(matches!(
        repo.create(
            "",
            session("sesn_empty_owner", "invalid"),
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
        repo.get("sesn_empty_owner").await,
        Err(SessionRepositoryError::NotFound),
        "C6/E6"
    );
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
    assert!(deletable.request_delete());
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

async fn run_suite<R: ManagedSessionRepository>(fresh: impl Fn() -> R) {
    save_get_round_trips(&fresh()).await;
    absent_id_reads_none(&fresh()).await;
    save_is_idempotent_upsert(&fresh()).await;
    ownership_is_one_atomic_repository_fact(&fresh()).await;
    environment_state_is_atomic_and_non_destructive(&fresh()).await;
    vault_reference_index_returns_only_live_scoped_sessions(&fresh()).await;
    credential_source_dependency_decision_table(&fresh()).await;
    lifecycle_outbox_tracks_every_committed_transition(&fresh()).await;
    pending_resource_activation_index_is_durable(&fresh()).await;
    tombstone_requires_hidden_completed_cleanup(&fresh()).await;
    let cas_repo = fresh();
    root_cas_decision_table(&cas_repo).await;
    create_receipt_decision_table(&fresh()).await;
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

fn deployment_fact(id: &str) -> DeploymentLifecycleFact {
    DeploymentLifecycleFact {
        id: id.into(),
        object_id: "depl_cas".into(),
        workspace_id: Some("ws_a".into()),
        event_type: "deployment.updated".into(),
        timestamp: 1,
        runtime_interval: None,
    }
}

async fn deployment_cas_decision_table<R: DeploymentRepository + ManagedSessionRepository>(
    repo: &R,
) {
    // Deployment repository cause/effect graph:
    // C1=create or two writers share revision r; C2=row is scheduled; C3=the
    // capacity limit is already occupied; C4=scheduler presents current or
    // stale revision. Effects: E1=one CAS write advances r exactly once;
    // E2=the loser conflicts without a lifecycle fact; E3=capacity admission
    // is transactional; E4=claim+run+cursor commit atomically; E5=stale claim
    // creates neither claim nor run.
    //
    // | Rule | C1                | C2 | C3 | C4      | Effect       |
    // | D1   | create            | F  | F  | -       | applied      |
    // | D2   | jump/reuse r      | F  | F  | -       | E2 reject   |
    // | D3   | two writers at r  | F  | F  | -       | E1 + E2     |
    // | D4   | create            | T  | T  | -       | E3 reject   |
    // | D5   | scheduler at r    | -  | -  | current | E4          |
    // | D6   | scheduler at r-1  | -  | -  | stale   | E5          |
    let baseline_facts = repo.pending_lifecycle().await.unwrap().len();
    let created = deployment_record("depl_cas", 0, false);
    assert_eq!(
        repo.write_deployment(
            created.clone(),
            None,
            1,
            Some(deployment_fact("deployment-create")),
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
            Some(deployment_fact("invalid-jump")),
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
    let (left_outcome, right_outcome) = tokio::join!(
        repo.write_deployment(left, Some(0), 1, Some(deployment_fact("left-writer")),),
        repo.write_deployment(right, Some(0), 1, Some(deployment_fact("right-writer")),)
    );
    let outcomes = [left_outcome.unwrap(), right_outcome.unwrap()];
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
        1,
        "D3/E2"
    );
    let facts = repo.pending_lifecycle().await.unwrap();
    assert_eq!(facts.len(), baseline_facts + 2, "D1/D3 lifecycle atomicity");
    assert!(
        facts.iter().all(|fact| fact.id != "invalid-jump"),
        "D2 no fact"
    );
    assert_eq!(
        usize::from(facts.iter().any(|fact| fact.id == "left-writer"))
            + usize::from(facts.iter().any(|fact| fact.id == "right-writer")),
        1,
        "D3/E2 only winner fact"
    );

    assert_eq!(
        repo.write_deployment(deployment_record("depl_scheduled", 0, true), None, 1, None)
            .await
            .unwrap(),
        DeploymentWriteOutcome::Applied,
        "D4 setup"
    );
    assert_eq!(
        repo.write_deployment(deployment_record("depl_over_limit", 0, true), None, 1, None)
            .await
            .unwrap(),
        DeploymentWriteOutcome::ScheduledCapacityReached,
        "D4/E3"
    );

    let mut advanced = deployment_record("depl_cas", 2, false);
    advanced.record.name = "advanced".into();
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
        repo.claim_scheduled_run("depl_cas:instant", 1, advanced.clone(), run, fact)
            .await
            .unwrap(),
        ScheduledRunClaimOutcome::Claimed,
        "D5/E4"
    );
    assert_eq!(repo.deployment_runs().await.unwrap().len(), 1, "D5/E4");
    assert_eq!(
        repo.claim_scheduled_run(
            "depl_cas:stale",
            1,
            advanced,
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
    assert_eq!(repo.deployment_runs().await.unwrap().len(), 1, "D6/E5");
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
        deployment_cas_decision_table(
            &SqliteManagedSessionRepository::open_in_memory().expect("sqlite deployment repo"),
        )
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

    let url = std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_string()
    });
    let Ok(admin) = PgPool::connect(&url).await else {
        println!("[skip] no Postgres reachable");
        return;
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
        PostgresManagedSessionRepository::with_pool(pool)
            .await
            .expect("open Postgres Session repository"),
    );
    root_cas_decision_table(repo.as_ref()).await;
    create_receipt_decision_table(repo.as_ref()).await;
    create_receipt_race_decision_table(repo.clone()).await;
    credential_source_dependency_decision_table(repo.as_ref()).await;
    deployment_cas_decision_table(repo.as_ref()).await;
}
