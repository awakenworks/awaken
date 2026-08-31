//! Closed Session histories published before the current aggregate baseline.
//! Both streams share V1..V14, diverge at V15, and receive the same physical
//! upgrade as their final branch-local migration. Runtime repositories never
//! branch after migration; all future DDL belongs to the convergence bundle.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

use super::BUNDLE_ID;

pub(super) const V1_CHECKSUM: &str =
    "eee4cbec6c488a3e5c13ca9ad4485fc2e105fcd50c83db1e7fb6cb3094f0b9fe";
pub(super) const COMPACTED_V15_CHECKSUM: &str =
    "d291b5d97cb66d4f245d163c8aed4b7c057008020731dd7d63b5ba51ed00c8e8";
const PUBLISHED_LEGACY_MIGRATION_COUNT: usize = 28;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ExpandedStream {
    Original,
    Compacted,
}

struct PublishedMigration {
    version: i64,
    description: &'static str,
    sql: &'static str,
    checksum: &'static str,
}

const COMMON: &[PublishedMigration] = &[
    PublishedMigration {
        version: 1,
        description: "managed session config: one row per session id (secret-free)",
        sql: "CREATE TABLE {prefix}_session (\
                 session_id     TEXT PRIMARY KEY, \
                 agent_id       TEXT NOT NULL, \
                 model          TEXT NOT NULL, \
                 title          TEXT, \
                 metadata_json  TEXT NOT NULL, \
                 environment_id TEXT NOT NULL, \
                 mcp_json       TEXT NOT NULL)",
        checksum: V1_CHECKSUM,
    },
    PublishedMigration {
        version: 2,
        description: "managed session owner scope_id (ADR-0051)",
        sql: "ALTER TABLE {prefix}_session ADD COLUMN scope_id TEXT NOT NULL DEFAULT 'default'",
        checksum: "9cba50939ce7c9edef5d55f922c29dc49b28bd0220a9a3724d25aed5e2c36dcd",
    },
    PublishedMigration {
        version: 3,
        description: "session lifecycle transactional outbox",
        sql: "CREATE TABLE {prefix}_lifecycle_outbox (\
                    fact_id TEXT PRIMARY KEY, \
                    data TEXT NOT NULL, \
                    created_at {timestamptz} NOT NULL DEFAULT {now})",
        checksum: "3cd5def858ba8ee0c7544c1bcecfaf0fbd9e687a375e406e57394728a7225be2",
    },
    PublishedMigration {
        version: 4,
        description: "managed session durable lifecycle status",
        sql: "ALTER TABLE {prefix}_session ADD COLUMN status TEXT NOT NULL DEFAULT 'idle'",
        checksum: "f8156abc3339fb88c37d213f41bc76e826101a9b9a888ac35763314def171e34",
    },
    PublishedMigration {
        version: 5,
        description: "managed session durable archive timestamp",
        sql: "ALTER TABLE {prefix}_session ADD COLUMN archived_at TEXT",
        checksum: "2c243cee6f95c0d8f3c4302ddb0b65a9e920be19420c1b695b02fce62f98bd56",
    },
    PublishedMigration {
        version: 6,
        description: "managed session frozen effective resource inputs",
        sql: "ALTER TABLE {prefix}_session ADD COLUMN effective_inputs_json TEXT NOT NULL DEFAULT '{\"inputs\":[]}'",
        checksum: "428849e250b7efb64dd38b18bbe5584278fd3c6ad20f41be94d44106fe412ae8",
    },
    PublishedMigration {
        version: 7,
        description: "durable Memory extraction intents",
        sql: "CREATE TABLE {prefix}_memory_extraction (\
                    intent_id TEXT PRIMARY KEY, \
                    idempotency_key TEXT NOT NULL UNIQUE, \
                    status TEXT NOT NULL, \
                    revision BIGINT NOT NULL, \
                    lease_expires_at_unix_ms BIGINT, \
                    data TEXT NOT NULL, \
                    created_at {timestamptz} NOT NULL DEFAULT {now})",
        checksum: "1be16cdd38b372f7f6edaf801295115b79f91946d2ed413a6d51766a1d328213",
    },
    PublishedMigration {
        version: 8,
        description: "managed session runtime environment binding",
        sql: "ALTER TABLE {prefix}_session ADD COLUMN environment_binding TEXT",
        checksum: "ff70ffbb8dd4e563369a310a80af7ad0dd9a19252b4c3edc5974eafeca02781f",
    },
    PublishedMigration {
        version: 9,
        description: "managed session secret-free runtime initialization pin",
        sql: "ALTER TABLE {prefix}_session ADD COLUMN runtime_json TEXT NOT NULL DEFAULT '{\"mcp_servers\":[],\"runtime\":null,\"deny_egress\":false,\"sandbox\":null}'",
        checksum: "b046774f9bf0a37ce91f5187eebdbb122c8167b14d2a8dde13ed190ef1d14b38",
    },
    PublishedMigration {
        version: 10,
        description: "managed session root optimistic-concurrency revision",
        sql: "ALTER TABLE {prefix}_session ADD COLUMN revision BIGINT NOT NULL DEFAULT 1",
        checksum: "a41b176c8abf9e173713a2071c22445ab5aa76ba3d27b3178dbae7f3ede0c118",
    },
    PublishedMigration {
        version: 11,
        description: "managed session idempotency receipts",
        sql: "CREATE TABLE {prefix}_session_idempotency (\
                    session_id TEXT NOT NULL, \
                    idempotency_key TEXT NOT NULL, \
                    payload_hash TEXT NOT NULL, \
                    committed_revision BIGINT NOT NULL, \
                    PRIMARY KEY (session_id, idempotency_key))",
        checksum: "18995212beed85c076f6871e15533ea2b2558d508be191a3797164842dccdb94",
    },
    PublishedMigration {
        version: 12,
        description: "managed session durable delete tombstones",
        sql: "CREATE TABLE {prefix}_session_tombstone (\
                    session_id TEXT PRIMARY KEY, \
                    scope_id TEXT NOT NULL, \
                    deleted_revision BIGINT NOT NULL, \
                    deleted_at TEXT NOT NULL)",
        checksum: "8c4fb858db9152c8b36aa347bf1d7f06f8d9b79eb0811971ad5c6913529ecdfc",
    },
    PublishedMigration {
        version: 13,
        description: "one canonical serialized Session aggregate (ADR-0066)",
        sql: "ALTER TABLE {prefix}_session ADD COLUMN aggregate_json TEXT",
        checksum: "22f353a9ba24f3653290596ae509744d4207b8588cbfcaac0753afd63629220f",
    },
    PublishedMigration {
        version: 14,
        description: "durable Dream jobs",
        sql: "CREATE TABLE {prefix}_dream (job_id TEXT PRIMARY KEY, data TEXT NOT NULL)",
        checksum: "7081058bb19809dc27e0e5c1d51ce498b52accdcd46d00286aee7c5e22b69c86",
    },
];

