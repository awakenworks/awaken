// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: This source implements the repository-controlled store product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Deterministic current schema authority shared by SQLite and PostgreSQL.

mod expanded;

use std::collections::BTreeMap;

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

pub(crate) const BUNDLE_ID: &str = "awaken.managed_session";
pub(crate) const CONVERGED_BUNDLE_ID: &str = "awaken.managed_session.converged";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PublishedSessionStream {
    Compact,
    Original,
    Compacted,
}

impl PublishedSessionStream {
    pub(crate) const fn is_legacy(self) -> bool {
        !matches!(self, Self::Compact)
    }
}

/// The one selected migration lineage and its legacy normalization boundary.
///
/// A legacy database must first reach its last historically published shape so
/// every column consumed by aggregate normalization exists. Only then may the
/// branch-local convergence migration remove the retired columns. Keeping both
/// bundles in this one selection result prevents SQLite and PostgreSQL openers
/// from inventing their own version cutoffs or supported-history tables.
pub(crate) struct SelectedSessionSchema {
    pub(crate) stream: PublishedSessionStream,
    pub(crate) pre_convergence: Option<MigrationBundle>,
    pub(crate) complete: MigrationBundle,
}

/// V1 is the published current baseline. Later additions keep its checksum
/// immutable and advance through the same ledger-owned migration stream.
#[cfg(test)]
pub(crate) fn session_bundle() -> Result<MigrationBundle, MigrationError> {
    append_current_tail(compact_published_bundle()?)
}

fn compact_published_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        BUNDLE_ID,
        vec![
            Migration::new(
                1,
                "current managed Session and adjacent process authorities",
                "CREATE TABLE {prefix}_session (\
                 session_id TEXT PRIMARY KEY CHECK (length(session_id) > 0), \
                 scope_id TEXT NOT NULL CHECK (length(scope_id) > 0), \
                 revision BIGINT NOT NULL CHECK (revision > 0), \
                 aggregate_json TEXT NOT NULL); \
             CREATE INDEX {prefix}_session_scope_idx \
                 ON {prefix}_session (scope_id, session_id); \
             CREATE TABLE {prefix}_lifecycle_outbox (\
                 fact_id TEXT PRIMARY KEY, \
                 data TEXT NOT NULL, \
                 created_at {timestamptz} NOT NULL DEFAULT {now}); \
             CREATE TABLE {prefix}_memory_extraction (\
                 intent_id TEXT PRIMARY KEY, \
                 idempotency_key TEXT NOT NULL UNIQUE, \
                 status TEXT NOT NULL, \
                 revision BIGINT NOT NULL CHECK (revision >= 0), \
                 lease_expires_at_unix_ms BIGINT \
                     CHECK (lease_expires_at_unix_ms IS NULL OR lease_expires_at_unix_ms >= 0), \
                 data TEXT NOT NULL, \
                 created_at {timestamptz} NOT NULL DEFAULT {now}); \
             CREATE TABLE {prefix}_session_idempotency (\
                 session_id TEXT NOT NULL, \
                 idempotency_key TEXT NOT NULL, \
                 payload_hash TEXT NOT NULL, \
                 committed_revision BIGINT NOT NULL CHECK (committed_revision > 0), \
                 PRIMARY KEY (session_id, idempotency_key)); \
             CREATE TABLE {prefix}_session_tombstone (\
                 session_id TEXT PRIMARY KEY, \
                 scope_id TEXT NOT NULL, \
                 deleted_revision BIGINT NOT NULL CHECK (deleted_revision > 0), \
                 deleted_at TEXT NOT NULL); \
             CREATE TABLE {prefix}_session_quarantine (\
                 session_id TEXT PRIMARY KEY \
                     REFERENCES {prefix}_session(session_id) ON DELETE CASCADE, \
                 reason TEXT NOT NULL, \
                 observed_revision BIGINT NOT NULL CHECK (observed_revision > 0), \
                 quarantined_at {timestamptz} NOT NULL DEFAULT {now}); \
             CREATE TABLE {prefix}_session_reconciliation_work (\
                 session_id TEXT PRIMARY KEY \
                     REFERENCES {prefix}_session(session_id) ON DELETE CASCADE, \
                 observed_revision BIGINT NOT NULL CHECK (observed_revision > 0)); \
             CREATE TABLE {prefix}_session_vault_reference (\
                 session_id TEXT NOT NULL \
                     REFERENCES {prefix}_session(session_id) ON DELETE CASCADE, \
                 vault_id TEXT NOT NULL, \
                 PRIMARY KEY (session_id, vault_id)); \
             CREATE INDEX {prefix}_session_vault_reference_lookup_idx \
                 ON {prefix}_session_vault_reference (vault_id, session_id); \
             CREATE TABLE {prefix}_dream (\
                 job_id TEXT PRIMARY KEY, data TEXT NOT NULL); \
             CREATE TABLE {prefix}_deployment (\
                 deployment_id TEXT PRIMARY KEY, \
                 workspace_id TEXT NOT NULL, \
                 data TEXT NOT NULL, \
                 revision BIGINT NOT NULL DEFAULT 0 CHECK (revision >= 0)); \
             CREATE INDEX {prefix}_deployment_workspace_idx \
                 ON {prefix}_deployment (workspace_id, deployment_id); \
             CREATE TABLE {prefix}_deployment_run (\
                 run_id TEXT PRIMARY KEY, \
                 deployment_id TEXT NOT NULL \
                     REFERENCES {prefix}_deployment(deployment_id), \
                 workspace_id TEXT NOT NULL, \
                 data TEXT NOT NULL); \
             CREATE INDEX {prefix}_deployment_run_deployment_idx \
                 ON {prefix}_deployment_run (deployment_id, run_id); \
             CREATE TABLE {prefix}_deployment_claim (\
                 claim_id TEXT PRIMARY KEY, \
                 run_id TEXT NOT NULL UNIQUE \
                     REFERENCES {prefix}_deployment_run(run_id), \
                 created_at {timestamptz} NOT NULL DEFAULT {now}); \
             CREATE TABLE {prefix}_dream_policy (\
                 workspace_id TEXT NOT NULL, \
                 memory_store_id TEXT NOT NULL, \
                 data TEXT NOT NULL, \
                 PRIMARY KEY (workspace_id, memory_store_id))",
            )?,
            Migration::new(
                2,
                "current desired MCP credential-source dependency index",
                "CREATE TABLE {prefix}_session_credential_source_reference (\
                     session_id TEXT NOT NULL \
                         REFERENCES {prefix}_session(session_id) ON DELETE CASCADE, \
                     credential_source_id TEXT NOT NULL, \
                     PRIMARY KEY (session_id, credential_source_id)); \
                 CREATE INDEX {prefix}_session_credential_source_reference_lookup_idx \
                     ON {prefix}_session_credential_source_reference \
                        (credential_source_id, session_id)",
            )?,
        ],
    )
}

