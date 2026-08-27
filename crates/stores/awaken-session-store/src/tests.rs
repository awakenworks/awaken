use std::collections::BTreeMap;

use awaken_credential_contract::{
    CredentialRealizationProfile, PlaintextBoundary, PlaintextHolder,
};
use awaken_session_contract::{
    EnvironmentFingerprint, EnvironmentSnapshot, McpAttachmentDraft, McpAttachmentOrigin,
    McpTarget, SessionBaseline, SessionBaselineState, SessionMcpAttachmentSet,
    SessionMcpAuthoringContext, SessionNetworkPolicy,
};

use super::*;
use std::time::Duration;

#[test]
fn lifecycle_decoder_rejects_missing_authoritative_fields_but_accepts_legacy_object_id() {
    assert!(decode_lifecycle("{}").is_err());
    let legacy = decode_lifecycle(
        r#"{"id":"fact-1","session_id":"sesn-1","event_type":"created","timestamp":1}"#,
    )
    .expect("legacy session_id alias remains readable");
    assert_eq!(legacy.object_id, "sesn-1");
    assert_eq!(legacy.workspace_id, None);
}

#[tokio::test]
async fn recovery_scans_fail_closed_without_panicking_the_supervisor() {
    /* FMECA cause/effect decision table. Causes: C1 the Postgres authority
     * is unavailable; C2 a durable lifecycle row cannot decode; C3 one
     * Session row is corrupt while another is healthy. Effects: E1 expose a
     * typed outage/error; E2 preserve durable rows; E3 durably quarantine only
     * the corrupt Session; E4 continue healthy recovery. Rules: healthy scans
     * are covered by repository conformance; R1 C1=>E1; R2 C2=>E1+E2;
     * R3 C3=>E2+E3+E4; R4 a later codec recognizes the isolated row=>clear
     * stale quarantine and resume reconciliation. A storage-wide outage fails
     * the scan; row-local decode corruption is isolated because returning
     * neither row would silently lose unrelated durable work. */
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(10))
        .connect_lazy("postgres://localhost/awaken")
        .unwrap();
    let postgres = PostgresManagedSessionRepository {
        pool,
        handle: tokio::runtime::Handle::current(),
    };
    postgres.pool.close().await;
    assert!(
        postgres.pending_lifecycle().await.is_err(),
        "R1 typed outbox"
    );
    assert!(
        postgres.reconcilable_sessions().await.is_err(),
        "R1 typed reconciliation"
    );

    let dir = tempfile::tempdir().unwrap();
    let sqlite =
        SqliteManagedSessionRepository::open(&dir.path().join("recovery.db").to_string_lossy())
            .unwrap();
    sqlite
        .conn
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO managed_lifecycle_outbox (fact_id, data) VALUES (?1, ?2)",
            params!["corrupt", "{"],
        )
        .unwrap();
    assert!(sqlite.pending_lifecycle().await.is_err(), "R2 typed error");

    let recoverable =
        create_fixture(&sqlite, "workspace", sample("sesn_corrupt"), Vec::new()).await;
    create_fixture(&sqlite, "workspace", sample("sesn_healthy"), Vec::new()).await;
    sqlite
        .conn
        .lock()
        .unwrap()
        .execute(
            "UPDATE managed_session SET aggregate_json = ?1 WHERE session_id = ?2",
            params!["{", "sesn_corrupt"],
        )
        .unwrap();
    let scan = sqlite.reconcilable_sessions().await.unwrap();
    assert_eq!(scan.sessions.len(), 1, "R3/E4 healthy work continues");
    assert_eq!(scan.sessions[0].session.session_id, "sesn_healthy");
    assert_eq!(
        scan.quarantined,
        vec![SessionRecoveryQuarantine {
            session_id: "sesn_corrupt".into(),
            reason: scan.quarantined[0].reason.clone(),
        }],
        "R3/E3 exposes secret-free durable isolation evidence"
    );
    let replay = sqlite.reconcilable_sessions().await.unwrap();
    assert_eq!(replay.sessions.len(), 1);
    assert_eq!(replay.quarantined, scan.quarantined, "R3 idempotent replay");
    let quarantined_rows: i64 = sqlite
        .conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM managed_session_quarantine WHERE session_id = ?1",
            params!["sesn_corrupt"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(quarantined_rows, 1, "R3 durable quarantine");

    let mut recognized_legacy = serde_json::to_value(recoverable).unwrap();
    recognized_legacy
        .as_object_mut()
        .unwrap()
        .remove("event_batches");
    recognized_legacy
        .as_object_mut()
        .unwrap()
        .remove("active_activity_epochs");
    sqlite
        .conn
        .lock()
        .unwrap()
        .execute(
            "UPDATE managed_session SET aggregate_json = ?1 WHERE session_id = ?2",
            params![recognized_legacy.to_string(), "sesn_corrupt"],
        )
        .unwrap();
    let healed = sqlite.reconcilable_sessions().await.unwrap();
    assert!(healed.quarantined.is_empty(), "R4 stale quarantine cleared");
    assert!(
        healed
            .sessions
            .iter()
            .any(|row| row.session.session_id == "sesn_corrupt"),
        "R4 recognized historical row resumes recovery"
    );
    let quarantined_rows: i64 = sqlite
        .conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM managed_session_quarantine WHERE session_id = ?1",
            params!["sesn_corrupt"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(quarantined_rows, 0, "R4 durable stale evidence removed");
}

/// Shared-file write-admission causal graph:
/// another aggregate owns the SQLite writer reservation × wait budget.
///
/// | Rule | competing writer | wait budget | Result |
/// |---|---|---|---|
/// | W1 | no | any | commit immediately |
/// | W2 | yes | sufficient | wait, then commit once |
/// | W3 | yes | exhausted | storage failure |
///
/// W1 is covered by every SQLite repository test; this case protects W2.
/// SQLite itself owns W3 and returns the typed storage failure after the
/// configured bound, so the repository does not add a parallel retry loop.
#[test]
fn sqlite_create_waits_for_a_competing_aggregate_writer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shared.db");
    let path = path.to_string_lossy().to_string();
    let repo = Arc::new(SqliteManagedSessionRepository::open(&path).unwrap());
    let mut blocker = Connection::open(&path).unwrap();
    let blocker_tx = blocker
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let writer = {
        let repo = repo.clone();
        std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(create_fixture(
                    repo.as_ref(),
                    "default",
                    sample("sesn_waiting_writer"),
                    Vec::new(),
                ))
        })
    };
    started_rx.recv().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    blocker_tx.commit().unwrap();

    let created = writer.join().unwrap();
    assert_eq!(created.session_id, "sesn_waiting_writer", "W2");
}

