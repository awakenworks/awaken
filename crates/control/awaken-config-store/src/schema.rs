//! The config-store schema, shared by the Postgres and SQLite backends.
//!
//! One portable [`MigrationBundle`] under the `config` namespace, so it coexists
//! with the runtime's `runtime_*` tables and its own ledger
//! (`config_schema_migrations`) in one database (ADR-0029/ADR-0031).

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

/// Bundle id for the config schema. Scoped so it never collides with the commit
/// or dispatch schemas in a shared database.
pub const BUNDLE_ID: &str = "awaken.config";

const SPECS: [(i64, &str, &str); 4] = [
    (
        1,
        "agent configs: the authoring aggregate, one row per agent id",
        "CREATE TABLE {prefix}_agent (\
            id TEXT PRIMARY KEY, \
            data {json} NOT NULL, \
            created_at {timestamptz} NOT NULL DEFAULT {now})",
    ),
    (
        2,
        "publications: the compiled artifact, content-addressed by fingerprint",
        "CREATE TABLE {prefix}_publication (\
            fingerprint TEXT PRIMARY KEY, \
            agent_id TEXT NOT NULL, \
            state TEXT NOT NULL, \
            record {json} NOT NULL, \
            created_at {timestamptz} NOT NULL DEFAULT {now})",
    ),
    (
        // Tenancy edge aspect (ADR-0051 D2/D4): one opaque owner `scope_id` on the
        // authoring aggregate. Additive with a seeded default so existing rows stay
        // valid; reads filter `WHERE scope_id = ?` and writes are scope-guarded, so
        // a workspace cannot read or clobber another's agent by id.
        3,
        "agent configs: opaque owner scope_id (ADR-0051)",
        "ALTER TABLE {prefix}_agent ADD COLUMN scope_id TEXT NOT NULL DEFAULT 'default'",
    ),
    (
        4,
        "publications: opaque owner scope_id (ADR-0051)",
        "ALTER TABLE {prefix}_publication ADD COLUMN scope_id TEXT NOT NULL DEFAULT 'default'",
    ),
];

/// Build the config-schema migration bundle.
pub fn config_bundle() -> Result<MigrationBundle, MigrationError> {
    let migrations = SPECS
        .iter()
        .map(|(version, description, sql)| Migration::new(*version, *description, *sql))
        .collect::<Result<Vec<_>, _>>()?;
    MigrationBundle::new(BUNDLE_ID, migrations)
}
