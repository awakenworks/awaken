//! The file-store schema, shared by the Postgres and SQLite backends (ADR-0043
//! scoped migration; foundation's `awaken-scoped-migration`). One portable
//! [`MigrationBundle`] under the `file_store` namespace with its own ledger
//! (`file_store_schema_migrations`), so the blob store coexists with other
//! schemas in a shared database. The **same** bundle — dialect-neutral tokens
//! (`{blob}`, `{timestamptz}`, `{now}`) — drives both runners; all schema changes
//! are additive migrations here, never a raw CREATE TABLE.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

/// Table namespace / bundle prefix: tables are `file_store_*`, the ledger is
/// `file_store_schema_migrations`.
pub const NS: &str = "file_store";

/// Bundle id for the file-store schema, scoped so it never collides with another
/// component's migrations in the same database.
pub const BUNDLE_ID: &str = "awaken.file_store";

/// The versioned schema bundle. Blob bytes and logical Files-API records are
/// deliberately separate tables: physical deduplication never merges public File
/// identity or metadata.
pub fn file_store_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        BUNDLE_ID,
        vec![
            Migration::new(
                1,
                "content-addressed blobs: one row per BLAKE3 content id",
                "CREATE TABLE {prefix}_blob (\
                 id TEXT PRIMARY KEY, \
                 bytes {blob} NOT NULL, \
                 size BIGINT NOT NULL, \
                 created_at {timestamptz} NOT NULL DEFAULT {now})",
            )?,
            Migration::new(
                2,
                "logical Files API records with durable scope and harvest identity",
                "CREATE TABLE {prefix}_file (\
                 id TEXT PRIMARY KEY, \
                 workspace_id TEXT NOT NULL, \
                 blob_id TEXT NOT NULL, \
                 filename TEXT NOT NULL, \
                 mime_type TEXT NOT NULL, \
                 size_bytes BIGINT NOT NULL, \
                 created_at TEXT NOT NULL, \
                 downloadable INTEGER NOT NULL, \
                 scope_id TEXT, \
                 logical_path TEXT, \
                 harvest_key TEXT, \
                 deleted INTEGER NOT NULL DEFAULT 0); \
                 CREATE UNIQUE INDEX {prefix}_file_workspace_harvest \
                 ON {prefix}_file(workspace_id, harvest_key) WHERE deleted=0; \
                 CREATE INDEX {prefix}_file_workspace_created \
                 ON {prefix}_file(workspace_id, created_at, id); \
                 CREATE INDEX {prefix}_file_scope \
                 ON {prefix}_file(workspace_id, scope_id, created_at, id)",
            )?,
            Migration::new(
                3,
                "optional GA Files download expiry",
                "ALTER TABLE {prefix}_file ADD COLUMN expires_at TEXT",
            )?,
            Migration::per_dialect(
                4,
                "preserve portable logical File flags as 64-bit integers",
                "ALTER TABLE {prefix}_file ALTER COLUMN downloadable TYPE BIGINT \
                 USING downloadable::BIGINT; \
                 ALTER TABLE {prefix}_file ALTER COLUMN deleted TYPE BIGINT \
                 USING deleted::BIGINT",
                "SELECT 1",
            )?,
            Migration::new(
                5,
                "terminal cleanup association on canonical artifact Files",
                "ALTER TABLE {prefix}_file ADD COLUMN artifact_idempotency_scope TEXT",
            )?,
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_store_bundle_lints() {
        let bundle = file_store_bundle().expect("bundle builds");
        awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).expect("bundle lints");
    }
}
