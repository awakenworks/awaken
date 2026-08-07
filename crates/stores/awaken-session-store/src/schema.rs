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
                "Workspace Dream Agent overrides",
                "CREATE TABLE {prefix}_dream_agent_override (workspace_id TEXT PRIMARY KEY, agent_id TEXT NOT NULL)",
            )?,
            Migration::new(
                16,
                "durable Managed Deployments",
                "CREATE TABLE {prefix}_deployment (deployment_id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL, data TEXT NOT NULL)",
            )?,
            Migration::new(
                17,
                "durable Managed DeploymentRuns",
                "CREATE TABLE {prefix}_deployment_run (run_id TEXT PRIMARY KEY, deployment_id TEXT NOT NULL, workspace_id TEXT NOT NULL, data TEXT NOT NULL)",
            )?,
            Migration::new(
                18,
                "exactly-once scheduled Deployment occurrence claims",
                "CREATE TABLE {prefix}_deployment_claim (claim_id TEXT PRIMARY KEY, run_id TEXT NOT NULL UNIQUE, created_at {timestamptz} NOT NULL DEFAULT {now})",
            )?,
            Migration::new(
                19,
                "Workspace Dream scheduling policies",
                "CREATE TABLE {prefix}_dream_policy (workspace_id TEXT NOT NULL, memory_store_id TEXT NOT NULL, data TEXT NOT NULL, PRIMARY KEY (workspace_id, memory_store_id))",
            )?,
            Migration::new(
                20,
                "remove duplicate Workspace Dream Agent override authority",
                "DROP TABLE {prefix}_dream_agent_override",
            )?,
            Migration::new(
                21,
                "Deployment aggregate compare-and-swap revision",
                "ALTER TABLE {prefix}_deployment ADD COLUMN revision BIGINT NOT NULL DEFAULT 0",
            )?,
        ],
    )
}
