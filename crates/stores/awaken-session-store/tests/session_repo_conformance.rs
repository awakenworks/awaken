//! Trait-generic **conformance suite** for the `ManagedSessionRepository` port — the
//! shared behavioural contract every backend must satisfy (ADR-0059, the
//! `awaken-store-conformance` pattern).
//!
//! Every save also persists an owner scope in the same repository operation. The generic
//! suite checks that universal invariant for both backends; durable restart persistence
//! remains in the backend-specific suite.

use awaken_session_contract::{
    IdempotencyRecord, ManagedLifecycleFact, ManagedSessionRepository, McpAttachmentDraft,
    McpAttachmentOrigin, McpTarget, PersistedSession, ScopedPersistedSession, SessionMutation,
    SessionMutationPayload, SessionMutationResult, SessionRepositoryError, SessionRevision,
    SessionTombstone,
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
        config_fingerprint: awaken_session_contract::EnvironmentFingerprint("env-4".into()),
        sandbox: json!({"isolation": "namespace"}),
        sandbox_provisioning: Default::default(),
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
                    mcp_authoring: Default::default(),
                    agent_id: "assistant".into(),
                    model: "kimi".into(),
                    runtime: Some("acp:custom".into()),
                    application: None,
                    delegate_ids: vec!["researcher".into()],
                    toolsets: Vec::new(),
                    mounts: Vec::new(),
                    env: Vec::new(),
                    prompts: Vec::new(),
                },
            ),
        ),
        title: Some(title.to_string()),
        metadata: std::collections::BTreeMap::from([("k".into(), "v".into())]),
        tools: Default::default(),
        environment_binding: None,
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
        resources: awaken_session_contract::SessionResourceState::from_legacy(
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
        status: "idle".into(),
        archived_at: None,
    }
}