const ORIGINAL_SUFFIX: &[PublishedMigration] = &[
    PublishedMigration {
        version: 15,
        description: "Workspace Dream Agent overrides",
        sql: "CREATE TABLE {prefix}_dream_agent_override (workspace_id TEXT PRIMARY KEY, agent_id TEXT NOT NULL)",
        checksum: "239d6e2bc4f1d6131476ed503e497b5975c79de24f1861537dbe9ed19d3ed025",
    },
    PublishedMigration {
        version: 16,
        description: "durable Managed Deployments",
        sql: "CREATE TABLE {prefix}_deployment (deployment_id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL, data TEXT NOT NULL)",
        checksum: "a1aff0fd0f1b11f97392797ca33a48993f0985b9335fbf501ff1869f05182be5",
    },
    PublishedMigration {
        version: 17,
        description: "durable Managed DeploymentRuns",
        sql: "CREATE TABLE {prefix}_deployment_run (run_id TEXT PRIMARY KEY, deployment_id TEXT NOT NULL, workspace_id TEXT NOT NULL, data TEXT NOT NULL)",
        checksum: "35b0094e4936f0a1d7a509d9e9a132516731dbae2bffd0332ce8e6b1cf3fe365",
    },
    PublishedMigration {
        version: 18,
        description: "exactly-once scheduled Deployment occurrence claims",
        sql: "CREATE TABLE {prefix}_deployment_claim (claim_id TEXT PRIMARY KEY, run_id TEXT NOT NULL UNIQUE, created_at {timestamptz} NOT NULL DEFAULT {now})",
        checksum: "72c0eee3376194c14c2df5de3054f797ded5338ea4144e97fccc44eb21541a82",
    },
    PublishedMigration {
        version: 19,
        description: "Workspace Dream scheduling policies",
        sql: "CREATE TABLE {prefix}_dream_policy (workspace_id TEXT NOT NULL, memory_store_id TEXT NOT NULL, data TEXT NOT NULL, PRIMARY KEY (workspace_id, memory_store_id))",
        checksum: "30145a50ff003d0ac9aeb8995c136ba15692bbddf425086263e50717d1143b1d",
    },
    PublishedMigration {
        version: 20,
        description: "remove duplicate Workspace Dream Agent override authority",
        sql: "DROP TABLE {prefix}_dream_agent_override",
        checksum: "6eaab27fffec8d9eee9f3e3afa89b8ba2b08aac683d9d3894ad835fbec3ad5a5",
    },
    PublishedMigration {
        version: 21,
        description: "Deployment aggregate compare-and-swap revision",
        sql: "ALTER TABLE {prefix}_deployment ADD COLUMN revision BIGINT NOT NULL DEFAULT 0",
        checksum: "4db8985e75249634cc179a7e58d4d1c0cd693c16472f2f647a876da0f002f082",
    },
    PublishedMigration {
        version: 22,
        description: "durable isolation for corrupt Session recovery rows",
        sql: "CREATE TABLE {prefix}_session_quarantine (\
                    session_id TEXT PRIMARY KEY, \
                    reason TEXT NOT NULL, \
                    quarantined_at {timestamptz} NOT NULL DEFAULT {now})",
        checksum: "107ddbc8842db5159f62d4d1a9c11987dbdcb5a6fdcdb4bbea48b3f163fd5e98",
    },
];

