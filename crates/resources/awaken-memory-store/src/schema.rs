//! The memory-store schema. One portable [`MigrationBundle`] under the
//! `memory_store` namespace; a blob is `(workspace_id, id) → content` bytes plus a
//! per-workspace `ordinal` for dense id minting. The same bundle renders on sqlite
//! and postgres ({blob} → BLOB / bytea) — the schema is written once.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

/// Namespaced bundle id — the split/merge unit for the memory-store domain.
pub const BUNDLE_ID: &str = "awaken.memory_store";

const SPECS: [(i64, &str, &str); 1] = [(
    1,
    "memory blobs: id-keyed bytes per workspace, with a dense-id ordinal",
    "CREATE TABLE {prefix}_blob (\
        workspace_id TEXT NOT NULL, \
        id TEXT NOT NULL, \
        ordinal BIGINT NOT NULL, \
        content {blob} NOT NULL, \
        PRIMARY KEY (workspace_id, id))",
)];

/// Build the memory-store migration bundle (prefix `memory_store`).
pub fn memory_store_bundle() -> Result<MigrationBundle, MigrationError> {
    let migrations = SPECS
        .iter()
        .map(|(version, description, sql)| Migration::new(*version, *description, *sql))
        .collect::<Result<Vec<_>, _>>()?;
    MigrationBundle::new(BUNDLE_ID, migrations)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_bundle_lints() {
        let bundle = memory_store_bundle().expect("bundle builds");
        awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).expect("bundle lints");
    }
}
