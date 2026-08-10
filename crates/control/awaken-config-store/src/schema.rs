//! Current Control config-store schema shared by PostgreSQL and SQLite.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

pub const BUNDLE_ID: &str = "awaken.config";

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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use awaken_scoped_migration::Migration;
    use awaken_scoped_migration_sqlite::SqliteMigrationRunner;
    use rusqlite::Connection;

    use super::{BUNDLE_ID, config_bundle};

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
}
