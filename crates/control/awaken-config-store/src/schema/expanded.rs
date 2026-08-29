//! Exact V1..V10 Config history published before the compact baseline.
//! Runtime repositories never branch on this module; only migration admission
//! selects it from the immutable V1 receipt.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

use super::BUNDLE_ID;

pub(super) const V1_CHECKSUM: &str =
    "3134e209014343fdb07c7e57bedbdd42a27fab4017e9b2c6a7a69f3d68dcf588";
const PUBLISHED_LEGACY_MIGRATION_COUNT: usize = 2;
const V5_POSTGRES: &str =
    include_str!("../migrations/expanded/V0005__publication_sequence.postgres.sql");
const V5_SQLITE: &str =
    include_str!("../migrations/expanded/V0005__publication_sequence.sqlite.sql");
const V9_POSTGRES: &str = include_str!("../migrations/expanded/V0009__agent_revision.postgres.sql");
const V9_SQLITE: &str = include_str!("../migrations/expanded/V0009__agent_revision.sqlite.sql");

pub(super) fn bundle() -> Result<MigrationBundle, MigrationError> {
    assert_eq!(PUBLISHED_LEGACY_MIGRATION_COUNT, 2);
    MigrationBundle::new(
        BUNDLE_ID,
        vec![
            Migration::new(
                1,
                "agent configs: the authoring aggregate, one row per agent id",
                "CREATE TABLE {prefix}_agent (\
                    id TEXT PRIMARY KEY, \
                    data {json} NOT NULL, \
                    created_at {timestamptz} NOT NULL DEFAULT {now})",
            )?,
            Migration::new(
                2,
                "publications: the compiled artifact, content-addressed by fingerprint",
                "CREATE TABLE {prefix}_publication (\
                    fingerprint TEXT PRIMARY KEY, \
                    agent_id TEXT NOT NULL, \
                    state TEXT NOT NULL, \
                    record {json} NOT NULL, \
                    created_at {timestamptz} NOT NULL DEFAULT {now})",
            )?,
            Migration::new(
                3,
                "agent configs: opaque owner scope_id (ADR-0051)",
                "ALTER TABLE {prefix}_agent ADD COLUMN scope_id TEXT NOT NULL DEFAULT 'default'",
            )?,
            Migration::new(
                4,
                "publications: opaque owner scope_id (ADR-0051)",
                "ALTER TABLE {prefix}_publication ADD COLUMN scope_id TEXT NOT NULL DEFAULT 'default'",
            )?,
            Migration::published_legacy_per_dialect_with_aliases(
                5,
                "publications: monotonic insertion-order tie-break for deterministic warm-load",
                V5_POSTGRES.trim(),
                "2126a3b57d9a71c654646795a42e3ed9a5c69cde6a11d9782a881273e5ec8b1e",
                std::iter::empty::<&str>(),
                V5_SQLITE.trim(),
                "fa0453638c9a39e09673d843eef2b37e1bf17363e69c0b3f099394c7e532e729",
                ["3811e84d319e0dd8d4f0f16d75fb2b7a73b5d76acb759f05518468b76682acd5"],
            )?,
            Migration::new(
                6,
                "agent configs: monotonic generation for atomic compare-and-set",
                "ALTER TABLE {prefix}_agent ADD COLUMN generation BIGINT NOT NULL DEFAULT 1",
            )?,
            Migration::new(
                7,
                "durable management audit keyed by scope and stable call id",
                "CREATE TABLE {prefix}_management_audit (\
                    scope_id TEXT NOT NULL, \
                    call_id TEXT NOT NULL, \
                    record {json} NOT NULL, \
                    business_committed BIGINT NOT NULL DEFAULT 0, \
                    created_at {timestamptz} NOT NULL DEFAULT {now}, \
                    PRIMARY KEY (scope_id, call_id))",
            )?,
            Migration::new(
                8,
                "durable idempotent effects for stores outside the config transaction",
                "CREATE TABLE {prefix}_management_effect (\
                    scope_id TEXT NOT NULL, \
                    kind TEXT NOT NULL, \
                    effect_key TEXT NOT NULL, \
                    payload {json} NOT NULL, \
                    created_at {timestamptz} NOT NULL DEFAULT {now}, \
                    PRIMARY KEY (scope_id, kind, effect_key))",
            )?,
            Migration::published_legacy_per_dialect_with_aliases(
                9,
                "agent configs: immutable revision history for managed Agent versions",
                V9_POSTGRES.trim(),
                "7ae3ea07935bdcd654b948d9c7ce255ca6d897a2c1ed7e664d75e0f0bcb7e69c",
                std::iter::empty::<&str>(),
                V9_SQLITE.trim(),
                "aba2c4fb899bbf493dd6e5aea7e682b3baa313e6d022af5b98ffbca7868a3684",
                ["9957a1bf1c53dfa318e927883f34f4200fe00f5db1853a272d63389022a96814"],
            )?,
            Migration::per_dialect(
                10,
                "agent configs and publications: identity is scoped, not globally first-writer-owned",
                "ALTER TABLE {prefix}_agent DROP CONSTRAINT {prefix}_agent_pkey; \
                 ALTER TABLE {prefix}_agent ADD PRIMARY KEY (scope_id, id); \
                 ALTER TABLE {prefix}_publication DROP CONSTRAINT {prefix}_publication_pkey; \
                 ALTER TABLE {prefix}_publication ADD PRIMARY KEY (scope_id, fingerprint)",
                "DROP TRIGGER {prefix}_agent_revision_insert; \
                 DROP TRIGGER {prefix}_agent_revision_update; \
                 ALTER TABLE {prefix}_agent RENAME TO {prefix}_agent_unscoped; \
                 CREATE TABLE {prefix}_agent (\
                    id TEXT NOT NULL, data TEXT NOT NULL, \
                    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, \
                    scope_id TEXT NOT NULL DEFAULT 'default', \
                    generation BIGINT NOT NULL DEFAULT 1, \
                    PRIMARY KEY (scope_id, id)); \
                 INSERT INTO {prefix}_agent (id, data, created_at, scope_id, generation) \
                    SELECT id, data, created_at, scope_id, generation \
                    FROM {prefix}_agent_unscoped ORDER BY rowid; \
                 DROP TABLE {prefix}_agent_unscoped; \
                 CREATE TRIGGER {prefix}_agent_revision_insert AFTER INSERT ON {prefix}_agent BEGIN \
                    INSERT INTO {prefix}_agent_revision (scope_id, id, generation, data) \
                      VALUES (NEW.scope_id, NEW.id, NEW.generation, NEW.data); END; \
                 CREATE TRIGGER {prefix}_agent_revision_update AFTER UPDATE ON {prefix}_agent \
                    WHEN OLD.generation <> NEW.generation BEGIN \
                    INSERT INTO {prefix}_agent_revision (scope_id, id, generation, data) \
                      VALUES (NEW.scope_id, NEW.id, NEW.generation, NEW.data); END; \
                 ALTER TABLE {prefix}_publication RENAME TO {prefix}_publication_unscoped; \
                 CREATE TABLE {prefix}_publication (\
                    fingerprint TEXT NOT NULL, agent_id TEXT NOT NULL, state TEXT NOT NULL, \
                    record TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, \
                    scope_id TEXT NOT NULL DEFAULT 'default', \
                    PRIMARY KEY (scope_id, fingerprint)); \
                 INSERT INTO {prefix}_publication \
                    (fingerprint, agent_id, state, record, created_at, scope_id) \
                    SELECT fingerprint, agent_id, state, record, created_at, scope_id \
                    FROM {prefix}_publication_unscoped ORDER BY rowid; \
                 DROP TABLE {prefix}_publication_unscoped; \
                 CREATE INDEX {prefix}_publication_created_at_idx \
                    ON {prefix}_publication (created_at)",
            )?,
        ],
    )
}
