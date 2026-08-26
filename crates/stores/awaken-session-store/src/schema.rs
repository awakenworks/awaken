//! Deterministic current schema authority shared by SQLite and PostgreSQL.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

/// Fresh installations have one current baseline. The ledger, rather than
/// conditional SQL or a retained compatibility stream, is the sole apply
/// decision.
pub(crate) fn session_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        "awaken.managed_session",
        vec![Migration::new(
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
        )?],
    )
}

#[cfg(test)]
mod tests {
    use super::session_bundle;

    #[test]
    fn session_schema_is_one_deterministic_current_baseline() {
        let bundle = session_bundle().expect("session bundle");
        assert_eq!(bundle.migrations().len(), 1);
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
    }
}