#[test]
fn sqlite_recovery_scan_waits_before_reading_and_repairing_quarantine() {
    // R1 another aggregate holds the writer reservation when recovery starts.
    // R2 recovery must wait before taking its read snapshot, then atomically
    // read the aggregate and repair quarantine evidence. A deferred read followed
    // by DELETE/UPSERT would instead fail with SQLITE_BUSY_SNAPSHOT.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shared-recovery.db");
    let path = path.to_string_lossy().to_string();
    let repo = Arc::new(SqliteManagedSessionRepository::open(&path).unwrap());
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(create_fixture(
            repo.as_ref(),
            "default",
            sample("sesn_waiting_recovery"),
            Vec::new(),
        ));

    let mut blocker = Connection::open(&path).unwrap();
    let blocker_tx = blocker
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let recovery = {
        let repo = repo.clone();
        std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(repo.reconcilable_sessions())
        })
    };
    started_rx.recv().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    blocker_tx.commit().unwrap();

    let scan = recovery.join().unwrap().expect("R2 recovery scan");
    assert!(scan.quarantined.is_empty(), "R2");
}

#[test]
fn sqlite_migration_startup_is_concurrent_and_replay_safe() {
    /* MIG-01/MIG-02 cause/effect decision table. Causes: C1 fresh database,
     * C2 two simultaneous Session-store starters, C3 later replay. Effects:
     * E1 exactly one current V1 receipt, E2 both starters converge, E3
     * replay is a no-op. Rules: S1 T/F/F=>E1; S2 T/T/F=>E1+E2; S3 F/F/T=>E3.
     * A backend crash cannot expose DDL without its receipt because the shared
     * migration runner commits each migration and ledger row in one backend
     * transaction; this test exercises the competing-start boundary around it. */
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("concurrent-migration.db");
    let path = path.to_string_lossy().to_string();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let starters = (0..2)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                SqliteManagedSessionRepository::open(&path)
            })
        })
        .collect::<Vec<_>>();
    for starter in starters {
        starter.join().unwrap().expect("S2 converges");
    }
    SqliteManagedSessionRepository::open(&path).expect("S3 replay");
    let conn = Connection::open(&path).unwrap();
    let ledger_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM managed_schema_migrations WHERE bundle_id = ?1",
            params!["awaken.managed_session"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ledger_count, 1, "S1/E1 and S3/E3");
    let quarantine_exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params!["managed_session_quarantine"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(quarantine_exists, 1, "S1/E1");
}

