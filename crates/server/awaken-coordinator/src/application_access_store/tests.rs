use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use awaken_authz_enforce::{
    ApplicationAccessAuthenticator, ApplicationAuthenticationError, ApplicationGrant,
    ApplicationThreadBinding,
};

use super::postgres::PostgresApplicationAccessRepository;
use super::sqlite::SqliteApplicationAccessRepository;
use super::*;

fn grant() -> ApplicationGrant {
    ApplicationGrant {
        protocols: HashSet::from(["ai-sdk".into()]),
        operations: HashSet::from(["thread.run".into()]),
        thread_bindings: HashMap::from([(
            "external-thread".into(),
            ApplicationThreadBinding {
                managed_session_id: "sesn_existing".into(),
                agent_id: "support".into(),
            },
        )]),
    }
}

fn record(id: ApplicationAccessTokenId, token: &str) -> ApplicationAccessRecord {
    record_at(id, token, 100, u64::MAX / 4)
}

fn record_at(
    id: ApplicationAccessTokenId,
    token: &str,
    created_at_unix_ms: u64,
    expires_at_unix_ms: u64,
) -> ApplicationAccessRecord {
    ApplicationAccessRecord::new(
        id,
        ApplicationAccessTokenHash::from_presented(token),
        "workspace-a".into(),
        created_at_unix_ms,
        expires_at_unix_ms,
        grant(),
    )
    .expect("valid application access record")
}

struct RetentionProbeRepository {
    inner: Option<Arc<dyn ApplicationAccessRepository>>,
    fail_next_delete: AtomicBool,
    over_report_delete: bool,
    delete_calls: AtomicUsize,
}

impl RetentionProbeRepository {
    fn over(inner: Arc<dyn ApplicationAccessRepository>, fail_next_delete: bool) -> Self {
        Self {
            inner: Some(inner),
            fail_next_delete: AtomicBool::new(fail_next_delete),
            over_report_delete: false,
            delete_calls: AtomicUsize::new(0),
        }
    }

    fn always_full() -> Self {
        Self {
            inner: None,
            fail_next_delete: AtomicBool::new(false),
            over_report_delete: false,
            delete_calls: AtomicUsize::new(0),
        }
    }

    fn over_reporting() -> Self {
        Self {
            inner: None,
            fail_next_delete: AtomicBool::new(false),
            over_report_delete: true,
            delete_calls: AtomicUsize::new(0),
        }
    }

    fn inner(
        &self,
    ) -> Result<&Arc<dyn ApplicationAccessRepository>, ApplicationAccessRepositoryError> {
        self.inner.as_ref().ok_or_else(|| {
            ApplicationAccessRepositoryError::Unavailable(
                "retention-only probe has no record authority".into(),
            )
        })
    }
}

#[async_trait::async_trait]
impl ApplicationAccessRepository for RetentionProbeRepository {
    async fn create(
        &self,
        record: ApplicationAccessRecord,
    ) -> Result<(), ApplicationAccessRepositoryError> {
        self.inner()?.create(record).await
    }

    async fn find_by_token_hash(
        &self,
        token_hash: &ApplicationAccessTokenHash,
    ) -> Result<ApplicationAccessRecord, ApplicationAccessRepositoryError> {
        self.inner()?.find_by_token_hash(token_hash).await
    }

    async fn revoke(
        &self,
        workspace_id: &str,
        id: &ApplicationAccessTokenId,
        revoked_at_unix_ms: u64,
    ) -> Result<(), ApplicationAccessRepositoryError> {
        self.inner()?
            .revoke(workspace_id, id, revoked_at_unix_ms)
            .await
    }

    async fn delete_terminal_before(
        &self,
        terminal_before_unix_ms: u64,
        limit: u32,
    ) -> Result<u64, ApplicationAccessRepositoryError> {
        self.delete_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_next_delete.swap(false, Ordering::SeqCst) {
            return Err(ApplicationAccessRepositoryError::Unavailable(
                "transient-retention-diagnostic".into(),
            ));
        }
        match &self.inner {
            Some(inner) => {
                inner
                    .delete_terminal_before(terminal_before_unix_ms, limit)
                    .await
            }
            None => Ok(u64::from(limit) + u64::from(self.over_report_delete as u8)),
        }
    }
}

