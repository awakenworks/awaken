//! The data-subject schema (ADR-0050). One portable [`MigrationBundle`] under
//! the `data_subject` namespace with its own ledger; the whole subject aggregate
//! (id, org, external_id, consents) serializes into the `data {json}` column.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

/// Namespaced bundle id — the split/merge unit for the data-subject domain.
pub const BUNDLE_ID: &str = "awaken.data_subject";

const SPECS: [(i64, &str, &str); 1] = [(
    1,
    "data subjects: the attributed party, its consent grants, keyed by org",
    "CREATE TABLE {prefix}_subject (\
        id TEXT PRIMARY KEY, \
        org TEXT NOT NULL, \
        data {json} NOT NULL, \
        created_at {timestamptz} NOT NULL DEFAULT {now})",
)];

/// Build the data-subject migration bundle (prefix `data_subject`).
pub fn data_subject_bundle() -> Result<MigrationBundle, MigrationError> {
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
    fn data_subject_bundle_lints() {
        let bundle = data_subject_bundle().expect("bundle builds");
        awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).expect("bundle lints");
    }
}
