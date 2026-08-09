//! Portable schema authority shared by the SQLite and PostgreSQL adapters.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

fn published(
    version: i64,
    description: &'static str,
    sql: &'static str,
) -> Result<Migration, MigrationError> {
    let (registered_description, checksum) = match version {
        1 => (
            "managed session config: one row per session id (secret-free)",
            "eee4cbec6c488a3e5c13ca9ad4485fc2e105fcd50c83db1e7fb6cb3094f0b9fe",
        ),
        2 => (
            "managed session owner scope_id (ADR-0051)",
            "9cba50939ce7c9edef5d55f922c29dc49b28bd0220a9a3724d25aed5e2c36dcd",
        ),
        3 => (
            "session lifecycle transactional outbox",
            "3cd5def858ba8ee0c7544c1bcecfaf0fbd9e687a375e406e57394728a7225be2",
        ),
        4 => (
            "managed session durable lifecycle status",
            "f8156abc3339fb88c37d213f41bc76e826101a9b9a888ac35763314def171e34",
        ),
        5 => (
            "managed session durable archive timestamp",
            "2c243cee6f95c0d8f3c4302ddb0b65a9e920be19420c1b695b02fce62f98bd56",
        ),
        6 => (
            "managed session frozen effective resource inputs",
            "428849e250b7efb64dd38b18bbe5584278fd3c6ad20f41be94d44106fe412ae8",
        ),
        7 => (
            "durable Memory extraction intents",
            "1be16cdd38b372f7f6edaf801295115b79f91946d2ed413a6d51766a1d328213",
        ),
        8 => (
            "managed session runtime environment binding",
            "ff70ffbb8dd4e563369a310a80af7ad0dd9a19252b4c3edc5974eafeca02781f",
        ),
        9 => (
            "managed session secret-free runtime initialization pin",
            "b046774f9bf0a37ce91f5187eebdbb122c8167b14d2a8dde13ed190ef1d14b38",
        ),
        10 => (
            "managed session root optimistic-concurrency revision",
            "a41b176c8abf9e173713a2071c22445ab5aa76ba3d27b3178dbae7f3ede0c118",
        ),
        11 => (
            "managed session idempotency receipts",
            "18995212beed85c076f6871e15533ea2b2558d508be191a3797164842dccdb94",
        ),
        12 => (
            "managed session durable delete tombstones",
            "8c4fb858db9152c8b36aa347bf1d7f06f8d9b79eb0811971ad5c6913529ecdfc",
        ),
        13 => (
            "one canonical serialized Session aggregate (ADR-0066)",
            "22f353a9ba24f3653290596ae509744d4207b8588cbfcaac0753afd63629220f",
        ),
        14 => (
            "durable Dream jobs",
            "7081058bb19809dc27e0e5c1d51ce498b52accdcd46d00286aee7c5e22b69c86",
        ),
        15 => (
            "Workspace Dream Agent overrides",
            "239d6e2bc4f1d6131476ed503e497b5975c79de24f1861537dbe9ed19d3ed025",
        ),
        16 => (
            "durable Managed Deployments",
            "a1aff0fd0f1b11f97392797ca33a48993f0985b9335fbf501ff1869f05182be5",
        ),
        17 => (
            "durable Managed DeploymentRuns",
            "35b0094e4936f0a1d7a509d9e9a132516731dbae2bffd0332ce8e6b1cf3fe365",
        ),
        18 => (
            "exactly-once scheduled Deployment occurrence claims",
            "72c0eee3376194c14c2df5de3054f797ded5338ea4144e97fccc44eb21541a82",
        ),
        19 => (
            "Workspace Dream scheduling policies",
            "30145a50ff003d0ac9aeb8995c136ba15692bbddf425086263e50717d1143b1d",
        ),
        20 => (
            "remove duplicate Workspace Dream Agent override authority",
            "6eaab27fffec8d9eee9f3e3afa89b8ba2b08aac683d9d3894ad835fbec3ad5a5",
        ),
        21 => (
            "Deployment aggregate compare-and-swap revision",
            "4db8985e75249634cc179a7e58d4d1c0cd693c16472f2f647a876da0f002f082",
        ),
        _ => {
            return Err(MigrationError::InvalidMigration {
                version,
                reason: "migration is absent from the published Session registry",
            });
        }
    };
    if description != registered_description {
        return Err(MigrationError::InvalidMigration {
            version,
            reason: "published Session migration description changed",
        });
    }
    Migration::published_legacy(version, registered_description, sql, checksum)
}