async fn repository_decision_table(
    writer: &dyn ApplicationAccessRepository,
    peer: &dyn ApplicationAccessRepository,
) {
    /* Persistence cause/effect graph:
     * C1 valid new id; C2 valid new token hash; C3 row exists; C4 revoked;
     * C5 backend shared across instances; C6 caller Workspace exact/foreign;
     * C7 terminal time before/equal/after retention boundary; C8 deletion
     * batch remaining. Effects are E1 exact
     * durable row, E2 Invalid with zero overwrite/partial insert, E3 NotFound,
     * E4 one globally visible first-revocation stamp, and E5 bounded terminal
     * deletion without removing live or retained terminal rows.
     *
     * | Rule | id/hash | row state | Workspace | replica | boundary | Effect |
     * |---|---|---|---|---|---|---|
     * | AAT-R1 | new/new | absent | exact | peer | n/a | E1 create/read |
     * | AAT-R2 | duplicate/new | live | exact | peer | n/a | E2 no overwrite |
     * | AAT-R3 | new/duplicate | live | exact | peer | n/a | E2 no partial row |
     * | AAT-R4 | any | absent | exact | peer | n/a | E3 NotFound |
     * | AAT-R5 | exact | live/revoked | exact | peer | n/a | E4 first stamp |
     * | TEN-R1 | exact | live | foreign | peer | n/a | E3, row remains live |
     * | RET-R1 | any | expired-old | n/a | either | before | E5 delete |
     * | RET-R2 | any | revoked-old | n/a | either | before | E5 delete |
     * | RET-R3 | any | live/recent | n/a | peer | equal/after | E5 retain |
     * | RET-R4 | any | two terminal | n/a | concurrent | before | E5 disjoint batches |
     */
    let original_id = ApplicationAccessTokenId::new();
    let original = record(original_id.clone(), "cleartext-original");
    writer.create(original.clone()).await.expect("AAT-R1");
    assert_eq!(
        peer.find_by_token_hash(&original.token_hash).await,
        Ok(original.clone()),
        "AAT-R1"
    );

    let duplicate_id = record(original_id, "cleartext-other");
    assert!(
        matches!(
            writer.create(duplicate_id).await,
            Err(ApplicationAccessRepositoryError::Invalid(_))
        ),
        "AAT-R2"
    );
    assert_eq!(
        peer.find_by_token_hash(&original.token_hash).await,
        Ok(original.clone()),
        "AAT-R2"
    );

    let duplicate_hash_id = ApplicationAccessTokenId::new();
    let duplicate_hash = record(duplicate_hash_id.clone(), "cleartext-original");
    assert!(
        matches!(
            writer.create(duplicate_hash).await,
            Err(ApplicationAccessRepositoryError::Invalid(_))
        ),
        "AAT-R3"
    );
    assert_eq!(
        writer.revoke("workspace-a", &duplicate_hash_id, 250).await,
        Err(ApplicationAccessRepositoryError::NotFound),
        "AAT-R3"
    );

    let missing_hash = ApplicationAccessTokenHash::from_presented("cleartext-missing");
    assert_eq!(
        peer.find_by_token_hash(&missing_hash).await,
        Err(ApplicationAccessRepositoryError::NotFound),
        "AAT-R4"
    );
    let missing_id = ApplicationAccessTokenId::new();
    assert_eq!(
        peer.revoke("workspace-a", &missing_id, 250).await,
        Err(ApplicationAccessRepositoryError::NotFound),
        "AAT-R4"
    );
    assert_eq!(
        peer.revoke("workspace-b", &original.id, 250).await,
        Err(ApplicationAccessRepositoryError::NotFound),
        "TEN-R1"
    );
    assert_eq!(
        peer.find_by_token_hash(&original.token_hash).await,
        Ok(original.clone()),
        "TEN-R1 foreign revoke leaves credential live"
    );

    let live = record_at(
        ApplicationAccessTokenId::new(),
        "retention-live",
        100,
        1_000,
    );
    let recent = record_at(
        ApplicationAccessTokenId::new(),
        "retention-recent",
        100,
        600,
    );
    let boundary = record_at(
        ApplicationAccessTokenId::new(),
        "retention-boundary",
        100,
        500,
    );
    let old_expired = record_at(
        ApplicationAccessTokenId::new(),
        "retention-old-expired",
        100,
        400,
    );
    let old_revoked = record_at(
        ApplicationAccessTokenId::new(),
        "retention-old-revoked",
        100,
        1_000,
    );
    for candidate in [
        live.clone(),
        recent.clone(),
        boundary.clone(),
        old_expired.clone(),
        old_revoked.clone(),
    ] {
        writer.create(candidate).await.expect("RET create fixture");
    }
    writer
        .revoke("workspace-a", &old_revoked.id, 350)
        .await
        .expect("RET-R2 revoke fixture");
    let (writer_deleted, peer_deleted) = tokio::join!(
        writer.delete_terminal_before(500, 1),
        peer.delete_terminal_before(500, 1),
    );
    assert_eq!(
        writer_deleted.unwrap() + peer_deleted.unwrap(),
        2,
        "RET-R1/RET-R2/RET-R4 replicas claim disjoint terminal rows"
    );
    assert_eq!(
        peer.find_by_token_hash(&old_expired.token_hash).await,
        Err(ApplicationAccessRepositoryError::NotFound),
        "RET-R1"
    );
    assert_eq!(
        peer.find_by_token_hash(&old_revoked.token_hash).await,
        Err(ApplicationAccessRepositoryError::NotFound),
        "RET-R2"
    );
    for retained in [&live, &recent, &boundary] {
        assert_eq!(
            peer.find_by_token_hash(&retained.token_hash).await,
            Ok((*retained).clone()),
            "RET-R3"
        );
    }
    assert_eq!(writer.delete_terminal_before(500, 1).await, Ok(0), "RET-R4");

    writer
        .revoke("workspace-a", &original.id, 300)
        .await
        .expect("AAT-R5");
    peer.revoke("workspace-a", &original.id, 400)
        .await
        .expect("AAT-R5");
    let revoked = peer
        .find_by_token_hash(&original.token_hash)
        .await
        .expect("AAT-R5 row remains");
    assert_eq!(revoked.revoked_at_unix_ms, Some(300), "AAT-R5");
}