const COMPACTED_SUFFIX: &[PublishedMigration] = &[
    PublishedMigration {
        version: 15,
        description: "durable Managed Deployments",
        sql: "CREATE TABLE {prefix}_deployment (deployment_id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL, data TEXT NOT NULL)",
        checksum: COMPACTED_V15_CHECKSUM,
    },
    PublishedMigration {
        version: 16,
        description: "durable Managed DeploymentRuns",
        sql: "CREATE TABLE {prefix}_deployment_run (run_id TEXT PRIMARY KEY, deployment_id TEXT NOT NULL, workspace_id TEXT NOT NULL, data TEXT NOT NULL)",
        checksum: "920d2a92bc14b6b885c7ed3a00337cf5e595aaeaffc944dbc5773ffe415ce316",
    },
    PublishedMigration {
        version: 17,
        description: "exactly-once scheduled Deployment occurrence claims",
        sql: "CREATE TABLE {prefix}_deployment_claim (claim_id TEXT PRIMARY KEY, run_id TEXT NOT NULL UNIQUE, created_at {timestamptz} NOT NULL DEFAULT {now})",
        checksum: "5a03f63ee60537c099e22d7d402c07a24edc71ac217e10f19b7287602c96c9d5",
    },
    PublishedMigration {
        version: 18,
        description: "Workspace Dream scheduling policies",
        sql: "CREATE TABLE {prefix}_dream_policy (workspace_id TEXT NOT NULL, memory_store_id TEXT NOT NULL, data TEXT NOT NULL, PRIMARY KEY (workspace_id, memory_store_id))",
        checksum: "71fa8678926c400255f8225e6a405895e0ecf3b7d4966293566a256edfa2812e",
    },
    PublishedMigration {
        version: 19,
        description: "Deployment aggregate compare-and-swap revision",
        sql: "ALTER TABLE {prefix}_deployment ADD COLUMN revision BIGINT NOT NULL DEFAULT 0",
        checksum: "fc37c7a3a7e3b6cec00c4e5c57e820d040d3e94a5b29dfed6d1977d4c0015496",
    },
    PublishedMigration {
        version: 20,
        description: "durable isolation for corrupt Session recovery rows",
        sql: "CREATE TABLE {prefix}_session_quarantine (\
                    session_id TEXT PRIMARY KEY, \
                    reason TEXT NOT NULL, \
                    quarantined_at {timestamptz} NOT NULL DEFAULT {now})",
        checksum: "b2552703b23629f0dd5c5027ea43ab206d9b921788047894edfc94da163d9c57",
    },
];

