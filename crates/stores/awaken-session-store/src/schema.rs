//! Portable schema authority shared by the SQLite and PostgreSQL adapters.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

/// The versioned schema bundle (ADR-0043 scoped migration). All columns are
/// portable so both adapters read and write identical aggregate rows.
pub(crate) fn session_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        "awaken.managed_session",
        vec![
            Migration::new(
                1,
                "managed session config: one row per session id (secret-free)",
                "CREATE TABLE {prefix}_session (\
                 session_id     TEXT PRIMARY KEY, \
                 agent_id       TEXT NOT NULL, \
                 model          TEXT NOT NULL, \
                 title          TEXT, \
                 metadata_json  TEXT NOT NULL, \
                 environment_id TEXT NOT NULL, \
                 mcp_json       TEXT NOT NULL)",
            )?,
            Migration::new(
                2,
                "managed session owner scope_id (ADR-0051)",
                "ALTER TABLE {prefix}_session ADD COLUMN scope_id TEXT NOT NULL DEFAULT 'default'",
            )?,
            Migration::new(
                3,
                "session lifecycle transactional outbox",
                "CREATE TABLE {prefix}_lifecycle_outbox (\
                    fact_id TEXT PRIMARY KEY, \
                    data TEXT NOT NULL, \
                    created_at {timestamptz} NOT NULL DEFAULT {now})",
            )?,
            Migration::new(
                4,
                "managed session durable lifecycle status",
                "ALTER TABLE {prefix}_session ADD COLUMN status TEXT NOT NULL DEFAULT 'idle'",
            )?,
            Migration::new(
                5,
                "managed session durable archive timestamp",
                "ALTER TABLE {prefix}_session ADD COLUMN archived_at TEXT",
            )?,
            Migration::new(
                6,
                "managed session frozen effective resource inputs",
                "ALTER TABLE {prefix}_session ADD COLUMN effective_inputs_json TEXT NOT NULL DEFAULT '{\"inputs\":[]}'",
            )?,
            Migration::new(
                7,
                "durable Memory extraction intents",
                "CREATE TABLE {prefix}_memory_extraction (\
                    intent_id TEXT PRIMARY KEY, \
                    idempotency_key TEXT NOT NULL UNIQUE, \
                    status TEXT NOT NULL, \
                    revision BIGINT NOT NULL, \
                    lease_expires_at_unix_ms BIGINT, \
                    data TEXT NOT NULL, \
                    created_at {timestamptz} NOT NULL DEFAULT {now})",
            )?,
            Migration::new(
                8,
                "managed session runtime environment binding",
                "ALTER TABLE {prefix}_session ADD COLUMN environment_binding TEXT",
            )?,
            Migration::new(
                9,
                "managed session secret-free runtime initialization pin",
                "ALTER TABLE {prefix}_session ADD COLUMN runtime_json TEXT NOT NULL DEFAULT '{\"mcp_servers\":[],\"runtime\":null,\"deny_egress\":false,\"sandbox\":null}'",
            )?,
            Migration::new(
                10,
                "managed session root optimistic-concurrency revision",
                "ALTER TABLE {prefix}_session ADD COLUMN revision BIGINT NOT NULL DEFAULT 1",
            )?,
            Migration::new(
                11,
                "managed session idempotency receipts",
                "CREATE TABLE {prefix}_session_idempotency (\
                    session_id TEXT NOT NULL, \
                    idempotency_key TEXT NOT NULL, \
                    payload_hash TEXT NOT NULL, \
                    committed_revision BIGINT NOT NULL, \
                    PRIMARY KEY (session_id, idempotency_key))",
            )?,
            Migration::new(
                12,
                "managed session durable delete tombstones",
                "CREATE TABLE {prefix}_session_tombstone (\
                    session_id TEXT PRIMARY KEY, \
                    scope_id TEXT NOT NULL, \
                    deleted_revision BIGINT NOT NULL, \
                    deleted_at TEXT NOT NULL)",
            )?,
            Migration::new(
                13,
                "one canonical serialized Session aggregate (ADR-0066)",
                "ALTER TABLE {prefix}_session ADD COLUMN aggregate_json TEXT",
            )?,
            Migration::new(
                14,
                "durable Dream jobs",
                "CREATE TABLE {prefix}_dream (job_id TEXT PRIMARY KEY, data TEXT NOT NULL)",
            )?,
            Migration::new(
                15,
                "durable Managed Deployments",
                "CREATE TABLE {prefix}_deployment (deployment_id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL, data TEXT NOT NULL)",
            )?,
            Migration::new(
                16,
                "durable Managed DeploymentRuns",
                "CREATE TABLE {prefix}_deployment_run (run_id TEXT PRIMARY KEY, deployment_id TEXT NOT NULL, workspace_id TEXT NOT NULL, data TEXT NOT NULL)",
            )?,
            Migration::new(
                17,
                "exactly-once scheduled Deployment occurrence claims",
                "CREATE TABLE {prefix}_deployment_claim (claim_id TEXT PRIMARY KEY, run_id TEXT NOT NULL UNIQUE, created_at {timestamptz} NOT NULL DEFAULT {now})",
            )?,
            Migration::new(
                18,
                "Workspace Dream scheduling policies",
                "CREATE TABLE {prefix}_dream_policy (workspace_id TEXT NOT NULL, memory_store_id TEXT NOT NULL, data TEXT NOT NULL, PRIMARY KEY (workspace_id, memory_store_id))",
            )?,
            Migration::new(
                19,
                "Deployment aggregate compare-and-swap revision",
                "ALTER TABLE {prefix}_deployment ADD COLUMN revision BIGINT NOT NULL DEFAULT 0",
            )?,
            Migration::new(
                20,
                "durable isolation for corrupt Session recovery rows",
                "CREATE TABLE {prefix}_session_quarantine (\
                    session_id TEXT PRIMARY KEY, \
                    reason TEXT NOT NULL, \
                    quarantined_at {timestamptz} NOT NULL DEFAULT {now})",
            )?,
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::session_bundle;

    #[test]
    fn session_schema_has_one_dense_current_stream() {
        /* MIG-01 causes: C1 current Session/Deployment/Dream schema effects;
         * C2 no retired Dream override table; C3 ordinary versioned migrations.
         * Effect E1 one dense V1..V20 stream. Decision rule M1=C1+C2+C3=>E1;
         * bundle construction owns rejection of duplicate/out-of-order versions. */
        let bundle = session_bundle().expect("session bundle");
        assert_eq!(
            bundle
                .migrations()
                .iter()
                .map(awaken_scoped_migration::Migration::version)
                .collect::<Vec<_>>(),
            (1..=20).collect::<Vec<_>>()
        );
        assert!(bundle.migrations().iter().all(|migration| {
            !migration
                .sql_for(awaken_scoped_migration::Dialect::Sqlite)
                .contains("dream_agent_override")
        }));
    }
}