/// SQLite adapter coverage rule: C1 two repository instances share one durable
/// file; E1 every AAT/TEN/RET rule owned by `repository_decision_table` passes,
/// including cross-instance revocation and disjoint retention claims. Rule
/// SQLITE-R1 is C1 -> E1; the helper remains the single decision-table owner.
#[tokio::test]
async fn sqlite_repository_conforms_and_two_instances_share_revocation() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("sessions.db");
    let first = SqliteApplicationAccessRepository::open(&path.to_string_lossy()).unwrap();
    let second = SqliteApplicationAccessRepository::open(&path.to_string_lossy()).unwrap();
    repository_decision_table(&first, &second).await;
}

/// Retention lifecycle cause/effect table. Let Q be the durable terminal-row
/// backlog, E newly eligible rows, and D rows deleted in one tick:
/// `Q[n+1] = max(0, Q[n] + E[n] - D[n])`.
///
/// | Rule | repository | batch result | Effect |
/// |---|---|---|---|
/// | RET-L1 | healthy | full then short | drain across batches, stop on short |
/// | RET-L2 | cleanup unavailable | error | stop; Q unchanged; live auth still uses repo |
/// | RET-L3 | recovered | full then short | next tick resumes the same Q |
/// | RET-L4 | healthy | full forever | stop at the per-tick hard cap and yield |
/// | RET-L5 | corrupt adapter | count > requested | stop; never exceed hard cap |
///
/// This proves bounded work, retry, and no loss. It deliberately makes no
/// global backlog bound: Hosted admission is partitioned per organization and
/// the number of active organizations is not bounded by Open.
#[tokio::test]
async fn retention_drain_is_bounded_and_recovers_durable_backlog() {
    let repository = Arc::new(SqliteApplicationAccessRepository::open_in_memory().unwrap());
    let first_hash = ApplicationAccessTokenHash::from_presented("retention-backlog-0");
    for index in 0..=crate::coordinator_component::APPLICATION_ACCESS_RETENTION_BATCH {
        repository
            .create(record_at(
                ApplicationAccessTokenId::new(),
                &format!("retention-backlog-{index}"),
                1,
                2,
            ))
            .await
            .expect("seed retention backlog");
    }
    let live_token = "retention-live-authority"; // awaken-allow: secret -- inert test fixture
    repository
        .create(record(ApplicationAccessTokenId::new(), live_token))
        .await
        .expect("seed live authorization row");
    let inner: Arc<dyn ApplicationAccessRepository> = repository.clone();
    let probe = Arc::new(RetentionProbeRepository::over(inner, true));
    let store = ApplicationAccessStore::over_shared(probe.clone());

    assert!(
        matches!(
            crate::coordinator_component::drain_application_access_retention(&store, 500).await,
            Err(ApplicationAccessRepositoryError::Unavailable(_))
        ),
        "RET-L2"
    );
    assert_eq!(probe.delete_calls.load(Ordering::SeqCst), 1, "RET-L2");
    assert!(
        repository.find_by_token_hash(&first_hash).await.is_ok(),
        "RET-L2 durable backlog is unchanged"
    );
    assert!(
        store.authenticate(live_token).await.is_ok(),
        "RET-L2 cleanup failure does not install or require an auth fallback"
    );

    assert_eq!(
        crate::coordinator_component::drain_application_access_retention(&store, 500).await,
        Ok(u64::from(
            crate::coordinator_component::APPLICATION_ACCESS_RETENTION_BATCH + 1
        )),
        "RET-L1/RET-L3"
    );
    assert_eq!(probe.delete_calls.load(Ordering::SeqCst), 3, "RET-L1");
    assert_eq!(
        repository.find_by_token_hash(&first_hash).await,
        Err(ApplicationAccessRepositoryError::NotFound),
        "RET-L3"
    );

    let cap_probe = Arc::new(RetentionProbeRepository::always_full());
    let cap_store = ApplicationAccessStore::over_shared(cap_probe.clone());
    assert_eq!(
        crate::coordinator_component::drain_application_access_retention(&cap_store, 500).await,
        Ok(u64::from(
            crate::coordinator_component::APPLICATION_ACCESS_RETENTION_CAPACITY_PER_TICK
        )),
        "RET-L4"
    );
    assert_eq!(
        cap_probe.delete_calls.load(Ordering::SeqCst),
        crate::coordinator_component::APPLICATION_ACCESS_RETENTION_MAX_BATCHES_PER_TICK as usize,
        "RET-L4"
    );

    let corrupt_probe = Arc::new(RetentionProbeRepository::over_reporting());
    let corrupt_store = ApplicationAccessStore::over_shared(corrupt_probe.clone());
    assert!(
        matches!(
            crate::coordinator_component::drain_application_access_retention(&corrupt_store, 500)
                .await,
            Err(ApplicationAccessRepositoryError::Corrupt(_))
        ),
        "RET-L5"
    );
    assert_eq!(
        corrupt_probe.delete_calls.load(Ordering::SeqCst),
        1,
        "RET-L5"
    );
}