pub(crate) fn sample(id: &str) -> PersistedSession {
    let mut metadata = BTreeMap::new();
    metadata.insert("team".to_string(), "research".to_string());
    PersistedSession {
        session_id: id.to_string(),
        revision: Default::default(),
        baseline: SessionBaselineState::Frozen(SessionBaseline::compile(
            awaken_session_contract::SessionBaselineInputs {
                environment: EnvironmentSnapshot {
                    environment_id: "env_local".into(),
                    revision: awaken_session_contract::EnvironmentRevision(1),
                    self_hosted: false,
                    config_fingerprint: EnvironmentFingerprint("env-fingerprint".into()),
                    sandbox: Default::default(),
                    sandbox_provisioning: Default::default(),
                    idle_retention: Default::default(),
                    packages: Default::default(),
                    prepared_image: None,
                    network: SessionNetworkPolicy::Unrestricted,
                    credential_realization: CredentialRealizationProfile {
                        inference_holder: PlaintextHolder::new(
                            PlaintextBoundary::Worker,
                            "awaken.worker",
                        ),
                        mcp_holder: PlaintextHolder::new(
                            PlaintextBoundary::Worker,
                            "awaken.worker",
                        ),
                        resource_holder: PlaintextHolder::new(
                            PlaintextBoundary::Worker,
                            "awaken.worker",
                        ),
                    },
                },
                runtime_placement: awaken_session_contract::SessionRuntimePlacement::Local,
                mcp_authoring: SessionMcpAuthoringContext::default(),
                agent_id: "coder".into(),
                agent_revision: None,
                model_override: None,
                toolsets: Vec::new(),
                model: "kimi-k2".into(),
                runtime: None,
                delegate_ids: Vec::new(),
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: Vec::new(),
                transcript_prefix: None,
            },
        )),
        title: Some("My session".to_string()),
        metadata,
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
        mcp: SessionMcpAttachmentSet::from_initial(
            vec![McpAttachmentDraft {
                name: "calc".into(),
                target: McpTarget::parse_http("https://x").unwrap(),
                credential: None,
                prompts_as_skills: false,
                origin: McpAttachmentOrigin::Session,
            }],
            None,
        )
        .unwrap(),
        resources: awaken_session_contract::SessionResourceState::from_active(
            serde_json::from_value(serde_json::json!({
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

pub(crate) async fn create_fixture<R: ManagedSessionRepository>(
    repo: &R,
    owner: &str,
    mut session: PersistedSession,
    facts: Vec<ManagedLifecycleFact>,
) -> PersistedSession {
    session.revision = SessionRevision(0);
    let payload = SessionMutationPayload::Replace(session.clone());
    let payload_hash = payload.stable_hash();
    session.revision = repo
        .create(
            owner,
            session.clone(),
            IdempotencyRecord {
                key: format!("test:create:{}:{payload_hash}", session.session_id),
                payload_hash,
            },
            facts,
        )
        .await
        .expect("create Session fixture");
    session
}

async fn replace_fixture<R: ManagedSessionRepository>(
    repo: &R,
    owner: &str,
    mut session: PersistedSession,
    key: &str,
    facts: Vec<ManagedLifecycleFact>,
) -> PersistedSession {
    session.revision = repo.get(&session.session_id).await.unwrap().revision;
    let payload = SessionMutationPayload::Replace(session.clone());
    let payload_hash = payload.stable_hash();
    let result = repo
        .commit_mutation(
            owner,
            SessionMutation {
                expected_revision: session.revision,
                idempotency: IdempotencyRecord {
                    key: key.into(),
                    payload_hash,
                },
                payload,
                lifecycle_facts: facts,
            },
        )
        .await
        .expect("replace Session fixture");
    session.revision = match result {
        SessionMutationResult::Applied { new_revision }
        | SessionMutationResult::Replayed { new_revision } => new_revision,
        other => panic!("replace Session fixture failed: {other:?}"),
    };
    session
}

async fn delete_fixture<R: ManagedSessionRepository>(
    repo: &R,
    owner: &str,
    session_id: &str,
    fact: ManagedLifecycleFact,
) {
    let current = repo.get(session_id).await.unwrap();
    let payload = SessionMutationPayload::Delete(awaken_session_contract::SessionTombstone {
        session_id: session_id.into(),
        deleted_revision: SessionRevision(current.revision.0 + 1),
        deleted_at: fact.timestamp.to_string(),
    });
    let payload_hash = payload.stable_hash();
    assert!(matches!(
        repo.commit_mutation(
            owner,
            SessionMutation {
                expected_revision: current.revision,
                idempotency: IdempotencyRecord {
                    key: format!("test:delete:{session_id}:{payload_hash}"),
                    payload_hash,
                },
                payload,
                lifecycle_facts: vec![fact],
            },
        )
        .await
        .unwrap(),
        SessionMutationResult::Applied { .. }
    ));
}

fn make_deletable(session: &mut PersistedSession) {
    let session_id = session.session_id.clone();
    session.request_delete();
    session
        .terminal_cleanup
        .freeze_targets(&session_id, [], 0, 0)
        .unwrap();
    let command = session
        .terminal_cleanup
        .command_for(&session_id, &session_id)
        .unwrap();
    let receipt = awaken_session_contract::SessionCleanupCompletion::new(&command, Vec::new())
        .verify(&command)
        .unwrap();
    session
        .terminal_cleanup
        .complete(&session_id, &[receipt])
        .unwrap();
}

/// Root-mutation cause graph shared by every durable backend:
/// C1=idempotency key exists, C2=payload hash matches, C3=root revision
/// matches, C4=aggregate was tombstoned. Key/hash resolution precedes CAS,
/// so response-loss replay remains deterministic after delete.
///
/// | Rule | C1 | C2 | C3 | C4 | effect |
/// |---|---|---|---|---|---|
/// | R1 | F | - | T | F | apply |
/// | R2 | T | T | - | F/T | replay |
/// | R3 | T | F | - | F/T | idempotency mismatch |
/// | R4 | F | - | F | F | revision conflict |
/// | R5 | F | - | - | T | tombstone conflict |
async fn root_mutation_decision_table<R: ManagedSessionRepository>(repo: &R, id: &str) {
    let created = create_fixture(repo, "ws_a", sample(id), Vec::new()).await;
    let mut replacement = created.clone();
    replacement.title = Some("winner".into());
    make_deletable(&mut replacement);
    let replace_payload = SessionMutationPayload::Replace(replacement);
    let replace_hash = replace_payload.stable_hash();
    let replace = || SessionMutation {
        expected_revision: created.revision,
        idempotency: IdempotencyRecord {
            key: format!("decision:{id}:replace"),
            payload_hash: replace_hash.clone(),
        },
        payload: replace_payload.clone(),
        lifecycle_facts: Vec::new(),
    };
    assert!(
        matches!(
            repo.commit_mutation("ws_a", replace()).await.unwrap(),
            SessionMutationResult::Applied {
                new_revision: SessionRevision(2)
            }
        ),
        "R1"
    );
    assert!(
        matches!(
            repo.commit_mutation("ws_a", replace()).await.unwrap(),
            SessionMutationResult::Replayed {
                new_revision: SessionRevision(2)
            }
        ),
        "R2"
    );

    let mut mismatched = replace();
    mismatched.idempotency.payload_hash = "another-hash".into();
    assert_eq!(
        repo.commit_mutation("ws_a", mismatched).await.unwrap(),
        SessionMutationResult::IdempotencyMismatch,
        "R3"
    );
    let mut stale = replace();
    stale.idempotency.key = format!("decision:{id}:stale");
    assert_eq!(
        repo.commit_mutation("ws_a", stale).await.unwrap(),
        SessionMutationResult::Conflict {
            current_revision: SessionRevision(2)
        },
        "R4"
    );

    let delete_payload =
        SessionMutationPayload::Delete(awaken_session_contract::SessionTombstone {
            session_id: id.into(),
            deleted_revision: SessionRevision(3),
            deleted_at: "3".into(),
        });
    let delete_hash = delete_payload.stable_hash();
    let delete = || SessionMutation {
        expected_revision: SessionRevision(2),
        idempotency: IdempotencyRecord {
            key: format!("decision:{id}:delete"),
            payload_hash: delete_hash.clone(),
        },
        payload: delete_payload.clone(),
        lifecycle_facts: Vec::new(),
    };
    assert!(
        matches!(
            repo.commit_mutation("ws_a", delete()).await.unwrap(),
            SessionMutationResult::Applied {
                new_revision: SessionRevision(3)
            }
        ),
        "R1 delete"
    );
    assert!(
        matches!(
            repo.commit_mutation("ws_a", delete()).await.unwrap(),
            SessionMutationResult::Replayed {
                new_revision: SessionRevision(3)
            }
        ),
        "R2 tombstone replay"
    );
    let mut after_delete = replace();
    after_delete.expected_revision = SessionRevision(3);
    after_delete.idempotency.key = format!("decision:{id}:after-delete");
    if let SessionMutationPayload::Replace(session) = &mut after_delete.payload {
        session.revision = SessionRevision(3);
    }
    after_delete.idempotency.payload_hash = after_delete.payload.stable_hash();
    assert_eq!(
        repo.commit_mutation("ws_a", after_delete).await.unwrap(),
        SessionMutationResult::Conflict {
            current_revision: SessionRevision(3)
        },
        "R5"
    );
}

#[tokio::test]
async fn negative_tombstone_revision_is_rejected_at_the_schema_boundary() {
    let repo = SqliteManagedSessionRepository::open_in_memory().unwrap();
    let session_id = "negative-tombstone";
    let error = repo
        .conn
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO managed_session_tombstone
                    (session_id, scope_id, deleted_revision, deleted_at)
                 VALUES (?1, 'ws_a', -1, 'now')",
            params![session_id],
        )
        .expect_err("negative revision must never enter the repository");
    assert!(error.to_string().contains("deleted_revision > 0"));
}

fn extraction(id: &str, key: &str) -> awaken_ext_memory::MemoryExtractionIntent {
    awaken_ext_memory::MemoryExtractionIntent::new_range(
        id,
        key,
        "ws-a",
        "sesn-1",
        "terminal-1",
        "memory-1",
        1,
        1,
        1,
        Vec::new(),
        {
            let mut extractor = awaken_ext_memory::MemoryExtractorSnapshot::host_executor(
                "memory-agent",
                "host",
                "model-1",
                "host",
            );
            extractor.agent.resolved_spec.instructions = "extract durable facts".into();
            extractor.agent.recompute_fingerprint().unwrap();
            extractor
        },
    )
    .unwrap()
}

fn snapshot_extraction(
    namespace: &str,
    physical_session: &str,
    logical_thread: &str,
    terminal: &str,
    end_seq: u64,
) -> awaken_ext_memory::MemoryExtractionIntent {
    let messages = (0..end_seq)
        .map(|sequence| {
            awaken_agent_contract::agent::message::Message::text(
                awaken_agent_contract::agent::message::Id(format!(
                    "{namespace}-{logical_thread}-{sequence}"
                )),
                awaken_agent_contract::agent::message::Role::User,
                format!("fact {sequence}"),
            )
        })
        .collect();
    awaken_ext_memory::MemoryExtractionIntent::new_snapshot(
        format!("{namespace}-{logical_thread}-{terminal}"),
        format!("{namespace}:{logical_thread}:{terminal}"),
        "ws-a",
        physical_session,
        terminal,
        "memory-1",
        1,
        awaken_agent_contract::thread::read::transcript::TranscriptSnapshotRef {
            thread_id: awaken_agent_contract::agent::thread::Id(logical_thread.into()),
            view: awaken_agent_contract::thread::read::transcript::TranscriptView::RawCommitted,
            version: end_seq,
            end_seq,
        },
        vec![awaken_agent_contract::thread::read::transcript::TranscriptRange::new(0, end_seq)],
        messages,
        extraction("snapshot-extractor", "snapshot-extractor").extractor,
    )
    .unwrap()
}

async fn put_snapshot_cursor_fixture(
    repository: &dyn awaken_ext_memory::MemoryExtractionRepository,
    namespace: &str,
) {
    use awaken_ext_memory::PutMemoryExtractionOutcome;

    for intent in [
        snapshot_extraction(namespace, "parent", "child-a", "terminal-1", 1),
        snapshot_extraction(namespace, "parent", "child-a", "terminal-2", 3),
        snapshot_extraction(namespace, "parent", "child-b", "terminal-1", 2),
    ] {
        assert_eq!(
            repository.put_extraction_if_absent(intent).await.unwrap(),
            PutMemoryExtractionOutcome::Inserted
        );
    }
}

async fn assert_snapshot_cursor_fixture(
    repository: &dyn awaken_ext_memory::MemoryExtractionRepository,
) {
    // C/E/K/D backend-conformance design. Causes: C1 two snapshot-backed
    // terminals share logical child A; C2 sibling child B shares their physical
    // parent Session; C3 storage is SQLite-after-reopen/Postgres. Effects: E1 A
    // advances to its greatest committed end (3); E2 B remains independently at
    // 2; E3 the physical parent does not acquire either logical cursor.
    // Constraints: K1 `TranscriptSnapshotRef.thread_id` is the only logical
    // identity for new intents; K2 `session_id` remains recovery affinity; K3
    // pending intents count. Decision table: D1 C1=>E1; D2 C1+C2=>E1+E2+E3;
    // D3 D2 across each C3 backend=>identical results.
    assert_eq!(
        repository.extraction_cursor("child-a").await.unwrap(),
        3,
        "D3/E1"
    );
    assert_eq!(
        repository.extraction_cursor("child-b").await.unwrap(),
        2,
        "D3/E2"
    );
    assert_eq!(
        repository.extraction_cursor("parent").await.unwrap(),
        0,
        "D3/E3"
    );
}

#[tokio::test]
async fn extraction_intent_and_claim_survive_sqlite_reopen() {
    use awaken_ext_memory::{
        MemoryExtractionRepository, MemoryExtractionStatus, PutMemoryExtractionOutcome,
    };

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sessions.db");
    let path = path.to_string_lossy().to_string();
    {
        let repo = SqliteManagedSessionRepository::open(&path).unwrap();
        let initial = extraction("extract-1", "terminal-1");
        assert_eq!(
            repo.put_extraction_if_absent(initial.clone())
                .await
                .unwrap(),
            PutMemoryExtractionOutcome::Inserted
        );
        assert_eq!(
            repo.put_extraction_if_absent(initial).await.unwrap(),
            PutMemoryExtractionOutcome::Existing
        );
    }

    let repo = SqliteManagedSessionRepository::open(&path).unwrap();
    let mut claimed = repo.get_extraction("extract-1").await.unwrap().unwrap();
    let expected_revision = claimed.revision;
    claimed.claim("worker-a", 100, 50).unwrap();
    repo.compare_and_swap_extraction(expected_revision, claimed.clone())
        .await
        .unwrap();
    drop(repo);

    let reopened = SqliteManagedSessionRepository::open(&path).unwrap();
    let recovered = reopened.recoverable_extractions(10).await.unwrap();
    assert_eq!(reopened.extraction_cursor("sesn-1").await.unwrap(), 1);
    put_snapshot_cursor_fixture(&reopened, "sqlite-snapshot").await;
    drop(reopened);
    let reopened = SqliteManagedSessionRepository::open(&path).unwrap();
    assert_snapshot_cursor_fixture(&reopened).await;
    assert_eq!(recovered, vec![claimed]);
    assert_eq!(recovered[0].status, MemoryExtractionStatus::Claimed);
    assert!(matches!(
        reopened
            .compare_and_swap_extraction(0, recovered[0].clone())
            .await,
        Err(awaken_ext_memory::MemoryExtractionError::RevisionConflict(
            _
        ))
    ));
}

#[tokio::test]
async fn extraction_revision_overflow_is_rejected_before_storage() {
    use awaken_ext_memory::{MemoryExtractionError, MemoryExtractionRepository};

    let repo = SqliteManagedSessionRepository::open_in_memory().unwrap();
    let mut intent = extraction("extract-overflow", "terminal-overflow");
    intent.revision = u64::MAX;

    assert!(matches!(
        repo.compare_and_swap_extraction(u64::MAX, intent).await,
        Err(MemoryExtractionError::RevisionConflict(id)) if id == "extract-overflow"
    ));
}

#[tokio::test]
async fn lifecycle_fact_survives_the_commit_to_notification_crash_window() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sessions-outbox.db");
    let path = path.to_string_lossy().to_string();
    {
        let repo = SqliteManagedSessionRepository::open(&path).unwrap();
        create_fixture(
            &repo,
            "ws_a",
            sample("sesn_tx"),
            vec![fact(
                "session:sesn_tx:created",
                "sesn_tx",
                "session.status_idled",
            )],
        )
        .await;
        // Simulated hard crash: the lifecycle sink is deliberately never called.
    }

    let reopened = SqliteManagedSessionRepository::open(&path).unwrap();
    assert_eq!(reopened.owner("sesn_tx").await.as_deref(), Ok("ws_a"));
    let pending = reopened.pending_lifecycle().await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, "session:sesn_tx:created");

    reopened.complete_lifecycle(&pending[0].id).await.unwrap();
    reopened.complete_lifecycle(&pending[0].id).await.unwrap();
    assert!(reopened.pending_lifecycle().await.unwrap().is_empty());
}

#[tokio::test]
async fn environment_state_survives_sqlite_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sessions-binding.db");
    let path = path.to_string_lossy().to_string();
    {
        let repo = SqliteManagedSessionRepository::open(&path).unwrap();
        create_fixture(&repo, "ws_a", sample("sesn_bound"), Vec::new()).await;
        let mut bound = repo.get("sesn_bound").await.unwrap();
        bound.environment.set_resident("opaque-binding");
        replace_fixture(&repo, "ws_a", bound, "test:bind", Vec::new()).await;
    }
    let reopened = SqliteManagedSessionRepository::open(&path).unwrap();
    assert_eq!(
        reopened
            .get("sesn_bound")
            .await
            .unwrap()
            .environment
            .binding()
            .map(str::to_owned),
        Some("opaque-binding".to_string())
    );
    assert_eq!(reopened.owner("sesn_bound").await.as_deref(), Ok("ws_a"));
}

#[tokio::test]
async fn terminal_cleanup_intent_and_receipt_survive_sqlite_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sessions-terminal-cleanup.db");
    let path = path.to_string_lossy().to_string();
    {
        let repo = SqliteManagedSessionRepository::open(&path).unwrap();
        create_fixture(&repo, "ws_a", sample("sesn_cleanup"), Vec::new()).await;
        let mut requested = repo.get("sesn_cleanup").await.unwrap();
        requested.terminal_cleanup.request("sesn_cleanup");
        requested
            .terminal_cleanup
            .freeze_targets("sesn_cleanup", ["child-cleanup".to_string()], 3, 0)
            .unwrap();
        requested.environment.set_resident("opaque-binding");
        replace_fixture(&repo, "ws_a", requested, "test:cleanup:request", Vec::new()).await;
    }
    {
        let repo = SqliteManagedSessionRepository::open(&path).unwrap();
        let mut recovered = repo.get("sesn_cleanup").await.unwrap();
        assert!(recovered.terminal_cleanup.is_requested());
        let intent = recovered
            .terminal_cleanup
            .command_for("sesn_cleanup", "sesn_cleanup")
            .unwrap();
        let child_intent = recovered
            .terminal_cleanup
            .command_for("sesn_cleanup", "child-cleanup")
            .expect("child cleanup intent survives restart");
        let receipt = awaken_session_contract::SessionCleanupCompletion::new(&intent, Vec::new());
        let child_receipt =
            awaken_session_contract::SessionCleanupCompletion::new(&child_intent, Vec::new());
        let receipt = receipt.verify(&intent).unwrap();
        let child_receipt = child_receipt.verify(&child_intent).unwrap();
        recovered
            .terminal_cleanup
            .complete("sesn_cleanup", &[receipt, child_receipt])
            .unwrap();
        recovered.environment = awaken_session_contract::SessionEnvironmentState::Unmaterialized;
        replace_fixture(
            &repo,
            "ws_a",
            recovered,
            "test:cleanup:complete",
            Vec::new(),
        )
        .await;
    }
    let reopened = SqliteManagedSessionRepository::open(&path).unwrap();
    let completed = reopened.get("sesn_cleanup").await.unwrap();
    assert!(completed.terminal_cleanup.is_completed());
    assert!(matches!(
        completed.environment,
        awaken_session_contract::SessionEnvironmentState::Unmaterialized
    ));
}

#[tokio::test]
async fn terminal_state_and_its_fact_share_one_repository_commit() {
    let repo = SqliteManagedSessionRepository::open_in_memory().unwrap();
    create_fixture(
        &repo,
        "ws_a",
        sample("sesn_terminal"),
        vec![fact("created", "sesn_terminal", "session.status_idled")],
    )
    .await;
    repo.complete_lifecycle("created").await.unwrap();

    let mut terminal = repo.get("sesn_terminal").await.unwrap();
    terminal.archive("2026-01-01T00:00:00Z").unwrap();
    replace_fixture(
        &repo,
        "ws_a",
        terminal,
        "test:terminal",
        vec![fact(
            "terminated",
            "sesn_terminal",
            "session.status_terminated",
        )],
    )
    .await;
    let archived = repo.get("sesn_terminal").await.unwrap();
    assert_eq!(archived.execution, SessionExecutionState::Terminated);
    assert_eq!(archived.archived_at(), Some("2026-01-01T00:00:00Z"));
    assert_eq!(repo.pending_lifecycle().await.unwrap()[0].id, "terminated");

    repo.complete_lifecycle("terminated").await.unwrap();
    let mut deleting = repo.get("sesn_terminal").await.unwrap();
    make_deletable(&mut deleting);
    replace_fixture(
        &repo,
        "ws_a",
        deleting,
        "test:delete-cleanup-complete",
        Vec::new(),
    )
    .await;
    delete_fixture(
        &repo,
        "ws_a",
        "sesn_terminal",
        fact("deleted", "sesn_terminal", "session.deleted"),
    )
    .await;
    assert_eq!(
        repo.get("sesn_terminal").await,
        Err(SessionRepositoryError::NotFound)
    );
    assert_eq!(repo.pending_lifecycle().await.unwrap()[0].id, "deleted");
}

#[tokio::test]
async fn sqlite_root_mutation_decision_table_is_atomic() {
    let repo = SqliteManagedSessionRepository::open_in_memory().unwrap();
    root_mutation_decision_table(&repo, "sesn_sqlite_decisions").await;
}

#[tokio::test]
async fn round_trips_and_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sessions.db");
    let path = path.to_string_lossy().to_string();

    // First process: create + persist, then drop the repo (simulated exit).
    {
        let repo = SqliteManagedSessionRepository::open(&path).unwrap();
        let expected = create_fixture(&repo, "default", sample("sesn_1"), Vec::new()).await;
        assert_eq!(repo.get("sesn_1").await, Ok(expected));
    }
    // Second process: a fresh repo over the same file restores the row.
    let reopened = SqliteManagedSessionRepository::open(&path).unwrap();
    let mut expected = sample("sesn_1");
    expected.revision = SessionRevision(1);
    assert_eq!(reopened.get("sesn_1").await, Ok(expected));
    assert_eq!(
        reopened.get("sesn_missing").await,
        Err(SessionRepositoryError::NotFound)
    );
}

