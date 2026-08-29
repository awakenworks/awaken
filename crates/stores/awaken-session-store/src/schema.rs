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

/// V1 is the published current baseline. Later additions keep its checksum
/// immutable and advance through the same ledger-owned migration stream.
pub(crate) fn session_bundle() -> Result<MigrationBundle, MigrationError> {
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

pub(crate) fn selected_session_bundle(
    receipts: &BTreeMap<i64, String>,
) -> Result<(PublishedSessionStream, MigrationBundle), MigrationError> {
    let stream = match receipts.get(&1).map(String::as_str) {
        Some(expanded::V1_CHECKSUM) => match receipts.get(&15).map(String::as_str) {
            Some(expanded::COMPACTED_V15_CHECKSUM) => PublishedSessionStream::Compacted,
            _ => PublishedSessionStream::Original,
        },
        _ => PublishedSessionStream::Compact,
    };
    let bundle = match stream {
        PublishedSessionStream::Compact => session_bundle(),
        PublishedSessionStream::Original => expanded::bundle(expanded::ExpandedStream::Original),
        PublishedSessionStream::Compacted => expanded::bundle(expanded::ExpandedStream::Compacted),
    }?;
    Ok((stream, bundle))
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

#[cfg(test)]
pub(crate) fn original_published_session_bundle() -> Result<MigrationBundle, MigrationError> {
    expanded::published_bundle(expanded::ExpandedStream::Original)
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_scoped_migration::{Dialect, MigrationError, plan};

    #[test]
    fn session_schema_preserves_v1_and_adds_one_deterministic_dependency_index() {
        // Migration cause/effect table: C1 published V1 receipt exists -> E1 its
        // checksum remains accepted; C2 V2 is absent -> E2 apply only additive
        // source-index DDL; C3 V2 is present -> E3 ledger replay is a no-op.
        // M1=C1+C2=>E1+E2, M2=C1+C3=>E1+E3. Backfill is deliberately owned by
        // repository startup because only the canonical Rust root decoder can
        // derive desired credential dependencies without a parallel JSON model.
        let bundle = session_bundle().expect("session bundle");
        assert_eq!(
            bundle
                .migrations()
                .iter()
                .map(awaken_scoped_migration::Migration::version)
                .collect::<Vec<_>>(),
            [1, 2]
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
    }

    #[test]
    fn all_published_histories_select_exactly_and_converge_once() {
        // Causes: H1 empty/current compact V1; H2 original V1 with no/old V15;
        // H3 original V1 plus compacted V15; H4 unknown V1/V15. Effects: E1
        // compact V1/V2; E2 original V1..V23; E3 compacted V1..V21; E4 ordinary
        // checksum failure; E5 one shared future append stream.
        // Rules: H1=>E1+E5; H2=>E2+E5; H3=>E3+E5; H4=>E4.
        let compact = session_bundle().unwrap();
        let compact_receipts =
            BTreeMap::from([(1, compact.migrations()[0].checksum_for(Dialect::Sqlite))]);
        assert_eq!(
            selected_session_bundle(&compact_receipts).unwrap().0,
            PublishedSessionStream::Compact,
            "H1"
        );
        let original_receipts = BTreeMap::from([(1, expanded::V1_CHECKSUM.to_owned())]);
        let (stream, original) = selected_session_bundle(&original_receipts).unwrap();
        assert_eq!(stream, PublishedSessionStream::Original, "H2");
        assert_eq!(original.migrations().last().unwrap().version(), 23, "E2");
        let compacted_receipts = BTreeMap::from([
            (1, expanded::V1_CHECKSUM.to_owned()),
            (15, expanded::COMPACTED_V15_CHECKSUM.to_owned()),
        ]);
        let (stream, compacted) = selected_session_bundle(&compacted_receipts).unwrap();
        assert_eq!(stream, PublishedSessionStream::Compacted, "H3");
        assert_eq!(compacted.migrations().last().unwrap().version(), 21, "E3");
        let unknown_v1 = BTreeMap::from([(1, "f".repeat(64))]);
        assert!(matches!(
            plan(
                &selected_session_bundle(&unknown_v1).unwrap().1,
                &unknown_v1,
                Dialect::Sqlite,
            ),
            Err(MigrationError::ChecksumMismatch { version: 1, .. })
        ));
        let unknown_v15 =
            BTreeMap::from([(1, expanded::V1_CHECKSUM.to_owned()), (15, "f".repeat(64))]);
        assert!(matches!(
            plan(
                &selected_session_bundle(&unknown_v15).unwrap().1,
                &unknown_v15,
                Dialect::Sqlite,
            ),
            Err(MigrationError::ChecksumMismatch { version: 15, .. })
        ));
        awaken_scoped_migration::lint(std::slice::from_ref(&converged_session_bundle().unwrap()))
            .expect("E5");
    }
}
