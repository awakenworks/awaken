// @formal entrypoint config.generation_is_monotonic
// @formal evidence audit.business_action_follows_audit_record
// @formal evidence audit.business_commit_and_audit_transition_are_atomic
// @formal evidence audit.call_identity_is_stable
// @formal evidence audit.stable_call_replay_does_not_repeat_business_write
//! Current Control config-store schema shared by PostgreSQL and SQLite.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

mod expanded;

pub const BUNDLE_ID: &str = "awaken.config";
pub(crate) const CONVERGED_BUNDLE_ID: &str = "awaken.config.converged";

const POSTGRES_BASELINE: &str = include_str!("migrations/V0001__control_config.postgres.sql");
const SQLITE_BASELINE: &str = include_str!("migrations/V0001__control_config.sqlite.sql");

/// Build the one versioned config-schema stream. The two dialect bodies encode
/// only database syntax differences; both establish the same five Control-owned
/// tables and revision-capture behavior.
pub fn config_bundle() -> Result<MigrationBundle, MigrationError> {
    with_current_migrations(compact_baseline()?)
}

fn compact_baseline() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        BUNDLE_ID,
        vec![Migration::per_dialect(
            1,
            "complete Control Agent config and publication schema",
            POSTGRES_BASELINE.trim(),
            SQLITE_BASELINE.trim(),
        )?],
    )
}

pub(crate) fn selected_config_bundle(
    v1_checksum: Option<&str>,
) -> Result<MigrationBundle, MigrationError> {
    if v1_checksum == Some(expanded::V1_CHECKSUM) {
        with_current_migrations(expanded::bundle()?)
    } else {
        config_bundle()
    }
}

fn with_current_migrations(history: MigrationBundle) -> Result<MigrationBundle, MigrationError> {
    // Both immutable histories end before V11. Append once under their original
    // table owner; the already-published convergence bundle is a seal only.
    let mut migrations = history.migrations().to_vec();
    migrations.push(Migration::per_dialect(
        11,
        "management audit identity is the exact scoped tool and call tuple",
        include_str!("migrations/V0011__audit_identity.postgres.sql").trim(),
        include_str!("migrations/V0011__audit_identity.sqlite.sql").trim(),
    )?);
    migrations.push(Migration::per_dialect(
        12,
        "publication order is verified only after Workspace commit serialization",
        include_str!("migrations/V0012__publication_commit_order.postgres.sql").trim(),
        include_str!("migrations/V0012__publication_commit_order.sqlite.sql").trim(),
    )?);
    MigrationBundle::new(BUNDLE_ID, migrations)
}