pub(crate) fn selected_session_schema(
    receipts: &BTreeMap<i64, String>,
) -> Result<SelectedSessionSchema, MigrationError> {
    let stream = match receipts.get(&1).map(String::as_str) {
        Some(expanded::V1_CHECKSUM) => match receipts.get(&15).map(String::as_str) {
            Some(expanded::COMPACTED_V15_CHECKSUM) => PublishedSessionStream::Compacted,
            _ => PublishedSessionStream::Original,
        },
        _ => PublishedSessionStream::Compact,
    };
    let (published, complete) = match stream {
        PublishedSessionStream::Compact => (None, compact_published_bundle()?),
        PublishedSessionStream::Original => (
            Some(expanded::published_bundle(
                expanded::ExpandedStream::Original,
            )?),
            expanded::bundle(expanded::ExpandedStream::Original)?,
        ),
        PublishedSessionStream::Compacted => (
            Some(expanded::published_bundle(
                expanded::ExpandedStream::Compacted,
            )?),
            expanded::bundle(expanded::ExpandedStream::Compacted)?,
        ),
    };
    // A fully converged legacy ledger must not be replayed against the shorter
    // historical bundle: its valid convergence receipt would look "too new" to
    // that bundle. Derive completion from the selected bundle's final migration,
    // never from an adapter-owned version/checksum table.
    let pre_convergence = published.and_then(|published| {
        let convergence_version = complete
            .migrations()
            .last()
            .expect("every Session bundle has a migration")
            .version();
        (!receipts.contains_key(&convergence_version)).then_some(published)
    });
    Ok(SelectedSessionSchema {
        stream,
        pre_convergence,
        complete: append_current_tail(complete)?,
    })
}