#[tokio::test]
async fn malformed_canonical_aggregate_fails_closed() {
    // The complete aggregate is the only readable truth. The current baseline
    // makes a missing aggregate impossible; malformed bytes still fail closed.
    let repo = SqliteManagedSessionRepository::open_in_memory().unwrap();
    repo.conn
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO managed_session
                 (session_id, scope_id, revision, aggregate_json)
                 VALUES (?1, 'default', 1, '{')",
            params!["malformed"],
        )
        .unwrap();

    assert!(matches!(
        repo.get("malformed").await,
        Err(SessionRepositoryError::Corrupt(_))
    ));
}

#[tokio::test]
async fn save_is_an_idempotent_upsert() {
    let repo = SqliteManagedSessionRepository::open_in_memory().unwrap();
    create_fixture(&repo, "default", sample("sesn_1"), Vec::new()).await;
    let mut updated = sample("sesn_1");
    updated.title = Some("Renamed".to_string());
    updated = replace_fixture(&repo, "default", updated, "test:rename", Vec::new()).await;
    assert_eq!(repo.get("sesn_1").await, Ok(updated), "re-save overwrites");
}

#[tokio::test]
async fn owner_scope_is_recorded_and_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sessions.db");
    let path = path.to_string_lossy().to_string();
    {
        let repo = SqliteManagedSessionRepository::open(&path).unwrap();
        create_fixture(&repo, "ws_a", sample("sesn_1"), Vec::new()).await;
        assert_eq!(repo.owner("sesn_1").await, Ok("ws_a".to_string()));
    }
    // After a restart the owner is still readable — the cross-process fence input
    // for the edge ownership guard (ADR-0051).
    let reopened = SqliteManagedSessionRepository::open(&path).unwrap();
    assert_eq!(reopened.owner("sesn_1").await, Ok("ws_a".to_string()));
    // A row saved but never owner-stamped defaults to the seeded scope.
    create_fixture(&reopened, "default", sample("sesn_2"), Vec::new()).await;
    assert_eq!(reopened.owner("sesn_2").await, Ok("default".to_string()));
    // An unknown session has no owner.
    assert_eq!(
        reopened.owner("sesn_missing").await,
        Err(SessionRepositoryError::NotFound)
    );
}

