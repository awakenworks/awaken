//! Real SQL evidence for the selected read capability, not a new Store fake.
use super::*;
use awaken_session_repository_contract::SessionRepositoryReads;
use awaken_session_state_contract::SessionScanQuery;
use sqlx::Executor as _;

const COMMAND_SNAPSHOT_SQL: &str = "SELECT session_id,idempotency_key,payload_hash,committed_revision FROM managed_session_idempotency ORDER BY session_id,idempotency_key";

fn read_fixture() -> PersistedSession {
    let mut root = sample_with_credential_source("read-root", "read-credential");
    let SessionBaselineState::Frozen(baseline) = &mut root.baseline else {
        unreachable!("original fixture is frozen")
    };
    baseline.mcp_authoring.ordered_vault_ids = vec!["read-vault".into()];
    root.realization = Some(
        awaken_session_configuration_contract::resource::SessionRealizationLease {
            owner: "read-worker".into(),
            runtime_incarnation: "read-worker/1".into(),
            epoch: 1,
            expires_at_unix_ms: 10_000,
        },
    );
    root
}

async fn select_table(reader: &dyn SessionRepositoryReads, original: &PersistedSession) {
    // S1 cause/effect table, common to BOTH actual SQL backends. C1 the
    // selected port is the same original Store allocation; C2 one healthy root
    // has stale quarantine evidence; C3 selects get/owner/tombstone or each of
    // the FOUR SELECT variants. Every C3 returns its original observation and
    // does not maintain quarantine/root/receipt/outbox. Caller SQL assertions
    // inspect the sentinel; a maintenance dispatch would clear it and fail.
    assert_eq!(reader.get(&original.session_id).await.unwrap(), *original);
    assert_eq!(reader.owner(&original.session_id).await.unwrap(), "read-ws");
    assert!(
        reader
            .deleted_event_admissions("read-ws", &original.session_id)
            .await
            .unwrap()
            .is_none()
    );
    let source = awaken_credential_contract::CredentialSourceId("read-credential".into());
    for query in [
        SessionScanQuery::Workspace {
            workspace_id: "read-ws",
        },
        SessionScanQuery::WorkerRealizationOwner {
            worker_id: "read-worker",
        },
        SessionScanQuery::VaultReferences {
            workspace_id: "read-ws",
            vault_id: "read-vault",
        },
        SessionScanQuery::CredentialSourceReferences {
            workspace_id: "read-ws",
            source_id: &source,
        },
    ] {
        let selected = reader.scan(query).await.unwrap();
        assert!(
            selected.quarantined.is_empty(),
            "S1 SELECT never returns maintenance evidence"
        );
        assert_eq!(selected.next_cursor, None, "S1 complete unpaged read");
        assert_eq!(
            selected.into_sessions(),
            vec![original.clone()],
            "S1 {query:?}"
        );
    }
}

#[tokio::test]
async fn sqlite_reader_preserves_quarantine_and_only_maintenance_clears_it() {
    // S1SQLite executes the full table; S3 then invokes the ORIGINAL required
    // maintenance path and observes sentinel removal, proving the oracle is
    // sensitive to accidentally routing any read through reconciliation.
    let store = Arc::new(memory_repository().await);
    let original = create_fixture(store.as_ref(), "read-ws", read_fixture(), Vec::new()).await;
    sqlx::query("INSERT INTO managed_session_quarantine(session_id,reason,observed_revision) VALUES('read-root','sentinel',?1)")
        .bind(i64::try_from(original.revision.0).unwrap()).execute(&store.pool).await.unwrap();
    let before: Vec<(String, i64, String)> = sqlx::query_as("SELECT reason,observed_revision,CAST(quarantined_at AS TEXT) FROM managed_session_quarantine")
        .fetch_all(&store.pool).await.unwrap();
    let commands: Vec<(String, String, String, i64)> = sqlx::query_as(COMMAND_SNAPSHOT_SQL)
        .fetch_all(&store.pool)
        .await
        .unwrap();
    let facts = store.pending_lifecycle().await.unwrap();
    let repo: Arc<dyn SessionRepository> = store.clone();
    let reader: Arc<dyn SessionRepositoryReads> = repo;
    let expected: Arc<dyn SessionRepositoryReads> = store.clone();
    assert!(
        Arc::ptr_eq(&reader, &expected),
        "S1 upcast retains the same Store"
    );
    select_table(reader.as_ref(), &original).await;
    let after: Vec<(String, i64, String)> = sqlx::query_as("SELECT reason,observed_revision,CAST(quarantined_at AS TEXT) FROM managed_session_quarantine")
        .fetch_all(&store.pool).await.unwrap();
    assert_eq!(
        after, before,
        "S1 SQLite read cannot clear/update diagnostic evidence"
    );
    assert_eq!(store.get(&original.session_id).await.unwrap(), original);
    assert_eq!(store.pending_lifecycle().await.unwrap(), facts);
    assert_eq!(
        sqlx::query_as::<_, (String, String, String, i64)>(COMMAND_SNAPSHOT_SQL)
            .fetch_all(&store.pool)
            .await
            .unwrap(),
        commands,
        "S1 command identities/payloads/revisions cannot change in place or in count"
    );
    // S4 the maintenance-only view is an upcast of the SAME root authority;
    // its original scan may clear quarantine, but carries no root write role.
    let full: Arc<dyn SessionRepository> = store.clone();
    let maintenance: Arc<dyn SessionReconciliation> = full;
    let expected: Arc<dyn SessionReconciliation> = store.clone();
    assert!(Arc::ptr_eq(&maintenance, &expected), "S4 same Store");
    maintenance.reconciliation_scan(None).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM managed_session_quarantine")
            .fetch_one(&store.pool)
            .await
            .unwrap(),
        0,
        "S3 only maintenance revalidates quarantine"
    );
}

