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
        expanded::bundle()
    } else {
        config_bundle()
    }
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

    use awaken_scoped_migration::{Dialect, Migration, MigrationError, plan};
    use awaken_scoped_migration_sqlite::SqliteMigrationRunner;
    use rusqlite::Connection;

    use super::{
        BUNDLE_ID, config_bundle, converged_config_bundle, expanded, selected_config_bundle,
    };

    #[test]
    fn current_baseline_has_one_history_and_captures_exact_revisions() {
        /* Causes: C1 empty ledger; C2 insert generation 1; C3 update to
         * generation 2; C4 exact V1 receipt on replay. Effects: E1 create only
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
            [1]
        );
        let conn = Connection::open_in_memory().expect("sqlite");
        let runner = SqliteMigrationRunner::with_prefix("config").expect("runner");
        assert_eq!(runner.run_bundle(&conn, &bundle).expect("G1").len(), 1);
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
    fn published_config_histories_select_exact_v1_and_converge() {
        /* History decision table: H1 empty/current V1 -> compact baseline;
         * H2 expanded V1 -> exact V1..V10; H3 unknown V1 -> ordinary compact
         * checksum mismatch; H4 either terminal -> shared convergence only.
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
            expanded,
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
        // Both executable schemas contain trigger `NEW` pseudo-rows which the
        // generic cross-bundle scanner misclassifies as tables. The compact
        // storage test and expanded ten-checksum proof cover those bodies; only
        // the common future append stream is linted here.
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