/// The versioned schema bundle (ADR-0043 scoped migration). All columns are
/// portable so both adapters read and write identical aggregate rows.
pub(crate) fn session_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        "awaken.managed_session",
        vec![
            published(
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
            published(
                2,
                "managed session owner scope_id (ADR-0051)",
                "ALTER TABLE {prefix}_session ADD COLUMN scope_id TEXT NOT NULL DEFAULT 'default'",
            )?,
            published(
                3,
                "session lifecycle transactional outbox",
                "CREATE TABLE {prefix}_lifecycle_outbox (\
                    fact_id TEXT PRIMARY KEY, \
                    data TEXT NOT NULL, \
                    created_at {timestamptz} NOT NULL DEFAULT {now})",
            )?,
            published(
                4,
                "managed session durable lifecycle status",
                "ALTER TABLE {prefix}_session ADD COLUMN status TEXT NOT NULL DEFAULT 'idle'",
            )?,
            published(
                5,
                "managed session durable archive timestamp",
                "ALTER TABLE {prefix}_session ADD COLUMN archived_at TEXT",
            )?,
            published(
                6,
                "managed session frozen effective resource inputs",
                "ALTER TABLE {prefix}_session ADD COLUMN effective_inputs_json TEXT NOT NULL DEFAULT '{\"inputs\":[]}'",
            )?,
            published(
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
            published(
                8,
                "managed session runtime environment binding",
                "ALTER TABLE {prefix}_session ADD COLUMN environment_binding TEXT",
            )?,
            published(
                9,
                "managed session secret-free runtime initialization pin",
                "ALTER TABLE {prefix}_session ADD COLUMN runtime_json TEXT NOT NULL DEFAULT '{\"mcp_servers\":[],\"runtime\":null,\"deny_egress\":false,\"sandbox\":null}'",
            )?,
            published(
                10,
                "managed session root optimistic-concurrency revision",
                "ALTER TABLE {prefix}_session ADD COLUMN revision BIGINT NOT NULL DEFAULT 1",
            )?,
            published(
                11,
                "managed session idempotency receipts",
                "CREATE TABLE {prefix}_session_idempotency (\
                    session_id TEXT NOT NULL, \
                    idempotency_key TEXT NOT NULL, \
                    payload_hash TEXT NOT NULL, \
                    committed_revision BIGINT NOT NULL, \
                    PRIMARY KEY (session_id, idempotency_key))",
            )?,
            published(
                12,
                "managed session durable delete tombstones",
                "CREATE TABLE {prefix}_session_tombstone (\
                    session_id TEXT PRIMARY KEY, \
                    scope_id TEXT NOT NULL, \
                    deleted_revision BIGINT NOT NULL, \
                    deleted_at TEXT NOT NULL)",
            )?,
            published(
                13,
                "one canonical serialized Session aggregate (ADR-0066)",
                "ALTER TABLE {prefix}_session ADD COLUMN aggregate_json TEXT",
            )?,
            published(
                14,
                "durable Dream jobs",
                "CREATE TABLE {prefix}_dream (job_id TEXT PRIMARY KEY, data TEXT NOT NULL)",
            )?,
            published(
                15,
                "Workspace Dream Agent overrides",
                "CREATE TABLE {prefix}_dream_agent_override (workspace_id TEXT PRIMARY KEY, agent_id TEXT NOT NULL)",
            )?,
            published(
                16,
                "durable Managed Deployments",
                "CREATE TABLE {prefix}_deployment (deployment_id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL, data TEXT NOT NULL)",
            )?,
            published(
                17,
                "durable Managed DeploymentRuns",
                "CREATE TABLE {prefix}_deployment_run (run_id TEXT PRIMARY KEY, deployment_id TEXT NOT NULL, workspace_id TEXT NOT NULL, data TEXT NOT NULL)",
            )?,
            published(
                18,
                "exactly-once scheduled Deployment occurrence claims",
                "CREATE TABLE {prefix}_deployment_claim (claim_id TEXT PRIMARY KEY, run_id TEXT NOT NULL UNIQUE, created_at {timestamptz} NOT NULL DEFAULT {now})",
            )?,
            published(
                19,
                "Workspace Dream scheduling policies",
                "CREATE TABLE {prefix}_dream_policy (workspace_id TEXT NOT NULL, memory_store_id TEXT NOT NULL, data TEXT NOT NULL, PRIMARY KEY (workspace_id, memory_store_id))",
            )?,
            published(
                20,
                "remove duplicate Workspace Dream Agent override authority",
                "DROP TABLE {prefix}_dream_agent_override",
            )?,
            published(
                21,
                "Deployment aggregate compare-and-swap revision",
                "ALTER TABLE {prefix}_deployment ADD COLUMN revision BIGINT NOT NULL DEFAULT 0",
            )?,
            Migration::new(
                22,
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
    use super::published;

    #[test]
    fn published_identity_registry_fails_closed_for_every_mutable_axis() {
        /* MIG-01 cause/effect decision table. Causes: C1 registered version,
         * C2 exact description, C3 exact SQL body. Effect E1 construct the
         * published Migration; E2 reject before any backend connects. Rules:
         * M1 T/T/T=>E1; M2 unknown/any/any=>E2; M3 T/F/T=>E2;
         * M4 T/T/F=>E2. */
        let exact_sql =
            "ALTER TABLE {prefix}_deployment ADD COLUMN revision BIGINT NOT NULL DEFAULT 0";
        assert!(
            published(
                21,
                "Deployment aggregate compare-and-swap revision",
                exact_sql,
            )
            .is_ok(),
            "M1"
        );
        assert!(
            published(
                99,
                "Deployment aggregate compare-and-swap revision",
                exact_sql,
            )
            .is_err(),
            "M2"
        );
        assert!(published(21, "renamed migration", exact_sql).is_err(), "M3");
        assert!(
            published(
                21,
                "Deployment aggregate compare-and-swap revision",
                "ALTER TABLE {prefix}_deployment ADD COLUMN revision BIGINT NOT NULL DEFAULT 1",
            )
            .is_err(),
            "M4"
        );
    }
}
