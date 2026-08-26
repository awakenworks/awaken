use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

pub const BUNDLE_ID: &str = "awaken.worker_registry";
pub(crate) const NS: &str = "worker_registry";

pub fn registry_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        BUNDLE_ID,
        vec![Migration::new(
            1,
            "current relational worker directory authority",
            "CREATE TABLE {prefix}_worker (\
             worker_id TEXT PRIMARY KEY CHECK (length(worker_id) > 0), \
             incarnation_id TEXT NOT NULL CHECK (length(incarnation_id) > 0), \
             generation BIGINT NOT NULL CHECK (generation >= 0), \
             state TEXT NOT NULL CHECK (state IN ('starting', 'ready', 'draining', 'quiesced', 'dead')), \
             manifest_json TEXT NOT NULL, \
             capability_fingerprint TEXT NOT NULL, \
             in_flight BIGINT NOT NULL CHECK (in_flight >= 0 AND in_flight <= 4294967295), \
             warm_environment_shapes_json TEXT NOT NULL, \
             credential_observations_json TEXT NOT NULL, \
             acp_capability_observations_json TEXT NOT NULL, \
             expires_at_ms BIGINT NOT NULL CHECK (expires_at_ms >= 0), \
             heartbeat_sequence BIGINT NOT NULL CHECK (heartbeat_sequence >= 0), \
             observation_sequence BIGINT NOT NULL CHECK (observation_sequence >= 0), \
             registered_at_ms BIGINT NOT NULL CHECK (registered_at_ms >= 0), \
             heartbeat_at_ms BIGINT NOT NULL CHECK (heartbeat_at_ms >= 0), \
             drain_deadline_ms BIGINT CHECK (drain_deadline_ms IS NULL OR drain_deadline_ms >= 0));\
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

    #[test]
    fn sqlite_rejects_invalid_worker_authority() {
        use awaken_scoped_migration_sqlite::SqliteMigrationRunner;
        use rusqlite::Connection;

        let conn = Connection::open_in_memory().unwrap();
        let bundle = registry_bundle().unwrap();
        let runner = SqliteMigrationRunner::with_prefix(NS).unwrap();
        runner.run_bundle(&conn, &bundle).unwrap();

        assert!(
            conn.execute(
                "INSERT INTO worker_registry_worker
                     (worker_id, incarnation_id, generation, state, manifest_json,
                      capability_fingerprint, in_flight, warm_environment_shapes_json,
                      credential_observations_json, acp_capability_observations_json,
                      expires_at_ms, heartbeat_sequence, observation_sequence,
                      registered_at_ms, heartbeat_at_ms)
                 VALUES ('negative', 'incarnation', -1, 'ready', '{}', 'fingerprint', 0,
                         '[]', '[]', '[]', 1, 0, 0, 0, 0)",
                [],
            )
            .is_err()
        );
        conn.execute(
            "INSERT INTO worker_registry_worker
                 (worker_id, incarnation_id, generation, state, manifest_json,
                  capability_fingerprint, in_flight, warm_environment_shapes_json,
                  credential_observations_json, acp_capability_observations_json,
                  expires_at_ms, heartbeat_sequence, observation_sequence,
                  registered_at_ms, heartbeat_at_ms)
             VALUES ('valid', 'incarnation', 1, 'ready', '{}', 'fingerprint', 0,
                     '[]', '[]', '[]', 1, 0, 0, 0, 0)",
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
}
