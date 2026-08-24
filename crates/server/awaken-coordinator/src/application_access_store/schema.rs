//! Portable schema authority for Coordinator application access.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

pub(super) const NAMESPACE: &str = "application_access";

pub(super) fn bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        "awaken.coordinator.application_access",
        vec![Migration::new(
            1,
            "durable short-lived application capabilities",
            "CREATE TABLE {prefix}_credential (\
                token_id TEXT PRIMARY KEY, \
                token_hash TEXT NOT NULL UNIQUE, \
                workspace_id TEXT NOT NULL, \
                created_at_unix_ms BIGINT NOT NULL, \
                expires_at_unix_ms BIGINT NOT NULL, \
                revoked_at_unix_ms BIGINT, \
                grant_json TEXT NOT NULL); \
             CREATE INDEX {prefix}_credential_expiry \
                ON {prefix}_credential(expires_at_unix_ms); \
             CREATE INDEX {prefix}_credential_revocation \
                ON {prefix}_credential(revoked_at_unix_ms)",
        )?],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_access_schema_has_one_dense_authority_stream() {
        // Cause/effect rule DB-R1: one application credential aggregate (C1)
        // and no Session/IAM shadow tables (C2) yield exactly one V1 migration
        // (E1) under the Coordinator-owned namespace.
        let bundle = bundle().expect("application access bundle");
        assert_eq!(
            bundle
                .migrations()
                .iter()
                .map(Migration::version)
                .collect::<Vec<_>>(),
            vec![1],
            "DB-R1"
        );
        let sql = bundle.migrations()[0].sql_for(awaken_scoped_migration::Dialect::Sqlite);
        assert!(!sql.contains("session"), "DB-R1");
        assert!(!sql.contains("role"), "DB-R1");
        assert!(!sql.contains("secret "), "DB-R1");
        assert!(!sql.contains("authority_id"), "DB-R1");
        assert!(!sql.contains("application_scope"), "DB-R1");
        assert!(!sql.contains("actor_key"), "DB-R1");
    }
}
