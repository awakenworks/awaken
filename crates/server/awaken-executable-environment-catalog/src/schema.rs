use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

pub(crate) const NS: &str = "executable_environment";

pub fn executable_environment_catalog_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        "awaken.executable_environment_catalog",
        vec![Migration::new(
            1,
            "executable Environment registration and withdrawal command log",
            "CREATE TABLE {prefix}_command (\
                environment_id TEXT NOT NULL, \
                lifecycle_revision BIGINT NOT NULL, \
                command_kind TEXT NOT NULL, \
                command_json TEXT NOT NULL, \
                PRIMARY KEY (environment_id, lifecycle_revision, command_kind))",
        )?],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_is_deterministic_versioned_and_portable() {
        // Cause/effect design: one fresh scoped ledger -> exactly one deterministic
        // CREATE; an already-applied ledger is handled by the migration runner, not
        // conditional DDL inside the command.
        let bundle = executable_environment_catalog_bundle().unwrap();
        awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).unwrap();
    }
}
