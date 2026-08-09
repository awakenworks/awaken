use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

pub(crate) const NS: &str = "environment_image_build";

pub fn environment_image_build_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        "awaken.environment_image_build",
        vec![Migration::new(
            1,
            "Environment image-build jobs",
            "CREATE TABLE {prefix}_job (\
                build_key TEXT PRIMARY KEY, \
                version BIGINT NOT NULL, \
                demand_json TEXT NOT NULL, \
                state_kind TEXT NOT NULL, \
                state_json TEXT NOT NULL, \
                updated_at_ms BIGINT NOT NULL)",
        )?],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_build_schema_has_one_portable_owner() {
        // One portable job bundle is the schema source of truth for both SQL
        // adapters; backend modules own only placeholder and connection syntax.
        let bundle = environment_image_build_bundle().unwrap();
        awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).unwrap();
    }
}
