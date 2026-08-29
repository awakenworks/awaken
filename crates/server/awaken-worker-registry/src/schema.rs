use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

pub const BUNDLE_ID: &str = "awaken.worker_registry";
pub(crate) const CONVERGED_BUNDLE_ID: &str = "awaken.worker_registry.converged";
pub(crate) const NS: &str = "worker_registry";

const LEGACY_V1_CHECKSUM: &str = "c4c60e0da1143df05e79cfb07a6629681ab65c442e4ecd2901479442dad38f01";
#[cfg(test)]
const RELATIONAL_V1_CHECKSUM: &str =
    "f7af8451462f69c6d860d7438a31e280dd19025dcf5df72689b3d7f906f9f890";
const LEGACY_V1_SQL: &str = "CREATE TABLE {prefix}_worker (\
             worker_id TEXT PRIMARY KEY, \
             incarnation_id TEXT NOT NULL, \
             generation BIGINT NOT NULL, \
             state TEXT NOT NULL, \
             expires_at_ms BIGINT NOT NULL, \
             record_json TEXT NOT NULL);\
             CREATE INDEX {prefix}_expiry_idx ON {prefix}_worker (state, expires_at_ms)";
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
const RELATIONAL_V1_SQL: &str = "CREATE TABLE {prefix}_worker (\
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
             CREATE INDEX {prefix}_expiry_idx ON {prefix}_worker (state, expires_at_ms)";
const RELATIONAL_V3_POSTGRES: &str =
    include_str!("migrations/V0003__relational_worker_registry.postgres.sql");
const RELATIONAL_V3_SQLITE: &str =
    include_str!("migrations/V0003__relational_worker_registry.sqlite.sql");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PublishedRegistryStream {
    LegacyJson,
    Relational,
}

pub fn registry_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        BUNDLE_ID,
        vec![
            Migration::new(
                1,
                "current worker incarnation with durable generation and heartbeat tombstone",
                LEGACY_V1_SQL,
            )?,
            Migration::per_dialect(
                2,
                "enforce non-negative worker authority values",
                NONNEGATIVE_AUTHORITY_POSTGRES,
                NONNEGATIVE_AUTHORITY_SQLITE,
            )?,
            Migration::per_dialect(
                3,
                "replace the legacy JSON snapshot with one constrained relational worker authority",
                RELATIONAL_V3_POSTGRES.trim(),
                RELATIONAL_V3_SQLITE.trim(),
            )?,
        ],
    )
}

fn relational_registry_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        BUNDLE_ID,
        vec![Migration::new(
            1,
            "current relational worker directory authority",
            RELATIONAL_V1_SQL,
        )?],
    )
}

fn published_registry_stream(v1_checksum: Option<&str>) -> PublishedRegistryStream {
    if v1_checksum == Some(LEGACY_V1_CHECKSUM) || v1_checksum.is_none() {
        PublishedRegistryStream::LegacyJson
    } else {
        PublishedRegistryStream::Relational
    }
}

pub(crate) fn selected_registry_bundle(
    v1_checksum: Option<&str>,
) -> Result<MigrationBundle, MigrationError> {
    match published_registry_stream(v1_checksum) {
        PublishedRegistryStream::LegacyJson => registry_bundle(),
        PublishedRegistryStream::Relational => relational_registry_bundle(),
    }
}

