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

/// Session-recovery scheduler-isolation cause/effect graph: C1 one synchronous
/// SQLite owner holds the canonical Session connection; C2 forty recovery scans
/// become runnable on a two-worker Tokio runtime, matching the observed hosted
/// backlog. Effects: E1 connection waiters consume blocking-pool capacity only;
/// E2 an authority/lease timer fires before the owner releases at 250 ms; E3 all
/// forty scans complete after release without losing or inventing Session rows.
/// Decision rule S1=C1+C2=>E1+E2+E3. The 40-way fan-out is deliberate: the
/// generic two-waiter boundary test proves mechanism, while this rule freezes
/// the production-shaped recovery pressure that starved Control and heartbeat.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_backlog_cannot_starve_session_authority_timers() {
    let repository = Arc::new(SqliteManagedSessionRepository::open_in_memory().unwrap());
    let held = repository.conn.clone();
    let (held_tx, held_rx) = std::sync::mpsc::sync_channel(1);
    let holder = std::thread::spawn(move || {
        let _guard = held.lock().expect("S1 connection lock");
        held_tx.send(()).expect("S1 announce held connection");
        std::thread::sleep(Duration::from_millis(250));
    });
    held_rx.recv().expect("S1 connection held");

    let started = std::time::Instant::now();
    let timer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        started.elapsed()
    });
    let scans = (0..40)
        .map(|_| {
            let repository = Arc::clone(&repository);
            tokio::spawn(async move { repository.reconcilable_sessions().await })
        })
        .collect::<Vec<_>>();
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    let elapsed = tokio::time::timeout(Duration::from_millis(100), timer)
        .await
        .expect("S1/E1-E2 authority timer remains schedulable")
        .expect("S1 timer task");
    assert!(
        elapsed < Duration::from_millis(100),
        "S1/E2 timer fired after {elapsed:?}; Session scans blocked runtime workers"
    );

    holder.join().expect("S1 release connection");
    for scan in scans {
        let result = scan.await.expect("S1 recovery task").expect("S1/E3 scan");
        assert!(result.sessions.is_empty(), "S1/E3 no invented Sessions");
        assert!(
            result.quarantined.is_empty(),
            "S1/E3 no invented quarantine"
        );
    }
}

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
    assert!(
        postgres
            .count_environment_phase(SessionEnvironmentPhase::Restoring)
            .await
            .is_err(),
        "R1 typed global Environment count"
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
    assert!(
        matches!(
            sqlite
                .count_environment_phase(SessionEnvironmentPhase::Restoring)
                .await,
            Err(SessionRepositoryError::Corrupt(_))
        ),
        "R3 a corrupt canonical root fails the global barrier closed"
    );
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

#[tokio::test]
async fn reconciliation_keyset_pages_cover_more_than_one_fixed_batch_exactly_once() {
    /* Reconciliation page cause/effect decision table. C1 the durable index
     * contains fewer than, exactly, or more than RECOVERY_BATCH_SIZE
     * decode-valid rows; C2 the caller supplies no cursor or the exact cursor
     * returned by page one; C3 every row still needs reconciliation. Effects:
     * E1 each page returns at most the fixed batch size; E2 a full page with a
     * successor returns its exclusive keyset cursor; E3 following that cursor
     * returns every later row exactly once; E4 the terminal page has no cursor.
     * R1 <batch+None=>E1+E4 is covered by the ordinary recovery tests;
     * R2 batch+1+None=>E1+E2; R3 R2 cursor=>E1+E3+E4. Cursor pagination changes
     * no durable row, quarantine rule, queue, or reconciliation predicate. */
    let dir = tempfile::tempdir().unwrap();
    let sqlite = SqliteManagedSessionRepository::open(
        &dir.path().join("recovery-pages.db").to_string_lossy(),
    )
    .unwrap();
    let total = usize::try_from(RECOVERY_BATCH_SIZE).unwrap() + 1;
    let expected = (0..total)
        .map(|index| format!("paged-{index:03}"))
        .collect::<Vec<_>>();
    for session_id in &expected {
        create_fixture(&sqlite, "workspace", sample(session_id), Vec::new()).await;
    }

    let first = sqlite.reconcilable_sessions_page(None).await.unwrap();
    assert_eq!(
        first.sessions.len(),
        usize::try_from(RECOVERY_BATCH_SIZE).unwrap(),
        "R2/E1 fixed first page"
    );
    let cursor = first.next_cursor.clone().expect("R2/E2 next page");
    assert_eq!(cursor.session_id(), expected[total - 2], "R2/E2");

    let second = sqlite
        .reconcilable_sessions_page(Some(&cursor))
        .await
        .unwrap();
    assert_eq!(second.sessions.len(), 1, "R3/E1");
    assert!(second.next_cursor.is_none(), "R3/E4 terminal page");
    let observed = first
        .sessions
        .into_iter()
        .chain(second.sessions)
        .map(|scoped| scoped.session.session_id)
        .collect::<Vec<_>>();
    assert_eq!(observed, expected, "R3/E3 no duplicate or missing row");
}

#[tokio::test]
async fn create_and_mutation_receipts_fail_closed_on_corrupt_identity_state() {
    // Durable-receipt corruption table. C1 create receipt exists without any
    // identity; C2 create receipt points at an undecodable live aggregate; C3 a
    // non-create revision is stored under a create receipt key; C4 both live and
    // tombstone identities exist; C5 a mutation receipt exists without either
    // identity. Effects: every rule returns Corrupt, never
    // replay/not-found/conflict, and no replacement aggregate or external outbox
    // fact is synthesized. These states require backend injection because the
    // repository transaction never creates them.
    let repo = SqliteManagedSessionRepository::open_in_memory().unwrap();
    repo.conn
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO managed_session_idempotency
                (session_id, idempotency_key, payload_hash, committed_revision)
             VALUES (?1, ?2, ?3, ?4)",
            params!["dangling-create", "create", "request", 1_i64],
        )
        .unwrap();
    assert!(matches!(
        repo.replay_create(
            "workspace",
            "dangling-create",
            &IdempotencyRecord {
                key: "create".into(),
                payload_hash: "request".into(),
            },
        )
        .await,
        Err(SessionRepositoryError::Corrupt(_))
    ));

    let damaged_id = "damaged-create";
    let candidate = sample(damaged_id);
    let payload = SessionMutationPayload::Replace(candidate.clone());
    let record = IdempotencyRecord {
        key: "damaged-create".into(),
        payload_hash: payload.stable_hash(),
    };
    repo.create("workspace", candidate, record.clone(), Vec::new())
        .await
        .unwrap();
    repo.conn
        .lock()
        .unwrap()
        .execute(
            "UPDATE managed_session SET aggregate_json = ?1 WHERE session_id = ?2",
            params!["{", damaged_id],
        )
        .unwrap();
    assert!(matches!(
        repo.replay_create("workspace", damaged_id, &record).await,
        Err(SessionRepositoryError::Corrupt(_))
    ));

    let non_create_id = "non-create-receipt";
    let candidate = sample(non_create_id);
    let payload = SessionMutationPayload::Replace(candidate.clone());
    let record = IdempotencyRecord {
        key: "create".into(),
        payload_hash: payload.stable_hash(),
    };
    repo.create("workspace", candidate, record.clone(), Vec::new())
        .await
        .unwrap();
    repo.conn
        .lock()
        .unwrap()
        .execute(
            "UPDATE managed_session_idempotency SET committed_revision = ?1
             WHERE session_id = ?2 AND idempotency_key = ?3",
            params![2_i64, non_create_id, record.key],
        )
        .unwrap();
    assert!(matches!(
        repo.replay_create("workspace", non_create_id, &record)
            .await,
        Err(SessionRepositoryError::Corrupt(_))
    ));

    let dual_identity_id = "dual-create-identity";
    let candidate = sample(dual_identity_id);
    let payload = SessionMutationPayload::Replace(candidate.clone());
    let record = IdempotencyRecord {
        key: "create".into(),
        payload_hash: payload.stable_hash(),
    };
    repo.create("workspace", candidate, record.clone(), Vec::new())
        .await
        .unwrap();
    repo.conn
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO managed_session_tombstone
                (session_id, scope_id, deleted_revision, deleted_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![dual_identity_id, "workspace", 1_i64, "now"],
        )
        .unwrap();
    assert!(matches!(
        repo.replay_create("workspace", dual_identity_id, &record)
            .await,
        Err(SessionRepositoryError::Corrupt(_))
    ));

    let missing = sample("dangling-mutation");
    let payload = SessionMutationPayload::Replace(missing.clone());
    let payload_hash = payload.stable_hash();
    repo.conn
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO managed_session_idempotency
                (session_id, idempotency_key, payload_hash, committed_revision)
             VALUES (?1, ?2, ?3, ?4)",
            params!["dangling-mutation", "mutation", payload_hash, 1_i64],
        )
        .unwrap();
    assert!(matches!(
        repo.commit_mutation(
            "workspace",
            SessionMutation {
                expected_revision: SessionRevision(0),
                idempotency: IdempotencyRecord {
                    key: "mutation".into(),
                    payload_hash: payload.stable_hash(),
                },
                payload,
                lifecycle_facts: Vec::new(),
            },
        )
        .await,
        Err(SessionRepositoryError::Corrupt(_))
    ));
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
     * E1 exactly one V1 and one V2 receipt, E2 both migration and deterministic
     * source-index rebuild converge, E3 replay reapplies no DDL but rebuilds the
     * derived index. Rules: S1 T/F/F=>E1; S2 T/T/F=>E1+E2; S3 F/F/T=>E3.
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
    assert_eq!(ledger_count, 2, "S1/E1 and S3/E3");
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