pub(crate) fn converged_config_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        CONVERGED_BUNDLE_ID,
        vec![Migration::new(
            1,
            "seal the converged config migration history",
            "SELECT 1",
        )?],
    )
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use awaken_scoped_migration::{Dialect, Migration, MigrationBundle, MigrationError, plan};
    use awaken_scoped_migration_sqlite::SqliteMigrationRunner;
    use rusqlite::Connection;

    use super::{
        BUNDLE_ID, compact_baseline, config_bundle, converged_config_bundle, expanded,
        selected_config_bundle, with_current_migrations,
    };

    #[test]
    fn current_baseline_has_one_history_and_captures_exact_revisions() {
        /* Causes: C1 empty ledger; C2 insert generation 1; C3 update to
         * generation 2; C4 exact V1/V11/V12 receipts on replay. Effects: E1 create only
         * the five active Control tables; E2 append revision 1; E3 append
         * revision 2; E4 execute no SQL. Decision table: G1=C1=>E1;
         * G2=G1+C2=>E2; G3=G2+C3=>E3; G4=C4=>E4. */
        let bundle = config_bundle().expect("bundle");
        assert_eq!(
            bundle
                .migrations()
                .iter()
                .map(Migration::version)
                .collect::<Vec<_>>(),
            [1, 11, 12]
        );
        let conn = Connection::open_in_memory().expect("sqlite");
        let runner = SqliteMigrationRunner::with_prefix("config").expect("runner");
        assert_eq!(runner.run_bundle(&conn, &bundle).expect("G1").len(), 3);
        let tables = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'config_%' \
                 AND name NOT LIKE 'config_schema_migrations%' ORDER BY name",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<BTreeSet<_>, _>>()
            .unwrap();
        assert_eq!(
            tables,
            BTreeSet::from([
                "config_agent".to_owned(),
                "config_agent_revision".to_owned(),
                "config_management_audit".to_owned(),
                "config_management_effect".to_owned(),
                "config_publication".to_owned(),
            ]),
            "G1/E1"
        );
        conn.execute(
            "INSERT INTO config_agent(id,data,scope_id,generation) VALUES('agent','{}','ws',1)",
            [],
        )
        .expect("G2");
        conn.execute(
            r#"UPDATE config_agent SET data='{"v":2}', generation=2
               WHERE scope_id='ws' AND id='agent'"#,
            [],
        )
        .expect("G3");
        let revisions = conn
            .prepare(
                "SELECT generation FROM config_agent_revision \
                 WHERE scope_id='ws' AND id='agent' ORDER BY generation",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(revisions, [1, 2], "G2/E2 + G3/E3");
        assert!(runner.run_bundle(&conn, &bundle).expect("G4").is_empty());
        assert_eq!(bundle.bundle_id(), BUNDLE_ID);
    }

    #[test]
    fn sqlite_publication_order_migration_marks_old_rows_verified() {
        // Causes: S1 a V11 SQLite publication predates the marker column;
        // S2 V12 is applied; S3 migration is replayed. Effects: E1 the old
        // row receives verified order because SQLite writes were serialized;
        // E2 repeat migration leaves that same row unchanged.
        // Rules S1+S2=>E1, S1+S2+S3=>E2.
        let current = config_bundle().unwrap();
        let old = MigrationBundle::new(BUNDLE_ID, current.migrations()[..2].to_vec()).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        let runner = SqliteMigrationRunner::with_prefix("config").unwrap();
        runner.run_bundle(&conn, &old).unwrap();
        conn.execute(
            "INSERT INTO config_publication (fingerprint, agent_id, state, record, scope_id) \
             VALUES ('fp', 'agent', 'published', '{}', 'ws')",
            [],
        )
        .unwrap();
        assert_eq!(runner.run_bundle(&conn, &current).unwrap().len(), 1, "E1");
        let verified: i64 = conn
            .query_row(
                "SELECT order_verified FROM config_publication WHERE fingerprint = 'fp'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(verified, 1, "E1");
        assert!(runner.run_bundle(&conn, &current).unwrap().is_empty(), "E2");
    }

    #[test]
    fn audit_identity_migration_preserves_both_histories_and_rejects_corruption() {
        // M1 compact/expanded history x pending/committed => preserve record,
        // state and timestamp; replay migration => no SQL. M2 alternate tuple
        // with the same legacy key => independent row. M3 mismatched indexed
        // coordinates or invalid state => DB rejects. M4 corrupt/duplicate legacy row =>
        // atomic rollback, old bytes and ledger intact, no shadow table remains.
        for history in [compact_baseline().unwrap(), expanded::bundle().unwrap()] {
            let current = with_current_migrations(history.clone()).unwrap();
            for (state, valid_record, duplicate) in [
                (0_i64, true, false),
                (1, true, false),
                (2, true, false),
                (0, false, false),
                (0, true, true),
            ] {
                let conn = Connection::open_in_memory().unwrap();
                let runner = SqliteMigrationRunner::with_prefix("config").unwrap();
                runner.run_bundle(&conn, &history).unwrap();
                let record =
                    r#"{ "summary":"body_sha256=one", "call_id":"b:调用", "tool":"http:POST:a" }"#;
                let record = if valid_record { record } else { "{}" };
                conn.execute("INSERT INTO config_management_audit(scope_id,call_id,record,business_committed,created_at) VALUES('ws','http:POST:a:b:调用',?1,?2,'2026-01-01 00:00:00')", rusqlite::params![record, state]).unwrap();
                if duplicate {
                    conn.execute("INSERT INTO config_management_audit(scope_id,call_id,record) VALUES('ws','other-legacy-key',?1)", [record]).unwrap();
                }
                let result = runner.run_bundle(&conn, &current);
                if state == 2 || !valid_record || duplicate {
                    assert!(result.is_err(), "M4");
                    let (key, unchanged): (String, String) = conn
                        .query_row(
                            "SELECT call_id,record FROM config_management_audit WHERE call_id='http:POST:a:b:调用'",
                            [],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .unwrap();
                    assert_eq!(key, "http:POST:a:b:调用");
                    assert_eq!(unchanged, record);
                    let rows: i64 = conn
                        .query_row("SELECT COUNT(*) FROM config_management_audit", [], |r| {
                            r.get(0)
                        })
                        .unwrap();
                    assert_eq!(rows, 1 + i64::from(duplicate));
                    let receipt: i64 = conn.query_row("SELECT COUNT(*) FROM config_schema_migrations WHERE bundle_id='awaken.config' AND version=11", [], |r| r.get(0)).unwrap();
                    assert_eq!(receipt, 0);
                    let shadow: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name='config_management_audit_identity'", [], |r| r.get(0)).unwrap();
                    assert_eq!(shadow, 0);
                    continue;
                }
                result.unwrap();
                let row: (String, String, String, i64, String) = conn.query_row("SELECT tool,call_id,record,business_committed,created_at FROM config_management_audit", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
                assert_eq!(
                    row,
                    (
                        "http:POST:a".into(),
                        "b:调用".into(),
                        record.into(),
                        state,
                        "2026-01-01 00:00:00".into()
                    ),
                    "M1"
                );
                assert!(runner.run_bundle(&conn, &current).unwrap().is_empty());
                conn.execute("INSERT INTO config_management_audit(scope_id,tool,call_id,record) VALUES('ws','http:POST','a:b:调用',?1)", [r#"{"tool":"http:POST","call_id":"a:b:调用","summary":"different"}"#]).expect("M2");
                for sql in [
                    "UPDATE config_management_audit SET tool='wrong'",
                    "UPDATE config_management_audit SET call_id='wrong'",
                    "UPDATE config_management_audit SET business_committed=-1",
                    "UPDATE config_management_audit SET record='{}'",
                ] {
                    assert!(conn.execute(sql, []).is_err(), "M3: {sql}");
                }
            }
        }
    }

    #[test]
    fn published_config_histories_select_exact_v1_and_converge() {
        /* History decision table: H1 empty/current V1 -> compact + common V11/V12;
         * H2 expanded V1 -> exact V1..V10 + same V11/V12; H3 unknown V1 -> compact
         * checksum mismatch; H4 either current history -> unchanged seal.
         */
        let compact = config_bundle().expect("compact");
        let expanded = expanded::bundle().expect("expanded");
        let compact_v1 = compact.migrations()[0].checksum_for(Dialect::Sqlite);
        assert_eq!(selected_config_bundle(None).expect("H1"), compact, "H1");
        assert_eq!(
            selected_config_bundle(Some(&compact_v1)).expect("H1 compact"),
            compact,
            "H1"
        );
        assert_eq!(
            selected_config_bundle(Some(expanded::V1_CHECKSUM)).expect("H2"),
            with_current_migrations(expanded.clone()).unwrap(),
            "H2"
        );
        let sqlite_receipts = [
            "3134e209014343fdb07c7e57bedbdd42a27fab4017e9b2c6a7a69f3d68dcf588",
            "76f0182f64e73c863acb737c25f3507d0e81ec01d5c6967331dc3c9ef3a9aab6",
            "fe8e6854ed3e22e3f8940cf64f2c1064b4a856bba57e6a38e2972c92f8340851",
            "5f7faf0e50e577be972afdf0f32dc1140f6cf00edf2c7db8b5808fd27179d22f",
            "fa0453638c9a39e09673d843eef2b37e1bf17363e69c0b3f099394c7e532e729",
            "00dec487869e300b97c26a44898b0fb7af0da0d438eff69afe7c177b7f8d7a0d",
            "ca632a2fcd446c6f0fde5f581f7d4be2be0ed4bea94e9d80646ed712200d97c9",
            "c5729c8a7e39fc422fed94bdc1892ad9b1e5e652595e89e2e0cc9903ea2427c8",
            "aba2c4fb899bbf493dd6e5aea7e682b3baa313e6d022af5b98ffbca7868a3684",
            "02abc971b170592cb4e294c6c3458201eb5641e3fd538a31948dc06aa6872aba",
        ];
        for (migration, expected) in expanded.migrations().iter().zip(sqlite_receipts) {
            assert_eq!(migration.checksum_for(Dialect::Sqlite), expected, "H2");
        }
        // The inherited revision triggers contain NEW pseudo-rows unsupported
        // by the pinned table scanner. Lint the exact audit ownership slice:
        // published V7 declaration + current V11, with no hand-written DDL or
        // cross-bundle exemption. Executable migration tests cover both complete
        // histories and V12's publication-table upgrade; the old convergence
        // receipt remains an unchanged seal.
        awaken_scoped_migration::lint(&[MigrationBundle::new(
            BUNDLE_ID,
            vec![
                expanded.migrations()[6].clone(),
                compact.migrations()[1].clone(),
            ],
        )
        .unwrap()])
        .expect("audit migration ownership");
        awaken_scoped_migration::lint(std::slice::from_ref(
            &converged_config_bundle().expect("converged"),
        ))
        .expect("convergence lint");
        let unknown = BTreeMap::from([(1, "f".repeat(64))]);
        assert!(matches!(
            plan(
                &selected_config_bundle(Some(&"f".repeat(64))).expect("H3 select"),
                &unknown,
                Dialect::Sqlite,
            ),
            Err(MigrationError::ChecksumMismatch { version: 1, .. })
        ));
    }
}