fn fact(id: &str, session_id: &str, event_type: &str) -> ManagedLifecycleFact {
    ManagedLifecycleFact {
        id: id.into(),
        object_id: session_id.into(),
        workspace_id: Some("ws_a".into()),
        event_type: event_type.into(),
        timestamp: 1_700_000_000,
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
    value.revision = repo
        .create(
            owner,
            value.clone(),
            record(&format!("test:create:{}", value.session_id), &payload),
            facts,
        )
        .await
        .expect("create Session fixture");
    value
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

// ── The universal port contract, trait-generic over any backend ──────────────────

/// Round-trip: a saved aggregate reads back byte-for-byte (every field persists).
async fn save_get_round_trips<R: ManagedSessionRepository>(r: &R) {
    let want = create_session(r, "default", session("sesn_1", "hello"), Vec::new()).await;
    assert_eq!(
        r.get("sesn_1").await,
        Some(want),
        "the full aggregate must round-trip"
    );
}

/// Absent id → None (no fabrication, fail-closed read).
async fn absent_id_reads_none<R: ManagedSessionRepository>(r: &R) {
    assert!(r.get("never-saved").await.is_none());
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
    assert_eq!(
        r.get("sesn_1").await.and_then(|s| s.title),
        Some("second".into())
    );
}

/// A visible row and its owner are one write: no backend may expose the row with
/// a missing or stale scope after aggregate creation or replacement returns.
async fn ownership_is_one_atomic_repository_fact<R: ManagedSessionRepository>(r: &R) {
    create_session(r, "ws_a", session("sesn_owned", "owned"), Vec::new()).await;
    assert!(r.get("sesn_owned").await.is_some());
    assert_eq!(r.owner("sesn_owned").await.as_deref(), Some("ws_a"));

    replace_session(
        r,
        "ws_a",
        session("sesn_owned", "updated"),
        "test:replace:owned",
        Vec::new(),
    )
    .await;
    assert_eq!(r.owner("sesn_owned").await.as_deref(), Some("ws_a"));
    assert_eq!(
        r.get("sesn_owned").await.and_then(|s| s.title),
        Some("updated".into())
    );
}

/// Binding is a narrow update: it fails closed for an unknown id and changes no
/// other aggregate field for a known Session.
async fn environment_binding_is_atomic_and_non_destructive<R: ManagedSessionRepository>(r: &R) {
    assert!(r.get("unknown").await.is_none());
    let want = session("sesn_bound", "unchanged");
    create_session(r, "ws_a", want.clone(), Vec::new()).await;
    let mut bound = want.clone();
    bound.environment_binding = Some(r#"{"provider_kind":"bwrap"}"#.into());
    replace_session(r, "ws_a", bound, "test:bind", Vec::new()).await;
    let mut got = r.get("sesn_bound").await.expect("bound Session");
    assert_eq!(
        got.environment_binding.as_deref(),
        Some(r#"{"provider_kind":"bwrap"}"#)
    );
    got.environment_binding = None;
    got.revision = Default::default();
    assert_eq!(got, want, "binding update preserves every other field");
    assert_eq!(r.owner("sesn_bound").await.as_deref(), Some("ws_a"));
}

/// The lifecycle fact is committed in the same repository transaction as the
/// aggregate and owner. Notification may crash afterwards without losing the fact.
async fn lifecycle_outbox_tracks_every_committed_transition<R: ManagedSessionRepository>(r: &R) {
    let created = fact("evt:create", "sesn_lifecycle", "session.created");
    create_session(
        r,
        "ws_a",
        session("sesn_lifecycle", "lifecycle"),
        vec![created.clone()],
    )
    .await;
    assert!(r.get("sesn_lifecycle").await.is_some());
    assert_eq!(r.owner("sesn_lifecycle").await.as_deref(), Some("ws_a"));
    assert_eq!(r.pending_lifecycle().await, vec![created.clone()]);

    // Stable event identity makes an enqueue retry a no-op.
    r.append_lifecycle(created.clone()).await;
    assert_eq!(r.pending_lifecycle().await, vec![created.clone()]);
    r.complete_lifecycle(&created.id).await;
    assert!(r.pending_lifecycle().await.is_empty());

    let archived = fact("evt:archive", "sesn_lifecycle", "session.archived");
    let mut archive = r.get("sesn_lifecycle").await.unwrap();
    archive.status = "terminated".into();
    archive.archived_at = Some("2026-07-19T00:00:00Z".into());
    replace_session(r, "ws_a", archive, "test:archive", vec![archived.clone()]).await;
    let durable = r.get("sesn_lifecycle").await.expect("archived session");
    assert_eq!(durable.status, "terminated");
    assert_eq!(durable.archived_at.as_deref(), Some("2026-07-19T00:00:00Z"));
    assert_eq!(r.pending_lifecycle().await, vec![archived.clone()]);
    r.complete_lifecycle(&archived.id).await;

    let deleted = fact("evt:delete", "sesn_lifecycle", "session.deleted");
    let current = r.get("sesn_lifecycle").await.unwrap();
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
    assert!(r.get("sesn_lifecycle").await.is_none());
    assert_eq!(r.pending_lifecycle().await, vec![deleted]);
}

/// Prepared/Releasing activations remain discoverable after a process crash;
/// terminal Active/Released/Failed records do not create reconciliation work.
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
        r.reconcilable_sessions().await,
        vec![ScopedPersistedSession {
            workspace_id: "ws_a".into(),
            session: pending.clone(),
        }]
    );

    pending.resources.start_attempt().unwrap();
    pending.resources.commit().unwrap();
    pending = replace_session(r, "ws_a", pending, "test:resource-active", Vec::new()).await;
    let indexed = r.reconcilable_sessions().await;
    assert_eq!(indexed.len(), 1, "active MCP remains restart work");
    assert!(indexed[0].session.mcp.needs_reconciliation());

    pending.mcp.attachments[0].state = awaken_session_contract::McpAttachmentState::Failed;
    replace_session(r, "ws_a", pending, "test:mcp-failed", Vec::new()).await;
    assert!(r.reconcilable_sessions().await.is_empty());
}

#[derive(Clone, Copy)]
enum CasRule {
    Create,
    CreateReplay,
    CreateIdempotencyMismatch,
    Replace,
    ReplaceReplay,
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
    // | R6 | replace retry | mismatch | T | T | T | idempotency mismatch |
    // | R7 | replace | absent | T | T | F | conflict revision 1 |
    // | R8 | replace | absent | T | F | T | conflict revision 1 |
    // | R9 | delete | absent | T | T | T | applied revision 2 + hidden |
    // | R10 | delete retry | equal | tombstone | T | old | replayed revision 2 |
    let rules = [
        CasRule::Create,
        CasRule::CreateReplay,
        CasRule::CreateIdempotencyMismatch,
        CasRule::Replace,
        CasRule::ReplaceReplay,
        CasRule::ReplaceIdempotencyMismatch,
        CasRule::StaleRevision,
        CasRule::WrongOwner,
        CasRule::Delete,
        CasRule::DeleteReplay,
    ];
    for (index, rule) in rules.into_iter().enumerate() {
        let id = format!("sesn_cas_{index}");
        let initial = session(&id, "initial");
        let create_payload = SessionMutationPayload::Replace(initial.clone());
        let create_record = record("create", &create_payload);
        let created = repo
            .create("ws_a", initial, create_record.clone(), Vec::new())
            .await
            .expect("initial CAS create");
        assert_eq!(created, SessionRevision(1));
        assert_eq!(
            repo.idempotency_receipt(&id, "create").await,
            Some(awaken_session_contract::SessionIdempotencyReceipt {
                payload_hash: create_record.payload_hash.clone(),
                committed_revision: SessionRevision(1),
            }),
            "the decision table reads the same atomic receipt it writes"
        );

        match rule {
            CasRule::Create => {}
            CasRule::CreateReplay => {
                let mut replay = session(&id, "initial");
                replay.revision = SessionRevision(0);
                assert_eq!(
                    repo.create("ws_a", replay, create_record, Vec::new())
                        .await
                        .unwrap(),
                    SessionRevision(1)
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
                assert_eq!(error, SessionRepositoryError::IdempotencyMismatch);
            }
            CasRule::Replace
            | CasRule::ReplaceReplay
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
                assert!(repo.get(&id).await.is_none());
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

async fn run_suite<R: ManagedSessionRepository>(fresh: impl Fn() -> R) {
    save_get_round_trips(&fresh()).await;
    absent_id_reads_none(&fresh()).await;
    save_is_idempotent_upsert(&fresh()).await;
    ownership_is_one_atomic_repository_fact(&fresh()).await;
    environment_binding_is_atomic_and_non_destructive(&fresh()).await;
    lifecycle_outbox_tracks_every_committed_transition(&fresh()).await;
    pending_resource_activation_index_is_durable(&fresh()).await;
    let cas_repo = fresh();
    root_cas_decision_table(&cas_repo).await;
}

// ── Backend rows: each must pass the identical universal suite ───────────────────

#[test]
fn sqlite_backend_conforms() {
    block(run_suite(|| {
        SqliteManagedSessionRepository::open_in_memory().expect("sqlite in-memory repo")
    }));
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
    let repo = PostgresManagedSessionRepository::with_pool(pool)
        .await
        .expect("open Postgres Session repository");
    root_cas_decision_table(&repo).await;
}