fn sample_with_credential_source(id: &str, source_id: &str) -> PersistedSession {
    let mut session = sample(id);
    session.mcp.attachments[0].credential =
        Some(awaken_credential_contract::CredentialAccess::new(
            awaken_credential_contract::CredentialRef {
                id: source_id.into(),
                revision: 1,
            },
            awaken_credential_contract::CredentialMaterialSource::ControlPlaneReference,
            awaken_credential_contract::CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            },
            awaken_credential_contract::CredentialExecutionPolicy::self_hosted_provider(),
        ));
    session
}

fn directory_snapshot(directory: &std::path::Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

fn migration_prefix_through(
    source: &awaken_scoped_migration::MigrationBundle,
    tail: i64,
) -> awaken_scoped_migration::MigrationBundle {
    awaken_scoped_migration::MigrationBundle::new(
        source.bundle_id(),
        source
            .migrations()
            .iter()
            .filter(|migration| migration.version() <= tail)
            .cloned()
            .collect(),
    )
    .expect("canonical Session migration prefix")
}

fn materialize_foundation_ledger_generation(
    conn: &Connection,
    ledger_exists: bool,
    meta_exists: bool,
) {
    let ledger = awaken_scoped_migration::LedgerSchema::with_prefix(NS).unwrap();
    let [create_ledger, create_meta, stamp_version] =
        ledger.create_statements(awaken_scoped_migration::Dialect::Sqlite);
    if ledger_exists {
        conn.execute_batch(&create_ledger).unwrap();
    }
    if meta_exists {
        conn.execute_batch(&create_meta).unwrap();
        conn.execute_batch(&stamp_version).unwrap();
    }
}

async fn postgres_test_admin() -> Option<(String, sqlx::postgres::PgPool)> {
    /* Postgres fixture admission decision table. Causes: C1 the caller
     * explicitly configured AWAKEN_TEST_DATABASE_URL; C2 that database is
     * reachable. Effects: E1 return the live pool; E2 allow an ordinary local
     * self-skip; E3 fail the test. Rules: PG1=C1+C2=>E1,
     * PG2=!C1+!C2=>E2, PG3=C1+!C2=>E3. A configured release-gate substrate can
     * therefore never turn a connection regression into a green skip. */
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
    match sqlx::postgres::PgPool::connect(&url).await {
        Ok(pool) => Some((url, pool)),
        Err(error) if configured.is_none() => {
            println!("[skip] no Postgres reachable: {error}");
            None
        }
        Err(error) => panic!("configured AWAKEN_TEST_DATABASE_URL is unreachable: {error}"),
    }
}

fn scoped_postgres_url(base: &str, schema: &str) -> String {
    let separator = if base.contains('?') { '&' } else { '?' };
    format!("{base}{separator}options=-c%20search_path%3D{schema}")
}

#[derive(Debug, PartialEq, Eq)]
struct PostgresSessionSnapshot {
    roots: Vec<(String, String, i64)>,
    reconciliation: Vec<(String, i64)>,
    vaults: Vec<(String, String)>,
    credential_sources: Vec<(String, String)>,
    receipts: Vec<(String, i64, String)>,
}

async fn postgres_session_snapshot(pool: &sqlx::postgres::PgPool) -> PostgresSessionSnapshot {
    PostgresSessionSnapshot {
        roots: sqlx::query_as(
            "SELECT session_id, aggregate_json, revision FROM managed_session ORDER BY session_id",
        )
        .fetch_all(pool)
        .await
        .unwrap(),
        reconciliation: sqlx::query_as(
            "SELECT session_id, observed_revision FROM managed_session_reconciliation_work \
             ORDER BY session_id, observed_revision",
        )
        .fetch_all(pool)
        .await
        .unwrap(),
        vaults: sqlx::query_as(
            "SELECT session_id, vault_id FROM managed_session_vault_reference \
             ORDER BY session_id, vault_id",
        )
        .fetch_all(pool)
        .await
        .unwrap(),
        credential_sources: sqlx::query_as(
            "SELECT session_id, credential_source_id \
             FROM managed_session_credential_source_reference \
             ORDER BY session_id, credential_source_id",
        )
        .fetch_all(pool)
        .await
        .unwrap(),
        receipts: sqlx::query_as(
            "SELECT bundle_id, version, checksum FROM managed_schema_migrations \
             ORDER BY bundle_id, version",
        )
        .fetch_all(pool)
        .await
        .unwrap(),
    }
}

#[test]
fn sqlite_existing_probe_accepts_migratable_history_and_rejects_invalid_storage_read_only() {
    /* Cause/effect graph: C1 an existing file has the current published and
     * converged receipt prefixes, either still present only in an open WAL or
     * checkpointed into the closed main file; C2 it has an exact original
     * V1/V9/V12/V13/V22 or compacted V20 prefix with a legal pending tail; C3
     * it is corrupt SQLite. Effects: E1 C1/C2 are accepted by the read-only
     * probe and the same selected schema opens successfully; E2 C3 fails
     * closed; E3 probing changes no source row, receipt, sidecar, or byte.
     * Decision table: SP1a=C1(open WAL)=>E1+E3,
     * SP1b=C1(closed/checkpointed)=>E1+E3,
     * SP2a..SP2f=C2(each published prefix)=>E1+E3, SP3=C3=>E2+E3. The SP1a
     * fixture proves the physical snapshot includes committed WAL pages rather
     * than using SQLite immutable mode, which would ignore them. Prefix
     * fixtures are sliced from the canonical published bundles; neither the
     * probe nor this test repeats checksums or DDL. */
    let directory = tempfile::tempdir().unwrap();

    let current = directory.path().join("current.db");
    let current_authority = SqliteManagedSessionRepository::open(&current.to_string_lossy())
        .expect("SP1 current authority");
    let current_wal = std::path::PathBuf::from(format!("{}-wal", current.display()));
    assert!(
        std::fs::metadata(&current_wal).unwrap().len() > 0,
        "SP1a fixture retains committed migration pages in WAL"
    );
    let before = directory_snapshot(directory.path());
    SqliteManagedSessionRepository::verify_existing(&current.to_string_lossy())
        .expect("SP1a current WAL prefix is ready");
    assert_eq!(directory_snapshot(directory.path()), before, "SP1a/E3");

    drop(current_authority);
    let before = directory_snapshot(directory.path());
    SqliteManagedSessionRepository::verify_existing(&current.to_string_lossy())
        .expect("SP1b checkpointed current prefix is ready");
    assert_eq!(directory_snapshot(directory.path()), before, "SP1b/E3");

    let original = original_published_session_bundle().unwrap();
    let compacted = compacted_published_session_bundle().unwrap();
    for (rule, name, source, tail) in [
        ("SP2a", "original-v1", &original, 1),
        ("SP2b", "original-v9", &original, 9),
        ("SP2c", "original-v12", &original, 12),
        ("SP2d", "original-v13", &original, 13),
        ("SP2e", "original-v22", &original, 22),
        ("SP2f", "compacted-v20", &compacted, 20),
    ] {
        let prefix = migration_prefix_through(source, tail);
        let legacy = directory.path().join(format!("{name}.db"));
        let conn = Connection::open(&legacy).unwrap();
        awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
            .unwrap()
            .run_bundle(&conn, &prefix)
            .unwrap();
        drop(conn);

        let before = directory_snapshot(directory.path());
        SqliteManagedSessionRepository::verify_existing(&legacy.to_string_lossy())
            .unwrap_or_else(|error| panic!("{rule} probe rejected {name}: {error}"));
        assert_eq!(directory_snapshot(directory.path()), before, "{rule}/E3");

        drop(
            SqliteManagedSessionRepository::open(&legacy.to_string_lossy())
                .unwrap_or_else(|error| panic!("{rule} opener rejected {name}: {error}")),
        );
        drop(
            SqliteManagedSessionRepository::open(&legacy.to_string_lossy())
                .unwrap_or_else(|error| panic!("{rule} converged reopen rejected {name}: {error}")),
        );
        SqliteManagedSessionRepository::verify_existing(&legacy.to_string_lossy())
            .unwrap_or_else(|error| panic!("{rule} converged store rejected: {error}"));
    }

    let corrupt = directory.path().join("corrupt.db");
    std::fs::write(&corrupt, b"SQLite format 3\0truncated").unwrap();
    let before = directory_snapshot(directory.path());
    let error = SqliteManagedSessionRepository::verify_existing(&corrupt.to_string_lossy())
        .expect_err("SP3 corrupt SQLite cannot be ready");
    assert!(
        error.contains("integrity") || error.contains("database disk image is malformed"),
        "SP3/E2: {error}"
    );
    assert_eq!(directory_snapshot(directory.path()), before, "SP3/E3");
}

#[cfg(unix)]
#[test]
fn sqlite_existing_probe_rejects_filesystem_aliases_read_only() {
    /* Alias-admission cause/effect table. Causes: C1 the configured path is a
     * direct single-link regular file; C2 its final component is a symbolic
     * link; C3 either name of its inode has multiple hard links. Effects: E1
     * C1 proceeds to canonical ledger/schema verification (covered by SP1b);
     * E2 C2/C3 fail before SQLite opens the source or its physical snapshot;
     * E3 every source name and byte is unchanged. Rules: AL1=C1=>E1,
     * AL2=C2=>E2+E3, AL3=C3=>E2+E3. The Unix link count is the platform
     * authority; the test declares no migration/schema shape. */
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    let authority = directory.path().join("authority.db");
    drop(SqliteManagedSessionRepository::open(&authority.to_string_lossy()).unwrap());

    let symbolic_alias = directory.path().join("symbolic.db");
    symlink(&authority, &symbolic_alias).unwrap();
    let before = directory_snapshot(directory.path());
    let error = SqliteManagedSessionRepository::verify_existing(&symbolic_alias.to_string_lossy())
        .expect_err("AL2 symbolic alias cannot define the Session authority");
    assert!(error.contains("symbolic link"), "AL2/E2: {error}");
    assert_eq!(directory_snapshot(directory.path()), before, "AL2/E3");

    let hard_alias = directory.path().join("hard.db");
    std::fs::hard_link(&authority, &hard_alias).unwrap();
    let before = directory_snapshot(directory.path());
    let error = SqliteManagedSessionRepository::verify_existing(&hard_alias.to_string_lossy())
        .expect_err("AL3 hard-link alias cannot define the Session authority");
    assert!(error.contains("hard links"), "AL3/E2: {error}");
    assert_eq!(directory_snapshot(directory.path()), before, "AL3/E3");
}

#[test]
fn sqlite_existing_probe_validates_the_foundation_ledger_generation_read_only() {
    /* Cause/effect decision table derived from the ledger-generation graph:
     *
     * | Rule | ledger | meta | meta rows | version | Effect |
     * | LG1  | no     | no   | n/a       | n/a     | MissingLedger |
     * | LG2  | yes    | no   | n/a       | n/a     | IncompleteLedger |
     * | LG3  | no     | yes  | one       | current | IncompleteLedger |
     * | LG4  | yes    | yes  | zero/two  | n/a     | row-count error |
     * | LG5  | yes    | yes  | one       | wrong   | version error |
     *
     * Effects shared by LG1..LG5: fail before receipt/schema admission and
     * leave the database and sidecars byte-for-byte unchanged. Ledger names,
     * presence decisions, creation statements, and current version remain
     * foundation-owned. The fixture materializes an unrelated namespace from
     * the canonical Session bundle and incomplete generations from
     * `LedgerSchema`; this test owns no schema declaration. */
    #[derive(Clone, Copy)]
    enum Damage {
        MissingBoth,
        MissingMeta,
        MissingLedger,
        EmptyMeta,
        DuplicateMeta,
        WrongVersion,
    }

    let directory = tempfile::tempdir().unwrap();
    for (rule, name, damage, expected) in [
        (
            "LG1",
            "missing-both",
            Damage::MissingBoth,
            "no migration ledger",
        ),
        (
            "LG2",
            "missing-meta",
            Damage::MissingMeta,
            "incomplete migration ledger",
        ),
        (
            "LG3",
            "missing-ledger",
            Damage::MissingLedger,
            "incomplete migration ledger",
        ),
        ("LG4a", "empty-meta", Damage::EmptyMeta, "exactly one row"),
        (
            "LG4b",
            "duplicate-meta",
            Damage::DuplicateMeta,
            "exactly one row",
        ),
        (
            "LG5",
            "wrong-version",
            Damage::WrongVersion,
            "stamped version",
        ),
    ] {
        let path = directory.path().join(format!("{name}.db"));
        match damage {
            Damage::MissingBoth => {
                let conn = Connection::open(&path).unwrap();
                awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix("unrelated")
                    .unwrap()
                    .run_bundle(&conn, &session_bundle().unwrap())
                    .unwrap();
            }
            Damage::MissingMeta | Damage::MissingLedger => {
                let conn = Connection::open(&path).unwrap();
                materialize_foundation_ledger_generation(
                    &conn,
                    matches!(damage, Damage::MissingMeta),
                    matches!(damage, Damage::MissingLedger),
                );
            }
            Damage::EmptyMeta | Damage::DuplicateMeta | Damage::WrongVersion => {
                drop(SqliteManagedSessionRepository::open(&path.to_string_lossy()).unwrap());
                let conn = Connection::open(&path).unwrap();
                match damage {
                    Damage::EmptyMeta => {
                        conn.execute("DELETE FROM managed_schema_migrations_meta", [])
                            .unwrap();
                    }
                    Damage::DuplicateMeta => {
                        conn.execute(
                            "INSERT INTO managed_schema_migrations_meta(ledger_version) \
                             SELECT ledger_version FROM managed_schema_migrations_meta",
                            [],
                        )
                        .unwrap();
                    }
                    Damage::WrongVersion => {
                        conn.execute(
                            "UPDATE managed_schema_migrations_meta SET ledger_version = ?1",
                            params![awaken_scoped_migration::LEDGER_VERSION + 1],
                        )
                        .unwrap();
                    }
                    Damage::MissingBoth | Damage::MissingMeta | Damage::MissingLedger => {
                        unreachable!()
                    }
                }
            }
        }
        let before = directory_snapshot(directory.path());
        let error =
            SqliteManagedSessionRepository::verify_existing(&path.to_string_lossy()).unwrap_err();
        assert!(error.contains(expected), "{rule}: {error}");
        assert_eq!(
            directory_snapshot(directory.path()),
            before,
            "{rule} read-only"
        );
    }
}

#[test]
fn sqlite_existing_probe_rejects_receipt_schema_disagreement_read_only() {
    /* Cause/effect graph: C1 exact original receipts through V13 are present;
     * C2 the canonical V13 migration body was not applied. Effects: E1 ledger
     * plan alone would accept the contiguous prefix; E2 the receipt-derived
     * in-memory shape disagrees and verification fails before startup; E3 the
     * probe changes no database or sidecar bytes. Decision table:
     * RS1=C1+!C2=>accept (covered by SP2d), RS2=C1+C2=>E1+E2+E3. Both physical
     * prefixes and the forged receipt identity are derived from the selected
     * canonical bundle, not repeated as test-owned schema. */
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("forged-current-schema.db");
    let conn = Connection::open(&path).unwrap();
    let original = original_published_session_bundle().unwrap();
    awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
        .unwrap()
        .run_bundle(&conn, &migration_prefix_through(&original, 12))
        .unwrap();
    let v13 = original
        .migrations()
        .iter()
        .find(|migration| migration.version() == 13)
        .expect("canonical V13 aggregate migration");
    conn.execute(
        "INSERT INTO managed_schema_migrations \
            (bundle_id, version, checksum, description, applied_by) \
         VALUES (?1, ?2, ?3, ?4, 'receipt-schema-probe')",
        params![
            original.bundle_id(),
            v13.version(),
            v13.checksum_for(awaken_scoped_migration::Dialect::Sqlite),
            v13.ledger_description(),
        ],
    )
    .unwrap();
    drop(conn);

    let before = directory_snapshot(directory.path());
    let error = SqliteManagedSessionRepository::verify_existing(&path.to_string_lossy())
        .expect_err("RS2 receipt/schema disagreement must fail closed");
    assert!(
        error.contains("disagrees with migration receipts"),
        "RS2/E2: {error}"
    );
    assert_eq!(directory_snapshot(directory.path()), before, "RS2/E3");
    assert!(
        SqliteManagedSessionRepository::open(&path.to_string_lossy()).is_err(),
        "RS2/E1 canonical opener cannot read the forged shape"
    );
}

#[tokio::test]
async fn sqlite_original_history_converges_without_a_parallel_session_model() {
    // Causes: L1 exact original V1..V22 receipts; L2 canonical aggregate bytes
    // in the nullable published column; L3 valid quarantine/deployment children;
    // L4 a new command after upgrade. Effects: E1 branch-local V23 plus one
    // convergence receipt; E2 one envelope aggregate and no retired root columns;
    // E3 all children preserved under current constraints/indexes; E4 the normal
    // repository writes a new root without legacy-column defaults. Rule
    // M1=L1+L2+L3+L4=>E1+E2+E3+E4. Missing/invalid aggregates and orphan children
    // are negative constraints: migration must roll back before serving.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session-original-upgrade.db");
    let path = path.to_string_lossy().to_string();
    let conn = Connection::open(&path).unwrap();
    awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
        .unwrap()
        .run_bundle(&conn, &original_published_session_bundle().unwrap())
        .unwrap();
    let mut legacy = sample("legacy-lineage");
    legacy.revision = SessionRevision(1);
    conn.execute(
        "INSERT INTO managed_session \
            (session_id,agent_id,model,title,metadata_json,environment_id,mcp_json,scope_id,revision,aggregate_json) \
         VALUES (?1,'legacy-agent','legacy-model',NULL,'{}','legacy-env','[]','legacy-space',1,?2)",
        params![legacy.session_id, serde_json::to_string(&legacy).unwrap()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO managed_session_quarantine(session_id,reason) VALUES(?1,'audit')",
        params![legacy.session_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO managed_deployment(deployment_id,workspace_id,data,revision) VALUES('dep','legacy-space','{}',0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO managed_deployment_run(run_id,deployment_id,workspace_id,data) VALUES('run','dep','legacy-space','{}')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO managed_deployment_claim(claim_id,run_id) VALUES('claim','run')",
        [],
    )
    .unwrap();
    drop(conn);

    let repo = SqliteManagedSessionRepository::open(&path).expect("M1 migration");
    assert_eq!(repo.get(&legacy.session_id).await.unwrap(), legacy, "E2");
    create_fixture(&repo, "legacy-space", sample("post-upgrade"), Vec::new()).await;
    let conn = repo.conn.lock().unwrap();
    let ledgers: (i64, i64) = conn
        .query_row(
            "SELECT \
               (SELECT COUNT(*) FROM managed_schema_migrations WHERE bundle_id='awaken.managed_session'), \
               (SELECT COUNT(*) FROM managed_schema_migrations WHERE bundle_id='awaken.managed_session.converged')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(ledgers, (23, 1), "E1");
    let retired_columns: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('managed_session') WHERE name IN ('agent_id','metadata_json','runtime_json')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retired_columns, 0, "E2");
    let canonical: String = conn
        .query_row(
            "SELECT aggregate_json FROM managed_session WHERE session_id=?1",
            params![legacy.session_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(canonical.contains("\"format\":\"awaken.session.v1\""), "E2");
    let preserved: (i64, i64) = conn
        .query_row(
            "SELECT \
               (SELECT COUNT(*) FROM managed_session_quarantine WHERE session_id=?1 AND observed_revision=1), \
               (SELECT COUNT(*) FROM managed_deployment_claim WHERE claim_id='claim')",
            params![legacy.session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(preserved, (1, 1), "E3");
    let foreign_key_errors: i64 = conn
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(foreign_key_errors, 0, "E3");
}

#[tokio::test]
async fn postgres_original_history_converges_with_the_same_domain_effects() {
    // PostgreSQL parity for M1 above: exact original receipts plus valid roots
    // and children must produce the same canonical aggregate, branch-local V23,
    // convergence receipt, preserved children, and normal post-upgrade writes.
    // The dialect-specific effect is in-place constraint/column conversion under
    // the migration lock. Only an unconfigured local database may self-skip;
    // an explicitly configured but unreachable gate is a test failure.
    use sqlx::Executor;
    use sqlx::postgres::PgPool;

    let Some((url, admin)) = postgres_test_admin().await else {
        return;
    };
    let _ = admin
        .execute("DROP SCHEMA IF EXISTS t_session_original_upgrade CASCADE")
        .await;
    admin
        .execute("CREATE SCHEMA t_session_original_upgrade")
        .await
        .unwrap();
    let pool = PgPool::connect(&scoped_postgres_url(&url, "t_session_original_upgrade"))
        .await
        .unwrap();
    awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
        .unwrap()
        .run_bundle(&original_published_session_bundle().unwrap())
        .await
        .unwrap();
    let mut legacy = sample("legacy-pg-lineage");
    legacy.revision = SessionRevision(1);
    sqlx::query(
        "INSERT INTO managed_session \
            (session_id,agent_id,model,title,metadata_json,environment_id,mcp_json,scope_id,revision,aggregate_json) \
         VALUES ($1,'legacy-agent','legacy-model',NULL,'{}','legacy-env','[]','legacy-space',1,$2)",
    )
    .bind(&legacy.session_id)
    .bind(serde_json::to_string(&legacy).unwrap())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO managed_session_quarantine(session_id,reason) VALUES($1,'audit')")
        .bind(&legacy.session_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO managed_deployment(deployment_id,workspace_id,data,revision) VALUES('dep','legacy-space','{}',0)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO managed_deployment_run(run_id,deployment_id,workspace_id,data) VALUES('run','dep','legacy-space','{}')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO managed_deployment_claim(claim_id,run_id) VALUES('claim','run')")
        .execute(&pool)
        .await
        .unwrap();

    let repo = PostgresManagedSessionRepository::with_pool(pool.clone())
        .await
        .expect("M1 postgres migration");
    assert_eq!(repo.get(&legacy.session_id).await.unwrap(), legacy, "E2");
    create_fixture(&repo, "legacy-space", sample("post-pg-upgrade"), Vec::new()).await;
    let ledgers: (i64, i64) = sqlx::query_as(
        "SELECT \
           (SELECT COUNT(*) FROM managed_schema_migrations WHERE bundle_id='awaken.managed_session'), \
           (SELECT COUNT(*) FROM managed_schema_migrations WHERE bundle_id='awaken.managed_session.converged')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(ledgers, (23, 1), "E1");
    let retired_columns: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.columns \
         WHERE table_schema='t_session_original_upgrade' AND table_name='managed_session' \
           AND column_name IN ('agent_id','metadata_json','runtime_json')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(retired_columns, 0, "E2");
    let preserved: (i64, i64) = sqlx::query_as(
        "SELECT \
           (SELECT COUNT(*) FROM managed_session_quarantine WHERE session_id=$1 AND observed_revision=1), \
           (SELECT COUNT(*) FROM managed_deployment_claim WHERE claim_id='claim')",
    )
    .bind(&legacy.session_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(preserved, (1, 1), "E3");
    pool.close().await;
    admin
        .execute("DROP SCHEMA IF EXISTS t_session_original_upgrade CASCADE")
        .await
        .unwrap();
    admin.close().await;
}

#[tokio::test]
async fn postgres_published_prefixes_converge_and_server_open_is_read_only() {
    /* PostgreSQL history/open cause-effect decision table. Causes: C1 the
     * published lineage is original V1/V9/V12/V13/V22 or compacted V20; C2 a
     * pre-V13 prefix has no canonical aggregate-bearing row; C3 a V13+ prefix
     * has one decodable aggregate whose vault, credential source, and recovery
     * predicates all project durable rows; C4 all current receipts and exact
     * projections exist; C5 one row is missing from each derived projection;
     * C6 a pre-V13 root exists and therefore cannot be reconstructed from the
     * retired SQL columns; C7 a decodable root still uses the published
     * pre-envelope encoding. Effects: E1 the operational opener reaches the one
     * current lineage; E2 all three projections are rebuilt from the canonical
     * root; E3 Server open succeeds without changing roots, projections, or
     * receipts; E4 Server open rejects drift without repairing it; E5 rerunning
     * the operational opener atomically repairs every projection; E6 C6 fails
     * before convergence and never invents an aggregate; E7 Server rejects C7
     * unchanged and operational migration canonicalizes it. Rules:
     * PGV1..PGV3=C1+C2(V1/V9/V12)=>E1; PGV4..PGV6=C1+C3(V13/V22/V20)=>E1+E2;
     * PGV7=C4=>E3; PGV8=C5=>E4+E5; PGV9=C6=>E6; PGV10=C7=>E7. Every prefix
     * fixture is sliced from the canonical published bundle, so the test owns
     * no copied DDL or checksum table. */
    use sqlx::Executor;
    use sqlx::postgres::PgPool;

    let Some((url, admin)) = postgres_test_admin().await else {
        return;
    };
    let original = original_published_session_bundle().unwrap();
    let compacted = compacted_published_session_bundle().unwrap();

    for (rule, schema, source, tail, seed_root, expected_main_receipts) in [
        (
            "PGV1",
            "t_session_original_v1",
            &original,
            1,
            false,
            23_usize,
        ),
        ("PGV2", "t_session_original_v9", &original, 9, false, 23),
        ("PGV3", "t_session_original_v12", &original, 12, false, 23),
        ("PGV4", "t_session_original_v13", &original, 13, true, 23),
        (
            "PGV5",
            "t_session_original_v22_matrix",
            &original,
            22,
            true,
            23,
        ),
        ("PGV6", "t_session_compacted_v20", &compacted, 20, true, 21),
    ] {
        let _ = admin
            .execute(format!("DROP SCHEMA IF EXISTS {schema} CASCADE").as_str())
            .await;
        admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .unwrap_or_else(|error| panic!("{rule} create schema: {error}"));
        let scoped_url = scoped_postgres_url(&url, schema);
        let pool = PgPool::connect(&scoped_url)
            .await
            .unwrap_or_else(|error| panic!("{rule} connect schema: {error}"));
        let prefix = migration_prefix_through(source, tail);
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .unwrap()
            .run_bundle(&prefix)
            .await
            .unwrap_or_else(|error| panic!("{rule} apply prefix: {error}"));

        let seeded = seed_root.then(|| {
            let mut session = sample_with_credential_source(
                &format!("session-{rule}"),
                &format!("credential-{rule}"),
            );
            let SessionBaselineState::Frozen(baseline) = &mut session.baseline else {
                panic!("test sample must have a frozen baseline")
            };
            baseline.mcp_authoring.ordered_vault_ids = vec![format!("vault-{rule}")];
            session.revision = SessionRevision(1);
            session
        });
        if let Some(session) = &seeded {
            assert!(
                session.needs_reconciliation(),
                "{rule} fixture precondition"
            );
            sqlx::query(
                "INSERT INTO managed_session \
                    (session_id,agent_id,model,title,metadata_json,environment_id,mcp_json,scope_id,revision,aggregate_json) \
                 VALUES ($1,'legacy-agent','legacy-model',NULL,'{}','legacy-env','[]','legacy-space',1,$2)",
            )
            .bind(&session.session_id)
            .bind(serde_json::to_string(session).unwrap())
            .execute(&pool)
            .await
            .unwrap_or_else(|error| panic!("{rule} seed aggregate: {error}"));
        }

        let migrated = PostgresManagedSessionRepository::with_pool(pool.clone())
            .await
            .unwrap_or_else(|error| panic!("{rule}/E1 migrate prefix: {error}"));
        if let Some(session) = &seeded {
            assert_eq!(
                migrated.get(&session.session_id).await.unwrap(),
                session.clone(),
                "{rule}/E1 canonical root"
            );
        }
        drop(migrated);

        let baseline = postgres_session_snapshot(&pool).await;
        assert_eq!(
            baseline
                .receipts
                .iter()
                .filter(|(bundle, _, _)| bundle == BUNDLE_ID)
                .count(),
            expected_main_receipts,
            "{rule}/E1 one selected lineage"
        );
        assert_eq!(
            baseline
                .receipts
                .iter()
                .filter(|(bundle, _, _)| bundle == CONVERGED_BUNDLE_ID)
                .count(),
            1,
            "{rule}/E1 one convergence receipt"
        );
        let expected_projection_rows = if seed_root { 1 } else { 0 };
        assert_eq!(
            (
                baseline.reconciliation.len(),
                baseline.vaults.len(),
                baseline.credential_sources.len(),
            ),
            (
                expected_projection_rows,
                expected_projection_rows,
                expected_projection_rows,
            ),
            "{rule}/E2 all root-derived projections"
        );

        let verified = PostgresManagedSessionRepository::connect_existing(&scoped_url)
            .await
            .unwrap_or_else(|error| panic!("{rule}/PGV7 verify current state: {error}"));
        verified.pool.close().await;
        assert_eq!(
            postgres_session_snapshot(&pool).await,
            baseline,
            "{rule}/PGV7/E3 read-only Server open"
        );

        if rule == "PGV6" {
            sqlx::query("DELETE FROM managed_session_reconciliation_work")
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("DELETE FROM managed_session_vault_reference")
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("DELETE FROM managed_session_credential_source_reference")
                .execute(&pool)
                .await
                .unwrap();
            let drifted = postgres_session_snapshot(&pool).await;
            let error = match PostgresManagedSessionRepository::connect_existing(&scoped_url).await
            {
                Ok(_) => panic!("PGV8 Server open must not repair derived state"),
                Err(error) => error,
            };
            assert!(
                error.contains("operational migration rebuild"),
                "PGV8/E4: {error}"
            );
            assert_eq!(
                postgres_session_snapshot(&pool).await,
                drifted,
                "PGV8/E4 rejection is read-only"
            );
            drop(
                PostgresManagedSessionRepository::with_pool(pool.clone())
                    .await
                    .expect("PGV8/E5 operational repair"),
            );
            assert_eq!(
                postgres_session_snapshot(&pool).await,
                baseline,
                "PGV8/E5 one transaction repairs all projections"
            );

            let session = seeded.as_ref().expect("PGV10 seeded Session");
            sqlx::query("UPDATE managed_session SET aggregate_json=$2 WHERE session_id=$1")
                .bind(&session.session_id)
                .bind(serde_json::to_string(session).unwrap())
                .execute(&pool)
                .await
                .unwrap();
            let noncanonical = postgres_session_snapshot(&pool).await;
            let error = match PostgresManagedSessionRepository::connect_existing(&scoped_url).await
            {
                Ok(_) => panic!("PGV10 Server open must not normalize a root"),
                Err(error) => error,
            };
            assert!(
                error.contains("operational migration normalization"),
                "PGV10/E7: {error}"
            );
            assert_eq!(
                postgres_session_snapshot(&pool).await,
                noncanonical,
                "PGV10/E7 rejection is read-only"
            );
            drop(
                PostgresManagedSessionRepository::with_pool(pool.clone())
                    .await
                    .expect("PGV10/E7 operational normalization"),
            );
            assert_eq!(
                postgres_session_snapshot(&pool).await,
                baseline,
                "PGV10/E7 migration restores canonical root and projections"
            );
        }

        pool.close().await;
        admin
            .execute(format!("DROP SCHEMA IF EXISTS {schema} CASCADE").as_str())
            .await
            .unwrap_or_else(|error| panic!("{rule} drop schema: {error}"));
    }

    let schema = "t_session_original_v12_with_row";
    let _ = admin
        .execute(format!("DROP SCHEMA IF EXISTS {schema} CASCADE").as_str())
        .await;
    admin
        .execute(format!("CREATE SCHEMA {schema}").as_str())
        .await
        .unwrap();
    let scoped_url = scoped_postgres_url(&url, schema);
    let pool = PgPool::connect(&scoped_url).await.unwrap();
    let v12 = migration_prefix_through(&original, 12);
    awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
        .unwrap()
        .run_bundle(&v12)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO managed_session \
            (session_id,agent_id,model,title,metadata_json,environment_id,mcp_json,scope_id,revision) \
         VALUES ('pre-aggregate','legacy-agent','legacy-model',NULL,'{}','legacy-env','[]','legacy-space',1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let error = match PostgresManagedSessionRepository::with_pool(pool.clone()).await {
        Ok(_) => panic!("PGV9 must not reconstruct a pre-V13 root from retired columns"),
        Err(error) => error,
    };
    assert!(error.contains("no canonical aggregate"), "PGV9/E6: {error}");
    let aggregate: Option<String> =
        sqlx::query_scalar("SELECT aggregate_json FROM managed_session WHERE session_id=$1")
            .bind("pre-aggregate")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(aggregate, None, "PGV9/E6 no synthesized aggregate");
    assert!(
        PostgresManagedSessionRepository::connect_existing(&scoped_url)
            .await
            .is_err(),
        "PGV9/E6 Server remains closed"
    );
    pool.close().await;
    admin
        .execute(format!("DROP SCHEMA IF EXISTS {schema} CASCADE").as_str())
        .await
        .unwrap();
    admin.close().await;
}

#[test]
fn sqlite_legacy_upgrade_rejects_missing_roots_and_orphan_children_atomically() {
    // Negative rules for the legacy-upgrade cause graph. N1 a published row has
    // no aggregate -> reject before V23/DDL; N2 a quarantine child has no root ->
    // V23 transaction fails and rolls back. Effects: E1 legacy ledger remains at
    // V22; E2 old root shape remains recoverable; E3 no convergence receipt.
    // These rules prevent silent reconstruction from retired columns and prevent
    // an inner join from discarding orphan recovery evidence.
    let dir = tempfile::tempdir().unwrap();
    let missing_path = dir.path().join("missing-aggregate.db");
    let missing = Connection::open(&missing_path).unwrap();
    awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
        .unwrap()
        .run_bundle(&missing, &original_published_session_bundle().unwrap())
        .unwrap();
    missing
        .execute(
            "INSERT INTO managed_session \
                (session_id,agent_id,model,title,metadata_json,environment_id,mcp_json,scope_id,revision,aggregate_json) \
             VALUES ('missing','agent','model',NULL,'{}','env','[]','space',1,NULL)",
            [],
        )
        .unwrap();
    drop(missing);
    assert!(
        SqliteManagedSessionRepository::open(missing_path.to_str().unwrap()).is_err(),
        "N1"
    );

    let orphan_path = dir.path().join("orphan-quarantine.db");
    let orphan = Connection::open(&orphan_path).unwrap();
    awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
        .unwrap()
        .run_bundle(&orphan, &original_published_session_bundle().unwrap())
        .unwrap();
    orphan
        .execute(
            "INSERT INTO managed_session_quarantine(session_id,reason) VALUES('orphan','audit')",
            [],
        )
        .unwrap();
    drop(orphan);
    assert!(
        SqliteManagedSessionRepository::open(orphan_path.to_str().unwrap()).is_err(),
        "N2"
    );

    for path in [missing_path, orphan_path] {
        let connection = Connection::open(path).unwrap();
        let state: (i64, i64, i64) = connection
            .query_row(
                "SELECT \
                   (SELECT MAX(version) FROM managed_schema_migrations WHERE bundle_id='awaken.managed_session'), \
                   (SELECT COUNT(*) FROM managed_schema_migrations WHERE bundle_id='awaken.managed_session.converged'), \
                   (SELECT COUNT(*) FROM pragma_table_info('managed_session') WHERE name='agent_id')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, (22, 0, 1), "E1+E2+E3");
    }
}

#[tokio::test]
async fn sqlite_v2_dependency_backfill_is_atomic_and_restart_repairable() {
    // V2 startup-backfill cause/effect graph. C1 a published V1 database has a
    // healthy canonical root; C2 another V1 root is corrupt; C3 the corrupt root
    // is repaired before restart; C4 V2 already exists but its derived rows are
    // missing. Effects: E1 V1 checksum is accepted and V2 DDL is applied once;
    // E2 canonical decode failure prevents repository service and rolls the
    // entire rebuild back; E3 repaired restart indexes every actual source while
    // preserving both ordinary roots; E4 every later constructor deterministically
    // repairs the complete derived table without another completion registry.
    //
    // | Rule | V2 | root decode | derived rows | Effect |
    // | B1 | absent | all valid | absent | E1 + E3 |
    // | B2 | absent/present | one corrupt | any | E2, no partial rebuild |
    // | B3 | present | repaired | absent | E3 + E4 |
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session-source-v1-upgrade.db");
    let path = path.to_string_lossy().to_string();
    let mut healthy = sample_with_credential_source("sesn_source_v1", "source-v1");
    healthy.revision = SessionRevision(1);
    let conn = Connection::open(&path).unwrap();
    let full = session_bundle().unwrap();
    let v1 = migration_prefix_through(&full, 1);
    awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
        .unwrap()
        .run_bundle(&conn, &v1)
        .unwrap();
    conn.execute(
        "INSERT INTO managed_session (session_id, scope_id, revision, aggregate_json) \
         VALUES (?1, ?2, ?3, ?4)",
        params![
            healthy.session_id,
            "ws-source",
            db_revision(healthy.revision).unwrap(),
            aggregate_str(&healthy).unwrap(),
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO managed_session (session_id, scope_id, revision, aggregate_json) \
         VALUES (?1, ?2, ?3, ?4)",
        params!["sesn_source_corrupt", "ws-source", 1_i64, "{"],
    )
    .unwrap();
    drop(conn);

    let first = SqliteManagedSessionRepository::open(&path);
    assert!(first.is_err(), "B2/E2 canonical decode prevents service");
    let conn = Connection::open(&path).unwrap();
    let ledger_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM managed_schema_migrations \
             WHERE bundle_id = ?1",
            params!["awaken.managed_session"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ledger_count, 2, "B2/E1 V2 DDL and receipt are durable");
    let dependency_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM managed_session_credential_source_reference",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dependency_count, 0, "B2/E2 rebuild rolled back atomically");
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM managed_session", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        2,
        "B2/E2 canonical roots are untouched"
    );

    let mut repaired = sample_with_credential_source("sesn_source_corrupt", "source-repaired");
    repaired.revision = SessionRevision(1);
    conn.execute(
        "UPDATE managed_session SET aggregate_json = ?1 WHERE session_id = ?2",
        params![aggregate_str(&repaired).unwrap(), repaired.session_id],
    )
    .unwrap();
    drop(conn);

    let repo = SqliteManagedSessionRepository::open(&path).expect("B3 repaired restart");
    for (source_id, session_id) in [
        ("source-v1", "sesn_source_v1"),
        ("source-repaired", "sesn_source_corrupt"),
    ] {
        assert_eq!(
            repo.sessions_referencing_credential_source(
                "ws-source",
                &awaken_credential_contract::CredentialSourceId(source_id.into()),
            )
            .await
            .unwrap()[0]
                .session_id,
            session_id,
            "B3/E3"
        );
    }
    drop(repo);

    let conn = Connection::open(&path).unwrap();
    conn.execute(
        "DELETE FROM managed_session_credential_source_reference \
         WHERE credential_source_id = ?1",
        params!["source-v1"],
    )
    .unwrap();
    drop(conn);
    let reopened = SqliteManagedSessionRepository::open(&path).expect("B3 restart repair");
    assert_eq!(
        reopened
            .sessions_referencing_credential_source(
                "ws-source",
                &awaken_credential_contract::CredentialSourceId("source-v1".into()),
            )
            .await
            .unwrap()[0]
            .session_id,
        "sesn_source_v1",
        "B3/E4"
    );
}

#[tokio::test]
async fn postgres_v2_dependency_backfill_and_operational_repair_use_the_same_root_decoder() {
    // Backend-parity rules reuse B1/B3 above: P1 a V1 canonical root plus
    // absent V2 -> apply additive DDL and rebuild; P2 V2 present plus missing
    // derived row -> the operational migration opener rebuilds it. Effects are
    // the same exact Workspace+source discovery and unchanged canonical
    // aggregate. The PostgreSQL table locks additionally serialize P1/P2 with
    // root writers. Server's read-only rejection is covered by PGV8 below.
    use sqlx::Executor;
    use sqlx::postgres::PgPool;

    let Some((url, admin)) = postgres_test_admin().await else {
        return;
    };
    let _ = admin
        .execute("DROP SCHEMA IF EXISTS t_session_source_backfill CASCADE")
        .await;
    admin
        .execute("CREATE SCHEMA t_session_source_backfill")
        .await
        .expect("create source-backfill schema");
    let pool = PgPool::connect(&scoped_postgres_url(&url, "t_session_source_backfill"))
        .await
        .expect("connect source-backfill schema");
    let full = session_bundle().unwrap();
    let v1 = migration_prefix_through(&full, 1);
    awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
        .unwrap()
        .run_bundle(&v1)
        .await
        .unwrap();
    let mut session = sample_with_credential_source("sesn_pg_source_v1", "source-pg-v1");
    session.revision = SessionRevision(1);
    sqlx::query(
        "INSERT INTO managed_session (session_id, scope_id, revision, aggregate_json) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(&session.session_id)
    .bind("ws-source")
    .bind(db_revision(session.revision).unwrap())
    .bind(aggregate_str(&session).unwrap())
    .execute(&pool)
    .await
    .unwrap();

    let repo = PostgresManagedSessionRepository::with_pool(pool.clone())
        .await
        .expect("P1 migrate and backfill");
    let source_id = awaken_credential_contract::CredentialSourceId("source-pg-v1".into());
    assert_eq!(
        repo.sessions_referencing_credential_source("ws-source", &source_id)
            .await
            .unwrap()[0]
            .session_id,
        session.session_id,
        "P1"
    );
    assert_eq!(
        repo.get(&session.session_id).await.unwrap(),
        session,
        "P1 root"
    );

    sqlx::query("DELETE FROM managed_session_credential_source_reference")
        .execute(&pool)
        .await
        .unwrap();
    let restarted = PostgresManagedSessionRepository::with_pool(pool.clone())
        .await
        .expect("P2 restart rebuild");
    assert_eq!(
        restarted
            .sessions_referencing_credential_source("ws-source", &source_id)
            .await
            .unwrap()[0]
            .session_id,
        session.session_id,
        "P2"
    );
    pool.close().await;
    admin
        .execute("DROP SCHEMA IF EXISTS t_session_source_backfill CASCADE")
        .await
        .expect("drop source-backfill schema");
    admin.close().await;
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
    match repo
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
        .expect("create Session fixture")
    {
        awaken_session_contract::SessionCreateResult::Applied(session)
        | awaken_session_contract::SessionCreateResult::Replayed(session) => session,
    }
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
    install_legacy_completion_wire(session, &command);
    session
        .normalize_legacy_terminal_cleanup("test fixture released")
        .unwrap();
}

/// Build only historical persisted bytes; no current Runtime completion API is
/// retained for storage fixtures. Cause/effect: an exact frozen command plus a
/// canonical v1 receipt decodes as legacy evidence; a different command or
/// fingerprint is rejected by the aggregate normalizer. Decision rule L1 uses
/// this helper to seed old wire, while all current completion transitions use
/// the two-stage preparation/disposal authority.
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
    let completion = serde_json::json!({
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
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .expect("legacy completions wire is an object")
        .insert(command.thread_id.clone(), completion);
    *session = serde_json::from_value(encoded).unwrap();
}

fn requested_cleanup_with_completion(session_id: &str) -> PersistedSession {
    let mut session = sample(session_id);
    assert!(session.terminal_cleanup.request(session_id));
    session
        .terminal_cleanup
        .freeze_targets(session_id, std::iter::empty::<String>(), 0, 0)
        .expect("freeze cleanup compatibility fixture");
    let command = session
        .terminal_cleanup
        .command_for(session_id, session_id)
        .expect("cleanup compatibility command");
    install_legacy_completion_wire(&mut session, &command);
    let revision = i64::try_from(session.revision.0).expect("test revision fits SQL index");
    let encoded: serde_json::Value =
        serde_json::from_str(&aggregate_str(&session).unwrap()).unwrap();
    decode(EncodedSessionRow {
        aggregate_json: encoded.to_string(),
        revision,
    })
    .expect("historical one-stage cleanup completion wire remains readable")
}

fn removed_bundle_v2_aggregate(session: &PersistedSession) -> serde_json::Value {
    let session_id = session.session_id.as_str();
    let command = session
        .terminal_cleanup
        .command_for(session_id, session_id)
        .expect("cleanup compatibility command");
    let removed_fingerprint = "removed-bundle-receipt";
    let v2_fingerprint = awaken_session_contract::stable_fingerprint(&(
        "session-terminal-cleanup-thread-receipt-v2",
        command.session_id.as_str(),
        command.thread_id.as_str(),
        command.effect_id.as_str(),
        Vec::<(&str, &str)>::new(),
        vec![removed_fingerprint],
    ));
    let mut encoded: serde_json::Value =
        serde_json::from_str(&aggregate_str(session).unwrap()).unwrap();
    let completion = &mut encoded["aggregate"]["terminal_cleanup"]["completions"][session_id];
    completion["artifact_bundle_receipts"] = serde_json::json!([{
        "purpose": "patch_bundle",
        "patch_sha256": "sha256:removed",
        "patch_artifact_id": "file-patch",
        "manifest_artifact_id": "file-manifest",
        "checksum_artifact_id": "file-checksum",
        "receipt_fingerprint": removed_fingerprint,
    }]);
    completion["receipt_fingerprint"] = serde_json::json!(v2_fingerprint);
    encoded
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
    assert_eq!(
        reopened
            .count_environment_phase(SessionEnvironmentPhase::Resident)
            .await
            .unwrap(),
        1,
        "the file-backed SQLite canonical store owns the phase count"
    );
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
        install_legacy_completion_wire(&mut recovered, &intent);
        install_legacy_completion_wire(&mut recovered, &child_intent);
        recovered
            .normalize_legacy_terminal_cleanup("test fixture released")
            .unwrap();
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
async fn removed_cleanup_v2_converges_through_sqlite_reopen_and_cas() {
    // Causes: C1 a cold row contains an exact removed v2 completion or a forged
    // v2 fingerprint; C2 the process restarts before reading it; C3 the exact
    // frozen cleanup command still owns the completion. Effects: E1 forged v2
    // fails closed without rewriting the row; E2 exact v2 rehydrates as current
    // ordinary-File evidence; E3 an unrelated aggregate CAS retains Requested
    // cleanup but rewrites its completion only as current v1; E4 the same
    // operation then completes; E5 another restart observes the terminal state.
    // Decision rules: S1=C1(forged)+C2=>E1; S2=C1(exact)+C2+C3=>E2+E3;
    // S3=S2+complete=>E4; S4=S3+restart=>E5. PostgreSQL uses the same decoder
    // and is exercised in its live parity test.
    let session_id = "sesn_cleanup_removed_v2";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sessions-cleanup-v2.db");
    let path = path.to_string_lossy().to_string();
    let legacy = {
        let repo = SqliteManagedSessionRepository::open(&path).unwrap();
        let requested = create_fixture(
            &repo,
            "ws_a",
            requested_cleanup_with_completion(session_id),
            Vec::new(),
        )
        .await;
        removed_bundle_v2_aggregate(&requested)
    };

    let mut forged = legacy.clone();
    forged["aggregate"]["terminal_cleanup"]["completions"][session_id]["receipt_fingerprint"] =
        serde_json::json!("forged");
    let conn = Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE managed_session SET aggregate_json = ?1 WHERE session_id = ?2",
        params![forged.to_string(), session_id],
    )
    .unwrap();
    drop(conn);
    let Err(error) = SqliteManagedSessionRepository::open(&path) else {
        panic!("S1/E1 corrupt cold row must fail startup");
    };
    assert!(error.contains("not canonical"), "S1/E1: {error}");

    let conn = Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE managed_session SET aggregate_json = ?1 WHERE session_id = ?2",
        params![legacy.to_string(), session_id],
    )
    .unwrap();
    drop(conn);
    let repo = SqliteManagedSessionRepository::open(&path).unwrap();
    let mut recovered = repo.get(session_id).await.expect("S2/E2 rehydrate");
    recovered.title = Some("compatibility rewrite".into());
    let mut recovered = replace_fixture(
        &repo,
        "ws_a",
        recovered,
        "test:cleanup:removed-v2:rewrite",
        Vec::new(),
    )
    .await;
    let persisted: String = repo
        .conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT aggregate_json FROM managed_session WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!persisted.contains("artifact_bundle_receipts"), "S2/E3");
    assert!(persisted.contains("\"completions\""), "S2/E3 Requested");

    assert_eq!(
        recovered.has_complete_legacy_terminal_cleanup_evidence(),
        Ok(true),
        "S2/E2 current receipt set"
    );
    assert!(
        recovered
            .normalize_legacy_terminal_cleanup("S3 historical cleanup normalized")
            .expect("S3/E4 complete"),
        "S3/E4",
    );
    replace_fixture(
        &repo,
        "ws_a",
        recovered,
        "test:cleanup:removed-v2:complete",
        Vec::new(),
    )
    .await;
    drop(repo);

    let reopened = SqliteManagedSessionRepository::open(&path).unwrap();
    assert!(
        reopened
            .get(session_id)
            .await
            .expect("S4/E5 reopen")
            .terminal_cleanup
            .is_completed(),
        "S4/E5",
    );
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

/// Live Postgres round-trip, isolated in its own schema. An ordinary local run
/// may skip when no database is configured or reachable; an explicitly
/// configured `AWAKEN_TEST_DATABASE_URL` must connect or fail the test.
#[tokio::test]
async fn postgres_round_trips_and_upserts() {
    use awaken_ext_memory::{MemoryExtractionRepository, PutMemoryExtractionOutcome};
    use sqlx::Executor;
    use sqlx::postgres::PgPool;

    let Some((url, admin)) = postgres_test_admin().await else {
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
    let pool = PgPool::connect(&scoped_postgres_url(&url, "t_managed_session"))
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

    /* Removed-completion rolling-upgrade parity. Causes/effects/rules are S2-S4
     * in the SQLite cold-reopen test: an exact v2 row must pass through this
     * backend's constructor and the same type decoder, be rewritten as Requested
     * v1 through the existing CAS, and then complete without a second path. */
    let legacy_id = "sesn_pg_cleanup_removed_v2";
    let requested = create_fixture(
        &repo,
        "ws_a",
        requested_cleanup_with_completion(legacy_id),
        Vec::new(),
    )
    .await;
    sqlx::query("UPDATE managed_session SET aggregate_json = $1 WHERE session_id = $2")
        .bind(removed_bundle_v2_aggregate(&requested).to_string())
        .bind(legacy_id)
        .execute(&repo.pool)
        .await
        .unwrap();
    let restarted = PostgresManagedSessionRepository::with_pool(repo.pool.clone())
        .await
        .expect("S2 PostgreSQL restart");
    let mut recovered = restarted.get(legacy_id).await.expect("S2/E2 PostgreSQL");
    recovered.title = Some("compatibility rewrite".into());
    let mut recovered = replace_fixture(
        &restarted,
        "ws_a",
        recovered,
        "test:pg:cleanup:removed-v2:rewrite",
        Vec::new(),
    )
    .await;
    let persisted: String =
        sqlx::query_scalar("SELECT aggregate_json FROM managed_session WHERE session_id = $1")
            .bind(legacy_id)
            .fetch_one(&repo.pool)
            .await
            .unwrap();
    assert!(
        !persisted.contains("artifact_bundle_receipts") && persisted.contains("\"completions\""),
        "S2/E3 PostgreSQL Requested v1 rewrite",
    );
    assert_eq!(
        recovered.has_complete_legacy_terminal_cleanup_evidence(),
        Ok(true),
        "S2/E2 PostgreSQL receipt set"
    );
    assert!(
        recovered
            .normalize_legacy_terminal_cleanup("S3 historical cleanup normalized")
            .expect("S3/E4 PostgreSQL complete"),
        "S3/E4 PostgreSQL",
    );
    replace_fixture(
        &restarted,
        "ws_a",
        recovered,
        "test:pg:cleanup:removed-v2:complete",
        Vec::new(),
    )
    .await;
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
/// stands in for a restart (the cross-process fence input the edge guard reads).
/// Only an unconfigured ordinary local run may self-skip.
#[tokio::test]
async fn postgres_owner_scope_is_recorded_and_survives_a_reopen() {
    use sqlx::Executor;
    use sqlx::postgres::PgPool;

    let Some((url, admin)) = postgres_test_admin().await else {
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

    let scoped_url = scoped_postgres_url(&url, "t_managed_session_owner");
    let pool = || PgPool::connect(&scoped_url);

    // First "process": save the row and owner atomically.
    let repo = PostgresManagedSessionRepository::with_pool(pool().await.expect("schema pool"))
        .await
        .expect("store");
    create_fixture(&repo, "ws_a", sample("sesn_1"), Vec::new()).await;
    assert_eq!(repo.owner("sesn_1").await, Ok("ws_a".to_string()));

    // Second "process": a fresh pool over the same schema still reads the owner.
    let reopened =
        PostgresManagedSessionRepository::with_pool(pool().await.expect("reopen schema pool"))
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