pub(crate) fn converged_session_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        CONVERGED_BUNDLE_ID,
        vec![Migration::new(
            1,
            "seal the converged managed Session migration history",
            "SELECT 1",
        )?],
    )
}

/// One shared DDL tail, appended within the table-owning selected lineage.
/// The historical convergence marker owns no tables and must not ALTER them.
/// Preserve its receipt and every historical checksum; never fork future DDL.
fn append_current_tail(bundle: MigrationBundle) -> Result<MigrationBundle, MigrationError> {
    let mut migrations = bundle.migrations().to_vec();
    let next = migrations
        .last()
        .expect("published Session schema is nonempty")
        .version()
        + 1;
    migrations.push(Migration::new(
        next,
        "retain minimal event admission evidence at Session deletion",
        "ALTER TABLE {prefix}_session_tombstone ADD COLUMN event_admissions_json TEXT",
    )?);
    migrations.push(Migration::new(
        next + 1,
        "index root-owned Worker capacity and Run-free preparation obligations",
        "ALTER TABLE {prefix}_session_reconciliation_work \
             ADD COLUMN worker_owner TEXT; \
         ALTER TABLE {prefix}_session_reconciliation_work \
             ADD COLUMN preparation_candidate INTEGER NOT NULL DEFAULT 0 \
             CHECK (preparation_candidate IN (0, 1)); \
         ALTER TABLE {prefix}_session_reconciliation_work \
             ADD COLUMN unbound_preparation INTEGER NOT NULL DEFAULT 0 \
             CHECK (unbound_preparation IN (0, 1)); \
         ALTER TABLE {prefix}_session_reconciliation_work \
             ADD COLUMN unbound_terminal INTEGER NOT NULL DEFAULT 0 \
             CHECK (unbound_terminal IN (0, 1)); \
         ALTER TABLE {prefix}_session_reconciliation_work \
             ADD COLUMN demand_invalid INTEGER NOT NULL DEFAULT 0 \
             CHECK (demand_invalid IN (0, 1)); \
         CREATE INDEX {prefix}_session_preparation_candidate_idx \
             ON {prefix}_session_reconciliation_work (session_id) \
             WHERE preparation_candidate = 1; \
         CREATE INDEX {prefix}_session_worker_owner_idx \
             ON {prefix}_session_reconciliation_work (worker_owner) \
             WHERE worker_owner IS NOT NULL",
    )?);
    migrations.push(Migration::new(
        next + 2,
        "record pre-root creation intents for prepared Session participants",
        "CREATE TABLE {prefix}_session_creation_intent (\
             session_id TEXT PRIMARY KEY CHECK (length(session_id) > 0), \
             scope_id TEXT NOT NULL CHECK (length(scope_id) > 0), \
             phase TEXT NOT NULL CHECK (phase IN ('preparing', 'aborting')), \
             repository_ids_json TEXT NOT NULL, \
             lease_expires_at_unix_ms BIGINT NOT NULL \
                 CHECK (lease_expires_at_unix_ms >= 0)); \
         CREATE INDEX {prefix}_session_creation_intent_recoverable_idx \
             ON {prefix}_session_creation_intent (lease_expires_at_unix_ms, session_id)",
    )?);
    migrations.push(Migration::new(
        next + 3,
        "retain Event no-op outcomes in the existing command receipt index",
        "ALTER TABLE {prefix}_session_idempotency \
             ADD COLUMN receipt_kind TEXT NOT NULL DEFAULT 'root' \
             CHECK (receipt_kind IN ('root', 'event_batch', 'event_no_op'))",
    )?);
    migrations.push(Migration::new(
        next + 4,
        "retain completed creation abort identities",
        "CREATE TABLE {prefix}_session_creation_intent_retained (\
             session_id TEXT PRIMARY KEY CHECK (length(session_id) > 0), \
             scope_id TEXT NOT NULL CHECK (length(scope_id) > 0), \
             phase TEXT NOT NULL CHECK (phase IN ('preparing', 'aborting', 'aborted')), \
             repository_ids_json TEXT NOT NULL, \
             lease_expires_at_unix_ms BIGINT NOT NULL \
                 CHECK (lease_expires_at_unix_ms >= 0)); \
         INSERT INTO {prefix}_session_creation_intent_retained \
             (session_id, scope_id, phase, repository_ids_json, lease_expires_at_unix_ms) \
             SELECT session_id, scope_id, phase, repository_ids_json, lease_expires_at_unix_ms \
             FROM {prefix}_session_creation_intent; \
         DROP TABLE {prefix}_session_creation_intent; \
         ALTER TABLE {prefix}_session_creation_intent_retained \
             RENAME TO {prefix}_session_creation_intent; \
         CREATE INDEX {prefix}_session_creation_intent_recoverable_idx \
             ON {prefix}_session_creation_intent (lease_expires_at_unix_ms, session_id)",
    )?);
    migrations.push(Migration::new(
        next + 5,
        "retain private content visibility at Session deletion",
        "ALTER TABLE {prefix}_session_tombstone \
             ADD COLUMN private_content INTEGER \
             CHECK (private_content IS NULL OR private_content IN (0, 1))",
    )?);
    MigrationBundle::new(bundle.bundle_id(), migrations)
}