const SQLITE_UPGRADE: &str = "ALTER TABLE {prefix}_session_quarantine RENAME TO {prefix}_session_quarantine_legacy; \
    ALTER TABLE {prefix}_session RENAME TO {prefix}_session_legacy; \
    CREATE TABLE {prefix}_session (session_id TEXT PRIMARY KEY CHECK (length(session_id) > 0), scope_id TEXT NOT NULL CHECK (length(scope_id) > 0), revision BIGINT NOT NULL CHECK (revision > 0), aggregate_json TEXT NOT NULL); \
    INSERT INTO {prefix}_session(session_id,scope_id,revision,aggregate_json) SELECT session_id,scope_id,revision,aggregate_json FROM {prefix}_session_legacy; \
    DROP TABLE {prefix}_session_legacy; \
    CREATE INDEX {prefix}_session_scope_idx ON {prefix}_session(scope_id,session_id); \
    ALTER TABLE {prefix}_memory_extraction RENAME TO {prefix}_memory_extraction_legacy; \
    CREATE TABLE {prefix}_memory_extraction (intent_id TEXT PRIMARY KEY, idempotency_key TEXT NOT NULL UNIQUE, status TEXT NOT NULL, revision BIGINT NOT NULL CHECK (revision >= 0), lease_expires_at_unix_ms BIGINT CHECK (lease_expires_at_unix_ms IS NULL OR lease_expires_at_unix_ms >= 0), data TEXT NOT NULL, created_at {timestamptz} NOT NULL DEFAULT {now}); \
    INSERT INTO {prefix}_memory_extraction SELECT * FROM {prefix}_memory_extraction_legacy; \
    DROP TABLE {prefix}_memory_extraction_legacy; \
    ALTER TABLE {prefix}_session_idempotency RENAME TO {prefix}_session_idempotency_legacy; \
    CREATE TABLE {prefix}_session_idempotency (session_id TEXT NOT NULL, idempotency_key TEXT NOT NULL, payload_hash TEXT NOT NULL, committed_revision BIGINT NOT NULL CHECK (committed_revision > 0), PRIMARY KEY(session_id,idempotency_key)); \
    INSERT INTO {prefix}_session_idempotency SELECT * FROM {prefix}_session_idempotency_legacy; \
    DROP TABLE {prefix}_session_idempotency_legacy; \
    ALTER TABLE {prefix}_session_tombstone RENAME TO {prefix}_session_tombstone_legacy; \
    CREATE TABLE {prefix}_session_tombstone (session_id TEXT PRIMARY KEY, scope_id TEXT NOT NULL, deleted_revision BIGINT NOT NULL CHECK (deleted_revision > 0), deleted_at TEXT NOT NULL); \
    INSERT INTO {prefix}_session_tombstone SELECT * FROM {prefix}_session_tombstone_legacy; \
    DROP TABLE {prefix}_session_tombstone_legacy; \
    CREATE TABLE {prefix}_session_quarantine (session_id TEXT PRIMARY KEY REFERENCES {prefix}_session(session_id) ON DELETE CASCADE, reason TEXT NOT NULL, observed_revision BIGINT NOT NULL CHECK (observed_revision > 0), quarantined_at {timestamptz} NOT NULL DEFAULT {now}); \
    INSERT INTO {prefix}_session_quarantine(session_id,reason,observed_revision,quarantined_at) SELECT legacy.session_id,legacy.reason,(SELECT revision FROM {prefix}_session WHERE session_id=legacy.session_id),legacy.quarantined_at FROM {prefix}_session_quarantine_legacy legacy; \
    DROP TABLE {prefix}_session_quarantine_legacy; \
    CREATE TABLE {prefix}_session_reconciliation_work (session_id TEXT PRIMARY KEY REFERENCES {prefix}_session(session_id) ON DELETE CASCADE, observed_revision BIGINT NOT NULL CHECK (observed_revision > 0)); \
    CREATE TABLE {prefix}_session_vault_reference (session_id TEXT NOT NULL REFERENCES {prefix}_session(session_id) ON DELETE CASCADE, vault_id TEXT NOT NULL, PRIMARY KEY(session_id,vault_id)); \
    CREATE INDEX {prefix}_session_vault_reference_lookup_idx ON {prefix}_session_vault_reference(vault_id,session_id); \
    CREATE TABLE {prefix}_session_credential_source_reference (session_id TEXT NOT NULL REFERENCES {prefix}_session(session_id) ON DELETE CASCADE, credential_source_id TEXT NOT NULL, PRIMARY KEY(session_id,credential_source_id)); \
    CREATE INDEX {prefix}_session_credential_source_reference_lookup_idx ON {prefix}_session_credential_source_reference(credential_source_id,session_id); \
    ALTER TABLE {prefix}_deployment_claim RENAME TO {prefix}_deployment_claim_legacy; \
    ALTER TABLE {prefix}_deployment_run RENAME TO {prefix}_deployment_run_legacy; \
    ALTER TABLE {prefix}_deployment RENAME TO {prefix}_deployment_legacy; \
    CREATE TABLE {prefix}_deployment (deployment_id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL, data TEXT NOT NULL, revision BIGINT NOT NULL DEFAULT 0 CHECK (revision >= 0)); \
    INSERT INTO {prefix}_deployment SELECT * FROM {prefix}_deployment_legacy; \
    CREATE INDEX {prefix}_deployment_workspace_idx ON {prefix}_deployment(workspace_id,deployment_id); \
    CREATE TABLE {prefix}_deployment_run (run_id TEXT PRIMARY KEY, deployment_id TEXT NOT NULL REFERENCES {prefix}_deployment(deployment_id), workspace_id TEXT NOT NULL, data TEXT NOT NULL); \
    INSERT INTO {prefix}_deployment_run SELECT * FROM {prefix}_deployment_run_legacy; \
    CREATE INDEX {prefix}_deployment_run_deployment_idx ON {prefix}_deployment_run(deployment_id,run_id); \
    CREATE TABLE {prefix}_deployment_claim (claim_id TEXT PRIMARY KEY, run_id TEXT NOT NULL UNIQUE REFERENCES {prefix}_deployment_run(run_id), created_at {timestamptz} NOT NULL DEFAULT {now}); \
    INSERT INTO {prefix}_deployment_claim SELECT * FROM {prefix}_deployment_claim_legacy; \
    DROP TABLE {prefix}_deployment_claim_legacy; \
    DROP TABLE {prefix}_deployment_run_legacy; \
    DROP TABLE {prefix}_deployment_legacy";