pub(crate) fn converged_registry_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        CONVERGED_BUNDLE_ID,
        vec![Migration::new(
            1,
            "seal the converged worker registry history",
            "SELECT 1",
        )?],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_lints() {
        // Cause/effect rules: L1 every immutable history and the convergence
        // bundle use deterministic SQL -> each lints; L2 any conditional body,
        // duplicate version, or cross-bundle dependency -> fail before storage.
        for bundle in [
            registry_bundle().unwrap(),
            relational_registry_bundle().unwrap(),
            converged_registry_bundle().unwrap(),
        ] {
            awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).unwrap();
        }
    }

    #[test]
    fn published_worker_registry_histories_select_exact_receipts() {
        use std::collections::BTreeMap;

        use awaken_scoped_migration::{Dialect, MigrationError, plan};

        /* Published-history decision table:
         * W1 no receipt -> legacy V1..V3 (fresh final schema);
         * W2 legacy V1 checksum -> legacy suffix and preserved rows;
         * W3 relational V1 checksum -> relational terminal with no rewrite;
         * W4 unknown V1 checksum -> selected relational history rejects drift.
         * Both W2/W3 then append only the convergence receipt.
         */
        let legacy = registry_bundle().expect("legacy");
        let relational = relational_registry_bundle().expect("relational");
        assert_eq!(
            legacy.migrations()[0].checksum_for(Dialect::Sqlite),
            LEGACY_V1_CHECKSUM,
            "W1/W2 immutable legacy identity"
        );
        assert_eq!(
            relational.migrations()[0].checksum_for(Dialect::Sqlite),
            RELATIONAL_V1_CHECKSUM,
            "W3 immutable relational identity"
        );
        assert_eq!(
            published_registry_stream(None),
            PublishedRegistryStream::LegacyJson,
            "W1"
        );
        assert_eq!(
            published_registry_stream(Some(LEGACY_V1_CHECKSUM)),
            PublishedRegistryStream::LegacyJson,
            "W2"
        );
        assert_eq!(
            published_registry_stream(Some(RELATIONAL_V1_CHECKSUM)),
            PublishedRegistryStream::Relational,
            "W3"
        );
        let unknown = BTreeMap::from([(1, "f".repeat(64))]);
        assert!(
            matches!(
                plan(
                    &selected_registry_bundle(Some(&"f".repeat(64))).expect("W4 selection"),
                    &unknown,
                    Dialect::Sqlite,
                ),
                Err(MigrationError::ChecksumMismatch { version: 1, .. })
            ),
            "W4"
        );
    }

    #[test]
    fn sqlite_migrates_legacy_worker_json_atomically() {
        use awaken_scoped_migration::MigrationBundle;
        use awaken_scoped_migration_sqlite::SqliteMigrationRunner;
        use rusqlite::Connection;

        /* JSON cutover decision table:
         * J1 valid legacy snapshot -> one relational row with every authority
         * field preserved and V3 committed; J2 missing required manifest -> V3
         * fails and both legacy table/row and V1..V2 receipts remain; J3 terminal
         * V3 reopen -> no writes. The runner transaction is the atomic boundary.
         */
        let full = registry_bundle().expect("registry");
        let legacy_v2 =
            MigrationBundle::new(BUNDLE_ID, full.migrations()[..2].to_vec()).expect("legacy V2");
        let runner = SqliteMigrationRunner::with_prefix(NS).expect("runner");
        let valid = Connection::open_in_memory().expect("valid sqlite");
        runner
            .run_bundle(&valid, &legacy_v2)
            .expect("J1 seed schema");
        let record = serde_json::json!({
            "snapshot": {
                "state": "dead",
                "manifest": {"manifest_version": 1, "capabilities": []},
                "capability_fingerprint": "sha256:fixture",
                "in_flight": 2,
                "warm_environment_shapes": ["shape-a"],
                "credential_observations": [],
                "acp_capability_observations": [],
                "expires_at_ms": 20
            },
            "heartbeat_sequence": 7,
            "observation_sequence": 8,
            "registered_at_ms": 9,
            "heartbeat_at_ms": 10,
            "drain_deadline_ms": null
        });
        valid
            .execute(
                "INSERT INTO worker_registry_worker
                 (worker_id, incarnation_id, generation, state, expires_at_ms, record_json)
                 VALUES ('worker-a', 'incarnation-a', 3, 'dead', 20, ?1)",
                [record.to_string()],
            )
            .expect("J1 legacy row");
        assert_eq!(
            runner
                .run_bundle(&valid, &full)
                .expect("J1 migrate")
                .iter()
                .map(|migration| migration.version)
                .collect::<Vec<_>>(),
            [3],
            "J1"
        );
        let migrated = valid
            .query_row(
                "SELECT worker_id, incarnation_id, generation, state,
                        capability_fingerprint, in_flight, heartbeat_sequence,
                        observation_sequence, registered_at_ms, heartbeat_at_ms
                 FROM worker_registry_worker",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, i64>(8)?,
                        row.get::<_, i64>(9)?,
                    ))
                },
            )
            .expect("J1 relational row");
        assert_eq!(
            migrated,
            (
                "worker-a".into(),
                "incarnation-a".into(),
                3,
                "dead".into(),
                "sha256:fixture".into(),
                2,
                7,
                8,
                9,
                10,
            ),
            "J1 authority preservation"
        );
        assert!(
            runner
                .run_bundle(&valid, &full)
                .expect("J3 reopen")
                .is_empty()
        );

        let corrupt = Connection::open_in_memory().expect("corrupt sqlite");
        runner
            .run_bundle(&corrupt, &legacy_v2)
            .expect("J2 seed schema");
        corrupt
            .execute(
                "INSERT INTO worker_registry_worker
                 (worker_id, incarnation_id, generation, state, expires_at_ms, record_json)
                 VALUES ('worker-b', 'incarnation-b', 1, 'dead', 2, '{\"snapshot\":{\"state\":\"dead\"}}')",
                [],
            )
            .expect("J2 corrupt legacy row");
        assert!(runner.run_bundle(&corrupt, &full).is_err(), "J2 reject");
        assert_eq!(
            corrupt
                .query_row("SELECT COUNT(*) FROM worker_registry_worker", [], |row| row
                    .get::<_, i64>(0))
                .expect("J2 legacy row retained"),
            1,
            "J2 rollback"
        );
        assert_eq!(
            corrupt
                .query_row(
                    "SELECT MAX(version) FROM worker_registry_schema_migrations WHERE bundle_id = ?1",
                    [BUNDLE_ID],
                    |row| row.get::<_, i64>(0),
                )
                .expect("J2 ledger retained"),
            2,
            "J2 no V3 receipt"
        );
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
