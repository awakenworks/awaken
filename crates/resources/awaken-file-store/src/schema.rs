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

/// The versioned schema bundle. One migration: the content-addressed
/// `file_store_blob` table (id = BLAKE3 content hash, immutable `bytes`).
pub fn file_store_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        BUNDLE_ID,
        vec![Migration::new(
            1,
            "content-addressed blobs: one row per BLAKE3 content id",
            "CREATE TABLE {prefix}_blob (\
                 id TEXT PRIMARY KEY, \
                 bytes {blob} NOT NULL, \
                 size BIGINT NOT NULL, \
                 created_at {timestamptz} NOT NULL DEFAULT {now})",
        )?],
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