const POSTGRES_UPGRADE: &str = "ALTER TABLE {prefix}_session ALTER COLUMN aggregate_json SET NOT NULL; \
    ALTER TABLE {prefix}_session ADD CONSTRAINT {prefix}_session_id_nonempty CHECK (length(session_id) > 0), ADD CONSTRAINT {prefix}_session_scope_nonempty CHECK (length(scope_id) > 0), ADD CONSTRAINT {prefix}_session_revision_positive CHECK (revision > 0); \
    ALTER TABLE {prefix}_session DROP COLUMN agent_id, DROP COLUMN model, DROP COLUMN title, DROP COLUMN metadata_json, DROP COLUMN environment_id, DROP COLUMN mcp_json, DROP COLUMN status, DROP COLUMN archived_at, DROP COLUMN effective_inputs_json, DROP COLUMN environment_binding, DROP COLUMN runtime_json; \
    CREATE INDEX {prefix}_session_scope_idx ON {prefix}_session(scope_id,session_id); \
    ALTER TABLE {prefix}_memory_extraction ADD CONSTRAINT {prefix}_memory_extraction_revision_nonnegative CHECK (revision >= 0), ADD CONSTRAINT {prefix}_memory_extraction_lease_nonnegative CHECK (lease_expires_at_unix_ms IS NULL OR lease_expires_at_unix_ms >= 0); \
    ALTER TABLE {prefix}_session_idempotency ADD CONSTRAINT {prefix}_session_idempotency_revision_positive CHECK (committed_revision > 0); \
    ALTER TABLE {prefix}_session_tombstone ADD CONSTRAINT {prefix}_session_tombstone_revision_positive CHECK (deleted_revision > 0); \
    ALTER TABLE {prefix}_session_quarantine ADD COLUMN observed_revision BIGINT; \
    UPDATE {prefix}_session_quarantine quarantine SET observed_revision=session.revision FROM {prefix}_session session WHERE session.session_id=quarantine.session_id; \
    ALTER TABLE {prefix}_session_quarantine ALTER COLUMN observed_revision SET NOT NULL, ADD CONSTRAINT {prefix}_session_quarantine_revision_positive CHECK (observed_revision > 0), ADD CONSTRAINT {prefix}_session_quarantine_session_fk FOREIGN KEY(session_id) REFERENCES {prefix}_session(session_id) ON DELETE CASCADE; \
    CREATE TABLE {prefix}_session_reconciliation_work (session_id TEXT PRIMARY KEY REFERENCES {prefix}_session(session_id) ON DELETE CASCADE, observed_revision BIGINT NOT NULL CHECK (observed_revision > 0)); \
    CREATE TABLE {prefix}_session_vault_reference (session_id TEXT NOT NULL REFERENCES {prefix}_session(session_id) ON DELETE CASCADE, vault_id TEXT NOT NULL, PRIMARY KEY(session_id,vault_id)); \
    CREATE INDEX {prefix}_session_vault_reference_lookup_idx ON {prefix}_session_vault_reference(vault_id,session_id); \
    CREATE TABLE {prefix}_session_credential_source_reference (session_id TEXT NOT NULL REFERENCES {prefix}_session(session_id) ON DELETE CASCADE, credential_source_id TEXT NOT NULL, PRIMARY KEY(session_id,credential_source_id)); \
    CREATE INDEX {prefix}_session_credential_source_reference_lookup_idx ON {prefix}_session_credential_source_reference(credential_source_id,session_id); \
    ALTER TABLE {prefix}_deployment ADD CONSTRAINT {prefix}_deployment_revision_nonnegative CHECK (revision >= 0); \
    CREATE INDEX {prefix}_deployment_workspace_idx ON {prefix}_deployment(workspace_id,deployment_id); \
    ALTER TABLE {prefix}_deployment_run ADD CONSTRAINT {prefix}_deployment_run_deployment_fk FOREIGN KEY(deployment_id) REFERENCES {prefix}_deployment(deployment_id); \
    CREATE INDEX {prefix}_deployment_run_deployment_idx ON {prefix}_deployment_run(deployment_id,run_id); \
    ALTER TABLE {prefix}_deployment_claim ADD CONSTRAINT {prefix}_deployment_claim_run_fk FOREIGN KEY(run_id) REFERENCES {prefix}_deployment_run(run_id)";