#[tokio::test]
async fn postgres_reader_preserves_quarantine_and_only_maintenance_clears_it() {
    // S1PG/S3 is the SAME cause/effect table against a real owned PostgreSQL
    // schema. An explicitly configured fixture must connect; no fake fallback.
    let Some((url, admin)) = postgres_test_admin().await else {
        return;
    };
    let schema = format!("t_session_read_{}", uuid::Uuid::new_v4().simple());
    admin
        .execute(format!("CREATE SCHEMA {schema}").as_str())
        .await
        .unwrap();
    let pool = sqlx::postgres::PgPool::connect(&scoped_postgres_url(&url, &schema))
        .await
        .unwrap();
    let store = Arc::new(
        PostgresManagedSessionRepository::with_pool(pool)
            .await
            .unwrap(),
    );
    let original = create_fixture(store.as_ref(), "read-ws", read_fixture(), Vec::new()).await;
    sqlx::query("INSERT INTO managed_session_quarantine(session_id,reason,observed_revision) VALUES('read-root','sentinel',$1)")
        .bind(i64::try_from(original.revision.0).unwrap()).execute(&store.pool).await.unwrap();
    let before: Vec<(String, i64, String)> = sqlx::query_as("SELECT reason,observed_revision,CAST(quarantined_at AS TEXT) FROM managed_session_quarantine")
        .fetch_all(&store.pool).await.unwrap();
    let commands: Vec<(String, String, String, i64)> = sqlx::query_as(COMMAND_SNAPSHOT_SQL)
        .fetch_all(&store.pool)
        .await
        .unwrap();
    let facts = store.pending_lifecycle().await.unwrap();
    let repo: Arc<dyn SessionRepository> = store.clone();
    let reader: Arc<dyn SessionRepositoryReads> = repo;
    let expected: Arc<dyn SessionRepositoryReads> = store.clone();
    assert!(
        Arc::ptr_eq(&reader, &expected),
        "S1 same PostgreSQL allocation"
    );
    select_table(reader.as_ref(), &original).await;
    let after: Vec<(String, i64, String)> = sqlx::query_as("SELECT reason,observed_revision,CAST(quarantined_at AS TEXT) FROM managed_session_quarantine")
        .fetch_all(&store.pool).await.unwrap();
    assert_eq!(
        after, before,
        "S1 PostgreSQL read cannot clear/update evidence"
    );
    assert_eq!(store.get(&original.session_id).await.unwrap(), original);
    assert_eq!(store.pending_lifecycle().await.unwrap(), facts);
    assert_eq!(
        sqlx::query_as::<_, (String, String, String, i64)>(COMMAND_SNAPSHOT_SQL)
            .fetch_all(&store.pool)
            .await
            .unwrap(),
        commands,
        "S1 command identities/payloads/revisions cannot change in place or in count"
    );
    // S4 shares the same positive and negative observations on PostgreSQL:
    // upcasting grants quarantine maintenance, never root mutation authority.
    let full: Arc<dyn SessionRepository> = store.clone();
    let maintenance: Arc<dyn SessionReconciliation> = full;
    let expected: Arc<dyn SessionReconciliation> = store.clone();
    assert!(
        Arc::ptr_eq(&maintenance, &expected),
        "S4 same PostgreSQL Store"
    );
    maintenance.reconciliation_scan(None).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM managed_session_quarantine")
            .fetch_one(&store.pool)
            .await
            .unwrap(),
        0,
        "S3 same canonical maintenance"
    );
    store.pool.close().await;
    admin
        .execute(format!("DROP SCHEMA {schema} CASCADE").as_str())
        .await
        .unwrap();
    admin.close().await;
}
