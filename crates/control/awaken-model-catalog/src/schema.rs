//! The model-catalog schema (ADR-0043). One portable [`MigrationBundle`] under the
//! `catalog` namespace with its own ledger (`catalog_schema_migrations`), so it
//! coexists with — or is split apart from — the other domains' schemas in one or
//! many databases (the "可分可合" property: one bundle per domain prefix).

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

/// Namespaced bundle id — never collides with `awaken.config`/`awaken.credential`/
/// the runtime `awaken.commit` schemas in a shared database.
pub const BUNDLE_ID: &str = "awaken.catalog";

const SPECS: [(i64, &str, &str); 4] = [
    (
        1,
        "providers: one row per vendor",
        "CREATE TABLE {prefix}_provider (\
            id TEXT PRIMARY KEY, \
            data {json} NOT NULL, \
            created_at {timestamptz} NOT NULL DEFAULT {now})",
    ),
    (
        2,
        "protocol endpoints: a wire surface + URL of a provider",
        "CREATE TABLE {prefix}_protocol_endpoint (\
            id TEXT PRIMARY KEY, \
            provider_id TEXT NOT NULL, \
            data {json} NOT NULL, \
            created_at {timestamptz} NOT NULL DEFAULT {now})",
    ),
    (
        3,
        "offerings: a model reachable on an endpoint (secret-free)",
        "CREATE TABLE {prefix}_offering (\
            model_id TEXT NOT NULL, \
            protocol_endpoint_id TEXT NOT NULL, \
            data {json} NOT NULL, \
            PRIMARY KEY (model_id, protocol_endpoint_id))",
    ),
    (
        4,
        "model attributes: intrinsic per-model_id properties (context_window, …)",
        "CREATE TABLE {prefix}_model_attributes (\
            model_id TEXT PRIMARY KEY, \
            data {json} NOT NULL)",
    ),
];

/// Build the catalog-schema migration bundle (prefix `catalog`).
pub fn catalog_bundle() -> Result<MigrationBundle, MigrationError> {
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
    fn catalog_bundle_lints() {
        let bundle = catalog_bundle().expect("bundle builds");
        awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).expect("bundle lints");
    }
}