/// Persistence authority causal graph: the canonical aggregate is the only
/// selected authority and corruption never falls back to retired split columns.
///
/// | Rule | aggregate present | payload valid | Effect |
/// |---|---|---|---|
/// | P1 | T | T | aggregate |
/// | P2 | T | F | fail closed |
/// | P3 | F | - | fail closed |
#[tokio::test]
async fn corrupt_canonical_aggregate_fails_closed() {
    let repo = SqliteManagedSessionRepository::open_in_memory().unwrap();
    create_fixture(&repo, "default", sample("sesn_1"), Vec::new()).await;
    {
        let conn = repo.conn.lock().unwrap();
        conn.execute(
            "UPDATE managed_session \
                 SET aggregate_json = ?2 WHERE session_id = ?1",
            params!["sesn_1", "{not valid json"],
        )
        .unwrap();
    }
    let error = repo
        .get("sesn_1")
        .await
        .expect_err("corrupt canonical authority must be distinguishable from absence");
    assert!(matches!(error, SessionRepositoryError::Corrupt(_)));
}

#[tokio::test]
async fn legacy_columns_are_not_a_parallel_authority() {
    let repo = SqliteManagedSessionRepository::open_in_memory().unwrap();
    let conn = repo.conn.lock().unwrap();
    let mut statement = conn.prepare("PRAGMA table_info(managed_session)").unwrap();
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        columns,
        vec!["session_id", "scope_id", "revision", "aggregate_json"]
    );
}