fn published_migrations(stream: ExpandedStream) -> Result<Vec<Migration>, MigrationError> {
    assert_eq!(
        COMMON.len() + ORIGINAL_SUFFIX.len() + COMPACTED_SUFFIX.len(),
        PUBLISHED_LEGACY_MIGRATION_COUNT
    );
    let suffix = match stream {
        ExpandedStream::Original => ORIGINAL_SUFFIX,
        ExpandedStream::Compacted => COMPACTED_SUFFIX,
    };
    COMMON
        .iter()
        .chain(suffix)
        .map(|entry| {
            Migration::published_legacy(entry.version, entry.description, entry.sql, entry.checksum)
        })
        .collect()
}

pub(super) fn published_bundle(stream: ExpandedStream) -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(BUNDLE_ID, published_migrations(stream)?)
}

pub(super) fn bundle(stream: ExpandedStream) -> Result<MigrationBundle, MigrationError> {
    let mut migrations = published_migrations(stream)?;
    let version = match stream {
        ExpandedStream::Original => 23,
        ExpandedStream::Compacted => 21,
    };
    migrations.push(Migration::per_dialect(
        version,
        "converge legacy Session storage to the canonical aggregate schema",
        POSTGRES_UPGRADE,
        SQLITE_UPGRADE,
    )?);
    MigrationBundle::new(BUNDLE_ID, migrations)
}
