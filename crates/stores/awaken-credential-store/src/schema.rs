//! The credential schema (ADR-0043). One portable [`MigrationBundle`] under the
//! `credential` namespace with its own ledger. The `credential_source` rows are
//! **secret-free**; sealed secret material lives in `credential_secret` (written
//! by a sealed-AEAD `SecretStore` backend, P1). Its own bundle prefix is what lets
//! credential be split into its own database/service (blast-radius isolation).

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

/// Namespaced bundle id — the split/merge unit for the credential domain.
pub const BUNDLE_ID: &str = "awaken.credential";

const SPECS: [(i64, &str, &str); 4] = [
    (
        1,
        "credential sources: the secret-free row (kind + refs, never material)",
        "CREATE TABLE {prefix}_source (\
            id TEXT PRIMARY KEY, \
            workspace_id TEXT NOT NULL, \
            data {json} NOT NULL, \
            created_at {timestamptz} NOT NULL DEFAULT {now})",
    ),
    (
        2,
        "sealed secrets: ciphertext keyed by secret ref (sealed-AEAD backend, P1)",
        "CREATE TABLE {prefix}_secret (\
            secret_ref TEXT PRIMARY KEY, \
            sealed {blob} NOT NULL, \
            created_at {timestamptz} NOT NULL DEFAULT {now})",
    ),
    (
        3,
        "credential pools: secret-free failover groupings of sources",
        "CREATE TABLE {prefix}_pool (\
            id TEXT PRIMARY KEY, \
            workspace_id TEXT NOT NULL, \
            data {json} NOT NULL, \
            created_at {timestamptz} NOT NULL DEFAULT {now})",
    ),
    (
        4,
        "credential creation intents: secret-free crash-recovery journal",
        "CREATE TABLE {prefix}_creation_intent (\
            source_id TEXT PRIMARY KEY, \
            data {json} NOT NULL, \
            created_at {timestamptz} NOT NULL DEFAULT {now})",
    ),
];

/// Build the credential-schema migration bundle (prefix `credential`).
pub fn credential_bundle() -> Result<MigrationBundle, MigrationError> {
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
    fn credential_bundle_lints() {
        let bundle = credential_bundle().expect("bundle builds");
        awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).expect("bundle lints");
    }
}