#[tokio::test]
async fn noncanonical_aggregate_fields_fail_closed() {
    // The strict aggregate grammar rejects a retired protocol projection rather
    // than retaining a second tool-configuration decoder in persistence.
    let repo = SqliteManagedSessionRepository::open_in_memory().unwrap();
    create_fixture(&repo, "default", sample("legacy-tools"), Vec::new()).await;
    let mut legacy = serde_json::to_value(sample("legacy-tools")).unwrap();
    legacy.as_object_mut().unwrap().remove("tools");
    legacy.as_object_mut().unwrap().insert(
        "agent_tools".into(),
        serde_json::json!([
            {"type": "agent_toolset_20260401", "configs": []},
            {
                "type": "custom",
                "name": "client_lookup",
                "description": "Client lookup",
                "input_schema": {"type": "object"}
            }
        ]),
    );
    repo.conn
        .lock()
        .unwrap()
        .execute(
            "UPDATE managed_session SET aggregate_json = ?2 WHERE session_id = ?1",
            params!["legacy-tools", serde_json::to_string(&legacy).unwrap()],
        )
        .unwrap();

    assert!(matches!(
        repo.get("legacy-tools").await,
        Err(SessionRepositoryError::Corrupt(_))
    ));
}

/// Live Postgres round-trip, isolated in its own schema. Skips when no Postgres
/// is reachable (`AWAKEN_TEST_DATABASE_URL`), proving the shared portable bundle
/// and the same behavior on the network backend.
#[tokio::test]
async fn postgres_round_trips_and_upserts() {
    use awaken_ext_memory::{MemoryExtractionRepository, PutMemoryExtractionOutcome};
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
        .execute("DROP SCHEMA IF EXISTS t_managed_session CASCADE")
        .await;
    admin
        .execute("CREATE SCHEMA t_managed_session")
        .await
        .expect("create schema");
    admin.close().await;
    let pool = PgPoolOptions::new()
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                conn.execute("SET search_path = t_managed_session").await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .expect("schema pool");
    let repo = PostgresManagedSessionRepository::with_pool(pool)
        .await
        .expect("store");

    root_mutation_decision_table(&repo, "sesn_pg_decisions").await;

    let created = create_fixture(&repo, "default", sample("sesn_1"), Vec::new()).await;
    assert_eq!(repo.get("sesn_1").await, Ok(created));
    assert_eq!(
        repo.get("sesn_missing").await,
        Err(SessionRepositoryError::NotFound)
    );

    let mut updated = sample("sesn_1");
    updated.title = None; // exercises the nullable title column
    updated = replace_fixture(&repo, "default", updated, "test:pg-update", Vec::new()).await;
    assert_eq!(repo.get("sesn_1").await, Ok(updated));

    create_fixture(
        &repo,
        "ws_a",
        sample("sesn_pg_tx"),
        vec![fact(
            "session:sesn_pg_tx:created",
            "sesn_pg_tx",
            "session.status_idled",
        )],
    )
    .await;
    assert_eq!(repo.owner("sesn_pg_tx").await.as_deref(), Ok("ws_a"));
    assert_eq!(
        repo.pending_lifecycle().await.unwrap()[0].id,
        "session:sesn_pg_tx:created"
    );
    repo.complete_lifecycle("session:sesn_pg_tx:created")
        .await
        .unwrap();
    let mut terminal = repo.get("sesn_pg_tx").await.unwrap();
    terminal.archive("2026-07-19T00:00:00Z").unwrap();
    replace_fixture(
        &repo,
        "ws_a",
        terminal,
        "test:pg-terminal",
        vec![fact(
            "session:sesn_pg_tx:terminated",
            "sesn_pg_tx",
            "session.status_terminated",
        )],
    )
    .await;
    assert_eq!(
        repo.get("sesn_pg_tx").await.unwrap().execution,
        SessionExecutionState::Terminated
    );
    assert_eq!(
        repo.pending_lifecycle().await.unwrap()[0].id,
        "session:sesn_pg_tx:terminated"
    );

    let initial = extraction("extract-pg", "terminal-pg");
    assert_eq!(
        repo.put_extraction_if_absent(initial.clone())
            .await
            .unwrap(),
        PutMemoryExtractionOutcome::Inserted
    );
    let mut claimed = initial;
    claimed.claim("worker-pg", 100, 50).unwrap();
    repo.compare_and_swap_extraction(0, claimed.clone())
        .await
        .unwrap();
    assert_eq!(
        repo.recoverable_extractions(10).await.unwrap(),
        vec![claimed]
    );
    assert_eq!(repo.extraction_cursor("sesn-1").await.unwrap(), 1);
    put_snapshot_cursor_fixture(&repo, "postgres-snapshot").await;
    assert_snapshot_cursor_fixture(&repo).await;

    /* Postgres parity for the recovery isolation decision table above.
     * Causes: P1 one decodable pending Session; P2 one corrupt aggregate.
     * Effects: Q1 P1 remains returned; Q2 P2 is excluded and durably
     * quarantined; Q3 replay preserves the same isolation record. */
    create_fixture(&repo, "workspace", sample("sesn_pg_healthy"), Vec::new()).await;
    create_fixture(&repo, "workspace", sample("sesn_pg_corrupt"), Vec::new()).await;
    sqlx::query("UPDATE managed_session SET aggregate_json = $1 WHERE session_id = $2")
        .bind("{")
        .bind("sesn_pg_corrupt")
        .execute(&repo.pool)
        .await
        .unwrap();
    let scan = repo.reconcilable_sessions().await.unwrap();
    assert!(
        scan.sessions
            .iter()
            .any(|row| row.session.session_id == "sesn_pg_healthy"),
        "Q1"
    );
    assert!(
        scan.quarantined
            .iter()
            .any(|row| row.session_id == "sesn_pg_corrupt"),
        "Q2"
    );
    let replay = repo.reconcilable_sessions().await.unwrap();
    assert_eq!(replay.quarantined, scan.quarantined, "Q3");
}

