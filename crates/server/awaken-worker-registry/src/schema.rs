use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

pub const BUNDLE_ID: &str = "awaken.worker_registry";
pub(crate) const NS: &str = "worker_registry";

const NONNEGATIVE_AUTHORITY_POSTGRES: &str = "\
    ALTER TABLE {prefix}_worker \
    ADD CONSTRAINT {prefix}_worker_nonnegative_authority \
    CHECK (generation >= 0 AND expires_at_ms >= 0)";

const NONNEGATIVE_AUTHORITY_SQLITE: &str = "\
    CREATE TRIGGER {prefix}_worker_nonnegative_authority_insert \
    BEFORE INSERT ON {prefix}_worker \
    WHEN NEW.generation < 0 OR NEW.expires_at_ms < 0 BEGIN \
        SELECT RAISE(ABORT, 'worker authority values must be non-negative'); \
    END; \
    CREATE TRIGGER {prefix}_worker_nonnegative_authority_update \
    BEFORE UPDATE OF generation, expires_at_ms ON {prefix}_worker \
    WHEN NEW.generation < 0 OR NEW.expires_at_ms < 0 BEGIN \
        SELECT RAISE(ABORT, 'worker authority values must be non-negative'); \
    END; \
    UPDATE {prefix}_worker \
    SET generation = generation, expires_at_ms = expires_at_ms";

pub fn registry_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        BUNDLE_ID,
        vec![
            Migration::new(
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
            )?,
            Migration::per_dialect(
                2,
                "enforce non-negative worker authority values",
                NONNEGATIVE_AUTHORITY_POSTGRES,
                NONNEGATIVE_AUTHORITY_SQLITE,
            )?,
        ],
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

    #[test]
    fn sqlite_rejects_negative_worker_authority() {
        use awaken_scoped_migration_sqlite::SqliteMigrationRunner;
        use rusqlite::Connection;

        let conn = Connection::open_in_memory().unwrap();
        let bundle = registry_bundle().unwrap();
        let runner = SqliteMigrationRunner::with_prefix(NS).unwrap();
        runner.run_bundle(&conn, &bundle).unwrap();

        assert!(
            conn.execute(
                "INSERT INTO worker_registry_worker
                     (worker_id, incarnation_id, generation, state, expires_at_ms, record_json)
                 VALUES ('negative', 'incarnation', -1, 'ready', 1, '{}')",
                [],
            )
            .is_err()
        );
        conn.execute(
            "INSERT INTO worker_registry_worker
                 (worker_id, incarnation_id, generation, state, expires_at_ms, record_json)
             VALUES ('valid', 'incarnation', 1, 'ready', 1, '{}')",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "UPDATE worker_registry_worker SET expires_at_ms = -1 WHERE worker_id = 'valid'",
                [],
            )
            .is_err()
        );
    }

    #[test]
    fn sqlite_constraint_migration_rolls_back_on_corrupt_existing_rows() {
        use awaken_scoped_migration::MigrationBundle;
        use awaken_scoped_migration_sqlite::SqliteMigrationRunner;
        use rusqlite::Connection;

        let conn = Connection::open_in_memory().unwrap();
        let full = registry_bundle().unwrap();
        let v1 = MigrationBundle::new(BUNDLE_ID, full.migrations()[..1].to_vec()).unwrap();
        let runner = SqliteMigrationRunner::with_prefix(NS).unwrap();
        runner.run_bundle(&conn, &v1).unwrap();
        conn.execute(
            "INSERT INTO worker_registry_worker
                 (worker_id, incarnation_id, generation, state, expires_at_ms, record_json)
             VALUES ('corrupt', 'incarnation', -1, 'ready', 1, '{}')",
            [],
        )
        .unwrap();

        assert!(runner.run_bundle(&conn, &full).is_err());
        conn.execute(
            "UPDATE worker_registry_worker SET generation = 0 WHERE worker_id = 'corrupt'",
            [],
        )
        .unwrap();
        assert_eq!(runner.run_bundle(&conn, &full).unwrap().len(), 1);
    }
}