#[tokio::test]
async fn durable_store_survives_restart_and_persists_no_cleartext() {
    /* Store/authentication decision table:
     * AAT-R7 wrong/unknown, expired, or revoked -> Invalid;
     * AAT-R8 cleartext mint result -> only its hash persists;
     * AAT-R9 revoke through replica B -> replica A immediately rejects;
     * AAT-R10 unknown durable grant fields -> Corrupt, never compatibility-read;
     * AAT-R11 JSON-valid but semantically invalid grant -> mint rejects Invalid,
     * and durable authentication rejects Corrupt before the PEP can dispatch.
     * Constraint K1: ApplicationGrant is the sole semantic validator shared by
     * mint and restore; the repository does not define a second grant schema.
     */
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("sessions.db");
    let path_text = path.to_string_lossy();
    let first = ApplicationAccessStore::open_sqlite(&path_text)
        .await
        .unwrap();
    let now = now_unix_millis();
    let issued = first
        .mint("workspace-a".into(), now, now + 60_000, grant())
        .await
        .expect("mint durable credential");
    assert_eq!(
        Uuid::parse_str(issued.id.trim_start_matches(TOKEN_ID_PREFIX))
            .unwrap()
            .get_version(),
        Some(Version::SortRand),
        "AAT-R6"
    );
    drop(first);

    let restarted = Arc::new(
        ApplicationAccessStore::open_sqlite(&path_text)
            .await
            .unwrap(),
    );
    let identity = restarted
        .authenticate(&issued.access_token)
        .await
        .expect("AAT-R6 survives restart");
    assert_eq!(identity.workspace_id, "workspace-a", "AAT-R6");
    assert_eq!(
        restarted.authenticate("wrong-token").await,
        Err(ApplicationAuthenticationError::Invalid),
        "AAT-R7"
    );

    let raw = rusqlite::Connection::open(&path).unwrap();
    let (stored_hash, stored_grant): (String, String) = raw
        .query_row(
            "SELECT token_hash, grant_json FROM application_access_credential WHERE token_id = ?1",
            [&issued.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        stored_hash,
        hash_session_token(&issued.access_token),
        "AAT-R8"
    );
    assert_ne!(stored_hash, issued.access_token, "AAT-R8");
    assert!(!stored_grant.contains(&issued.access_token), "AAT-R8");
    let stored_grant: serde_json::Value = serde_json::from_str(&stored_grant).unwrap();
    assert_eq!(
        stored_grant
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from(["operations", "protocols", "thread_bindings"]),
        "AAT-R8 stores only PEP-consumed grant fields"
    );

    let peer = ApplicationAccessStore::open_sqlite(&path_text)
        .await
        .unwrap();
    peer.revoke("workspace-a", &issued.id, now + 1)
        .await
        .expect("AAT-R9");
    assert_eq!(
        restarted.authenticate(&issued.access_token).await,
        Err(ApplicationAuthenticationError::Invalid),
        "AAT-R9"
    );

    let mut legacy_grant = serde_json::to_value(grant()).unwrap();
    legacy_grant
        .as_object_mut()
        .unwrap()
        .insert("authority_id".into(), serde_json::json!("dead-authority"));
    raw.execute(
        "UPDATE application_access_credential
         SET revoked_at_unix_ms = NULL, grant_json = ?2
         WHERE token_id = ?1",
        rusqlite::params![&issued.id, legacy_grant.to_string()],
    )
    .unwrap();
    assert_eq!(
        restarted.authenticate(&issued.access_token).await,
        Err(ApplicationAuthenticationError::Corrupt),
        "AAT-R10 legacy dead grant fields are corrupt, never a compatibility read"
    );

    let mut invalid_grant = grant();
    invalid_grant.thread_bindings.insert(
        "second-external-thread".into(),
        ApplicationThreadBinding {
            managed_session_id: "sesn_existing".into(),
            agent_id: "support".into(),
        },
    );
    assert!(
        matches!(
            restarted
                .mint(
                    "workspace-a".into(),
                    now,
                    now + 60_000,
                    invalid_grant.clone(),
                )
                .await,
            Err(ApplicationAccessRepositoryError::Invalid(_))
        ),
        "AAT-R11 mint rejects a non-bijective semantic grant"
    );
    raw.execute(
        "UPDATE application_access_credential SET grant_json = ?2 WHERE token_id = ?1",
        rusqlite::params![&issued.id, serde_json::to_string(&invalid_grant).unwrap()],
    )
    .unwrap();
    assert_eq!(
        restarted.authenticate(&issued.access_token).await,
        Err(ApplicationAuthenticationError::Corrupt),
        "AAT-R11 JSON-valid semantic corruption fails closed"
    );
}

/// Authentication failure cause/effect table. C1 the exact durable record is
/// expired; C2 the authority repository is unavailable. Effects are E1
/// caller-safe `Invalid` and E2 `Unavailable` for both authentication and
/// retention, with no fallback installed. Rules: AUTH-F1 C1+!C2 -> E1;
/// AUTH-F2 C2 -> E2 before any credential interpretation.
#[tokio::test]
async fn expired_and_unavailable_authorities_remain_distinct() {
    let repository = Arc::new(SqliteApplicationAccessRepository::open_in_memory().unwrap());
    let token = "expired-token"; // awaken-allow: secret -- inert test fixture
    repository
        .create(
            ApplicationAccessRecord::new(
                ApplicationAccessTokenId::new(),
                ApplicationAccessTokenHash::from_presented(token),
                "workspace-a".into(),
                1,
                2,
                grant(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let expired = ApplicationAccessStore::over_shared(repository);
    assert_eq!(
        expired.authenticate(token).await,
        Err(ApplicationAuthenticationError::Invalid),
        "AAT-R7"
    );

    let unavailable = ApplicationAccessStore::failing_for_test(
        ApplicationAccessRepositoryError::Unavailable("offline".into()),
    );
    assert_eq!(
        unavailable.authenticate("any-token").await,
        Err(ApplicationAuthenticationError::Unavailable),
        "PEP-R11"
    );
    assert!(
        matches!(
            unavailable.delete_terminal_before(500, 1).await,
            Err(ApplicationAccessRepositoryError::Unavailable(_))
        ),
        "RET-R5 cleanup failure preserves the same unavailable authority; no fallback"
    );
    assert_eq!(
        unavailable.authenticate("any-token").await,
        Err(ApplicationAuthenticationError::Unavailable),
        "RET-R5"
    );
}

async fn run_postgres_repository_conformance(url: &str) {
    use sqlx::Executor;
    use sqlx::postgres::{PgPool, PgPoolOptions};

    async fn schema_pool(url: &str) -> PgPool {
        PgPoolOptions::new()
            .after_connect(|connection, _| {
                Box::pin(async move {
                    connection
                        .execute("SET search_path = t_application_access")
                        .await?;
                    Ok(())
                })
            })
            .connect(url)
            .await
            .expect("connect application access schema")
    }

    let admin = PgPool::connect(url)
        .await
        .expect("connect provisioned application access Postgres");
    let _ = admin
        .execute("DROP SCHEMA IF EXISTS t_application_access CASCADE")
        .await;
    admin
        .execute("CREATE SCHEMA t_application_access")
        .await
        .expect("create application access schema");
    admin.close().await;

    /* Migration cause/effect graph:
     * C1 the scoped schema is fresh or migrated; C2 the operation is verify-only
     * or migration-only; C3 migration runs once or is repeated. Effects are E1
     * fresh verify fails without creating the ledger, E2 migration installs the
     * one canonical bundle, E3 repeated migration is idempotent, and E4 two
     * runtime repositories open through verification only.
     *
     * | Rule | schema | operation | repeat | Effect |
     * |---|---|---|---|---|
     * | PG-M1 | fresh | verify | no | E1 error; zero startup DDL |
     * | PG-M2 | fresh | migrate | no | E2 bundle installed |
     * | PG-M3 | migrated | migrate | yes | E3 success; no second schema owner |
     * | PG-M4 | migrated | verify | no | E4 both runtime repositories open |
     */
    assert!(
        PostgresApplicationAccessRepository::with_existing_pool(schema_pool(url).await)
            .await
            .is_err(),
        "PG-M1"
    );
    let migration_pool = schema_pool(url).await;
    PostgresApplicationAccessRepository::migrate_pool(&migration_pool)
        .await
        .expect("PG-M2 migration-only installs application access bundle");
    migration_pool.close().await;
    let repeated_migration_pool = schema_pool(url).await;
    PostgresApplicationAccessRepository::migrate_pool(&repeated_migration_pool)
        .await
        .expect("PG-M3 repeated migration is idempotent");
    repeated_migration_pool.close().await;
    let first = PostgresApplicationAccessRepository::with_existing_pool(schema_pool(url).await)
        .await
        .expect("PG-M4 first verified Postgres repository");
    let second = PostgresApplicationAccessRepository::with_existing_pool(schema_pool(url).await)
        .await
        .expect("PG-M4 second verified Postgres repository");
    repository_decision_table(&first, &second).await;

    /* Hosted store-level cause/effect graph:
     * C1 the migration bundle is installed; C2 two Coordinator store instances
     * use that exact PostgreSQL schema; C3 origin mints one live credential;
     * C4 the peer instance is dropped and rebuilt through schema verification;
     * C5 the rebuilt peer revokes the exact management id. Effects are E1 peer
     * authentication resolves the exact Workspace/grant, E2 restart preserves
     * that identity, and E3 revocation is immediately rejected by the still-live
     * origin store.
     *
     * | Rule | C1 | C2 | C3 | C4 | C5 | Effect |
     * |---|---|---|---|---|---|---|
     * | PG-S1 | T | T | T | F | F | E1 exact peer authentication |
     * | PG-S2 | T | T | T | T | F | E2 exact authentication after reconnect |
     * | PG-S3 | T | T | T | T | T | E3 origin rejects the revoked token |
     *
     * The adapter decision table above remains the schema/repository authority;
     * these rules prove that the production store composition does not turn its
     * durable PostgreSQL behavior into a test-only synthetic-record claim.
     */
    let origin = ApplicationAccessStore::over(first);
    let peer = ApplicationAccessStore::over(second);
    let expected_grant = grant();
    let now = now_unix_millis();
    let issued = origin
        .mint(
            "workspace-a".into(),
            now,
            now.saturating_add(60_000),
            expected_grant.clone(),
        )
        .await
        .expect("PG-S1 origin mint");

    let peer_identity = peer
        .authenticate(&issued.access_token)
        .await
        .expect("PG-S1 peer authenticate");
    assert_eq!(peer_identity.workspace_id, "workspace-a", "PG-S1");
    assert_eq!(peer_identity.grant, expected_grant, "PG-S1");

    drop(peer);
    let restarted_peer = ApplicationAccessStore::over(
        PostgresApplicationAccessRepository::with_existing_pool(schema_pool(url).await)
            .await
            .expect("PG-S2 reconnect peer through schema verification"),
    );
    let restarted_identity = restarted_peer
        .authenticate(&issued.access_token)
        .await
        .expect("PG-S2 authenticate after peer restart");
    assert_eq!(restarted_identity.workspace_id, "workspace-a", "PG-S2");
    assert_eq!(restarted_identity.grant, expected_grant, "PG-S2");

    restarted_peer
        .revoke("workspace-a", &issued.id, now.saturating_add(1))
        .await
        .expect("PG-S3 peer revoke");
    assert_eq!(
        origin.authenticate(&issued.access_token).await,
        Err(ApplicationAuthenticationError::Invalid),
        "PG-S3 origin observes peer revocation"
    );
}

/// Developer-probe coverage rules: C1 Postgres is reachable -> PG-D1 runs the
/// complete migration/repository/store decision tables in
/// `run_postgres_repository_conformance`; C2 it is unreachable -> PG-D2 skips
/// only this convenience probe. The adjacent ignored release gate owns the
/// fail-closed C2 outcome, so this wrapper does not duplicate its contract.
#[tokio::test]
async fn postgres_repository_conforms_when_available() {
    use sqlx::postgres::PgPool;

    let url = std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_string()
    });
    let Ok(probe) = PgPool::connect(&url).await else {
        println!("[skip] no Postgres reachable; release gate is fail-closed");
        return;
    };
    probe.close().await;
    run_postgres_repository_conformance(&url).await;
}

/// Fail-closed Hosted durability gate. Unlike the developer convenience test,
/// P1 missing URL, P2 unreachable Postgres, or P3 any conformance failure all
/// fail this test; only P4 a provisioned backend passing the shared two-instance
/// decision table succeeds. `scripts/ci/pg_tests.sh --require-docker` is the
/// single owner that provisions the URL and invokes this ignored gate exactly.
#[tokio::test]
#[ignore = "requires Postgres provisioned by scripts/ci/pg_tests.sh"]
async fn provisioned_postgres_application_access_durability_release_gate() {
    let url = std::env::var("AWAKEN_TEST_DATABASE_URL")
        .expect("release gate requires AWAKEN_TEST_DATABASE_URL");
    run_postgres_repository_conformance(&url).await;
}