/// Postgres parity for the ADR-0051 owner `scope_id` — the same atomic
/// aggregate `create` / `commit_mutation` ownership assertions the SQLite test
/// test has, which the pg test previously OMITTED. A second pool over the same schema
/// stands in for a restart (the cross-process fence input the edge guard reads). Skips
/// when no Postgres is reachable (`AWAKEN_TEST_DATABASE_URL`), isolated in its schema.
#[tokio::test]
async fn postgres_owner_scope_is_recorded_and_survives_a_reopen() {
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
        .execute("DROP SCHEMA IF EXISTS t_managed_session_owner CASCADE")
        .await;
    admin
        .execute("CREATE SCHEMA t_managed_session_owner")
        .await
        .expect("create schema");
    admin.close().await;

    let pool = || {
        let url = url.clone();
        async move {
            PgPoolOptions::new()
                .after_connect(|conn, _meta| {
                    Box::pin(async move {
                        conn.execute("SET search_path = t_managed_session_owner")
                            .await?;
                        Ok(())
                    })
                })
                .connect(&url)
                .await
                .expect("schema pool")
        }
    };

    // First "process": save the row and owner atomically.
    let repo = PostgresManagedSessionRepository::with_pool(pool().await)
        .await
        .expect("store");
    create_fixture(&repo, "ws_a", sample("sesn_1"), Vec::new()).await;
    assert_eq!(repo.owner("sesn_1").await, Ok("ws_a".to_string()));

    // Second "process": a fresh pool over the same schema still reads the owner.
    let reopened = PostgresManagedSessionRepository::with_pool(pool().await)
        .await
        .expect("store");
    assert_eq!(reopened.owner("sesn_1").await, Ok("ws_a".to_string()));
    // A row saved but never owner-stamped defaults to the seeded scope.
    create_fixture(&reopened, "default", sample("sesn_2"), Vec::new()).await;
    assert_eq!(reopened.owner("sesn_2").await, Ok("default".to_string()));
    // An unknown session has no owner.
    assert_eq!(
        reopened.owner("sesn_missing").await,
        Err(SessionRepositoryError::NotFound)
    );
}