#[cfg(test)]
pub(crate) fn original_published_session_bundle() -> Result<MigrationBundle, MigrationError> {
    expanded::published_bundle(expanded::ExpandedStream::Original)
}

#[cfg(test)]
pub(crate) fn compacted_published_session_bundle() -> Result<MigrationBundle, MigrationError> {
    expanded::published_bundle(expanded::ExpandedStream::Compacted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_scoped_migration::{Dialect, MigrationError, plan};

    #[test]
    fn session_schema_preserves_v1_and_adds_owned_dependency_and_worker_indexes() {
        // Causes: C1 published V1 receipt; C2 missing additive V2/V3/V4/V5;
        // C3 current receipts. Effects: E1 V1 checksum remains accepted;
        // E2 deterministic source, tombstone, Worker and typed-receipt DDL tails;
        // E3 current ledger replay is a no-op. M1=C1+C2=>E1+E2;
        // M2=C1+C3=>E1+E3. Backfill belongs to repository startup, whose
        // canonical Rust root decoder owns derived index truth.
        let bundle = session_bundle().expect("session bundle");
        assert_eq!(
            bundle
                .migrations()
                .iter()
                .map(awaken_scoped_migration::Migration::version)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4, 5, 6, 7, 8]
        );
        let sql = bundle.migrations()[0].sql_for(awaken_scoped_migration::Dialect::Sqlite);
        for retired in [
            "agent_id",
            "metadata_json",
            "environment_id",
            "mcp_json",
            "effective_inputs_json",
            "runtime_json",
        ] {
            assert!(!sql.contains(retired), "retired Session column {retired}");
        }
        assert!(sql.contains("session_reconciliation_work"));
        assert!(sql.contains("session_vault_reference"));
        assert!(!sql.contains("session_credential_source_reference"));
        assert!(
            bundle.migrations()[1]
                .sql_for(awaken_scoped_migration::Dialect::Sqlite)
                .contains("session_credential_source_reference")
        );
        let worker_index_sql =
            bundle.migrations()[3].sql_for(awaken_scoped_migration::Dialect::Sqlite);
        assert!(worker_index_sql.contains("worker_owner"));
        assert!(worker_index_sql.contains("preparation_candidate"));
        let receipt_sql = bundle.migrations()[5].sql_for(Dialect::Sqlite);
        assert!(receipt_sql.contains("receipt_kind TEXT NOT NULL DEFAULT 'root'"));
        assert!(receipt_sql.contains("'event_batch', 'event_no_op'"));
        for dialect in [Dialect::Sqlite, Dialect::Postgres] {
            let close_sql = bundle.migrations()[6].sql_for(dialect);
            assert!(close_sql.contains("'preparing', 'aborting', 'aborted'"));
            assert!(close_sql.contains(
                "SELECT session_id, scope_id, phase, repository_ids_json, lease_expires_at_unix_ms"
            ));
        }
    }

    async fn assert_upgraded_creation_intents(
        store: &dyn awaken_session_application::SessionStore,
    ) {
        // Shared DT-U oracle: V6 Preparing/Aborting rows survive V7 with exact
        // id/scope/phase/participants/lease. Completion then retains the local
        // abort fence; the backend-specific wrappers own the real upgrade.
        use awaken_session_contract::PreparedParticipantIntentPhase;
        let intents = store.recoverable_creation_intents(2000, 50).await.unwrap();
        assert_eq!(intents.len(), 2);
        for intent in &intents {
            assert_eq!(intent.owner_scope(), "original-workspace");
            assert_eq!(intent.repository_ids(), &["original-repository"]);
            let expected = match intent.session_id().as_str() {
                "preparing-before-upgrade" => (PreparedParticipantIntentPhase::Preparing, 1000),
                "aborting-before-upgrade" => (PreparedParticipantIntentPhase::Aborting, 900),
                other => panic!("foreign upgraded intent {other}"),
            };
            assert_eq!(
                (intent.phase(), intent.lease_expires_at_unix_ms()),
                expected
            );
        }
        store
            .complete_creation_abort(&"aborting-before-upgrade".into())
            .await
            .unwrap();
    }

    async fn assert_retained_creation_abort(store: &dyn awaken_session_application::SessionStore) {
        use awaken_session_contract::{SessionRepositoryConflict, SessionRepositoryError};
        assert_eq!(
            store
                .recoverable_creation_intents(2000, 50)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(matches!(
            store
                .record_creation_intent(
                    "original-workspace",
                    &"aborting-before-upgrade".into(),
                    "original-repository",
                    3000
                )
                .await,
            Err(SessionRepositoryError::Conflict(
                SessionRepositoryConflict::Tombstoned
            ))
        ));
    }

    #[tokio::test]
    async fn retained_abort_upgrade_preserves_existing_intent_coordinates_and_replays() {
        // DT-U: C1 published V6 DB contains Preparing/Aborting intents; C2 V7
        // upgrades the SAME file; C3 complete abort and cold reopen. U1 C1+C2
        // => exact durable coordinates, historical checksums still admitted;
        // U2 C3 => closure refuses re-prepare and is not recovery work.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("intent-upgrade.db");
        let raw = crate::tests::raw_sqlite(&path).await;
        let prior = crate::tests::migration_prefix_through(&session_bundle().unwrap(), 6);
        crate::tests::migrate_fixture(&raw, crate::NS, &prior).await;
        for (id, phase, lease) in [
            ("preparing-before-upgrade", "preparing", 1000_i64),
            ("aborting-before-upgrade", "aborting", 900_i64),
        ] {
            sqlx::query("INSERT INTO managed_session_creation_intent (session_id,scope_id,phase,repository_ids_json,lease_expires_at_unix_ms) VALUES (?1,'original-workspace',?2,'[\"original-repository\"]',?3)")
                .bind(id).bind(phase).bind(lease).execute(&raw).await.unwrap();
        }
        raw.close().await;
        let store = crate::SqliteManagedSessionRepository::open(path.to_str().unwrap())
            .await
            .unwrap();
        assert_upgraded_creation_intents(&store).await;
        drop(store);
        let reopened = crate::SqliteManagedSessionRepository::open(path.to_str().unwrap())
            .await
            .unwrap();
        assert_retained_creation_abort(&reopened).await;
    }

    #[tokio::test]
    async fn postgres_retained_abort_upgrade_preserves_existing_intents_after_reconnect() {
        // DT-U PostgreSQL row: configured real V6 schema with live intents ->
        // the same portable V7 migration -> new pool on the SAME schema.
        // Uses the exact SQLite oracle, not a second upgrade interpretation.
        let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
            return;
        };
        let pool = awaken_reliability_testkit::postgres_fixture_pool(&url).await;
        let prior = crate::tests::migration_prefix_through(&session_bundle().unwrap(), 6);
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(
            pool.clone(),
            crate::NS,
        )
        .unwrap()
        .run_bundle(&prior)
        .await
        .unwrap();
        for (id, phase, lease) in [
            ("preparing-before-upgrade", "preparing", 1000_i64),
            ("aborting-before-upgrade", "aborting", 900_i64),
        ] {
            sqlx::query("INSERT INTO managed_session_creation_intent (session_id,scope_id,phase,repository_ids_json,lease_expires_at_unix_ms) VALUES ($1,'original-workspace',$2,'[\"original-repository\"]',$3)")
                .bind(id).bind(phase).bind(lease).execute(&pool).await.unwrap();
        }
        let store = crate::PostgresManagedSessionRepository::with_pool(pool)
            .await
            .unwrap();
        assert_upgraded_creation_intents(&store).await;
        drop(store);
        let reopened = crate::PostgresManagedSessionRepository::connect(&url)
            .await
            .unwrap();
        assert_retained_creation_abort(&reopened).await;
    }

    #[test]
    fn all_published_histories_select_exactly_and_converge_once() {
        // Causes: H1 empty/current compact V1; H2 original V1 with no/old V15;
        // H3 original V1 plus compacted V15; H4 fully converged legacy; H5
        // unknown V1/V15. Effects: E1
        // compact V1..V3; E2 original V1..V24; E3 compacted V1..V22; E4 ordinary
        // checksum failure; E5 one shared table-owning DDL tail; E6 skip the
        // shorter pre-convergence replay. Rules: H1=>E1+E5; H2=>E2+E5;
        // H3=>E3+E5; H4=>E6; H5=>E4.
        let compact = session_bundle().unwrap();
        let compact_receipts =
            BTreeMap::from([(1, compact.migrations()[0].checksum_for(Dialect::Sqlite))]);
        assert_eq!(
            selected_session_schema(&compact_receipts).unwrap().stream,
            PublishedSessionStream::Compact,
            "H1"
        );
        let original_receipts = BTreeMap::from([(1, expanded::V1_CHECKSUM.to_owned())]);
        let original = selected_session_schema(&original_receipts).unwrap();
        assert_eq!(original.stream, PublishedSessionStream::Original, "H2");
        assert_eq!(
            original
                .pre_convergence
                .as_ref()
                .unwrap()
                .migrations()
                .last()
                .unwrap()
                .version(),
            22,
            "H2 shared normalization boundary"
        );
        assert_eq!(
            original.complete.migrations().last().unwrap().version(),
            29,
            "E2"
        );
        let compacted_receipts = BTreeMap::from([
            (1, expanded::V1_CHECKSUM.to_owned()),
            (15, expanded::COMPACTED_V15_CHECKSUM.to_owned()),
        ]);
        let compacted = selected_session_schema(&compacted_receipts).unwrap();
        assert_eq!(compacted.stream, PublishedSessionStream::Compacted, "H3");
        assert_eq!(
            compacted
                .pre_convergence
                .as_ref()
                .unwrap()
                .migrations()
                .last()
                .unwrap()
                .version(),
            20,
            "H3 shared normalization boundary"
        );
        assert_eq!(
            compacted.complete.migrations().last().unwrap().version(),
            27,
            "E3"
        );
        let converged_original_receipts = original
            .complete
            .migrations()
            .iter()
            .map(|migration| (migration.version(), migration.checksum_for(Dialect::Sqlite)))
            .collect();
        assert!(
            selected_session_schema(&converged_original_receipts)
                .unwrap()
                .pre_convergence
                .is_none(),
            "H4/E6"
        );
        let unknown_v1 = BTreeMap::from([(1, "f".repeat(64))]);
        assert!(matches!(
            plan(
                &selected_session_schema(&unknown_v1).unwrap().complete,
                &unknown_v1,
                Dialect::Sqlite,
            ),
            Err(MigrationError::ChecksumMismatch { version: 1, .. })
        ));
        let unknown_v15 =
            BTreeMap::from([(1, expanded::V1_CHECKSUM.to_owned()), (15, "f".repeat(64))]);
        assert!(matches!(
            plan(
                &selected_session_schema(&unknown_v15).unwrap().complete,
                &unknown_v15,
                Dialect::Sqlite,
            ),
            Err(MigrationError::ChecksumMismatch { version: 15, .. })
        ));
        awaken_scoped_migration::lint(std::slice::from_ref(&converged_session_bundle().unwrap()))
            .expect("E5");
        awaken_scoped_migration::lint(std::slice::from_ref(&compact))
            .expect("E5 current table ownership");
        // The lint cannot resolve historical RENAME TO aliases. Keep those
        // immutable histories verified by real upgrade tests; compare their
        // new tail to the same linted compact authority, not copied DDL.
        for selected in [&compact, &original.complete, &compacted.complete] {
            assert_eq!(
                selected
                    .migrations()
                    .last()
                    .unwrap()
                    .sql_for(Dialect::Sqlite),
                compact
                    .migrations()
                    .last()
                    .unwrap()
                    .sql_for(Dialect::Sqlite),
                "E5 same DDL, no branch copy"
            );
        }
    }
}
