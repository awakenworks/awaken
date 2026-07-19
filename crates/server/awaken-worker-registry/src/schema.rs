use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

pub const BUNDLE_ID: &str = "awaken.worker_registry";
pub(crate) const NS: &str = "worker_registry";

pub fn registry_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        BUNDLE_ID,
        vec![Migration::new(
            1,
            "current worker incarnation with durable generation and heartbeat tombstone",
            "CREATE TABLE {prefix}_worker (\
             worker_id TEXT PRIMARY KEY, \
             incarnation_id TEXT NOT NULL, \
             generation BIGINT NOT NULL, \
             state TEXT NOT NULL, \
             expires_at_ms BIGINT NOT NULL, \
             record_json TEXT NOT NULL);\
             CREATE INDEX {prefix}_expiry_idx ON {prefix}_worker (state, expires_at_ms)",
        )?],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_lints() {
        let bundle = registry_bundle().unwrap();
        awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).unwrap();
    }
}
