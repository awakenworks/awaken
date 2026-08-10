//! The durable dispatch schema, shared by the Postgres and SQLite backends.
//!
//! One [`MigrationBundle`] drives both runners. Portable migrations use the
//! migrator's dialect-neutral tokens; the final storage invariant uses the
//! migrator's explicit per-dialect escape hatch because SQLite cannot add a
//! table constraint after creation. The DDL itself is NOT encoded in this
//! source: every body is a `.sql` file under `migrations/`, embedded at build
//! time with `include_str!` and turned into a [`Migration`] here.
//!
//! Two tables back the two aggregates: `{prefix}_dispatch` is the run-dispatch
//! queue (one row per accepted run with its claim/lease state) and
//! `{prefix}_pending` is the thread's pending input; `{prefix}_outbox` stages
//! cross-thread deliveries. The remaining migrations index the claim/lease/pending
//! hot paths.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

/// Bundle id for the durable dispatch schema. Scoped so it never collides with
/// the commit schema (`awaken.runtime_commit`) in a shared database.
pub const BUNDLE_ID: &str = "awaken.run_dispatch";

/// The embedded migration files, in apply order. Each entry is
/// `(file_name, file_contents)`: the name yields the version, the contents yield
/// the description (first `-- comment` line) and the SQL body. `include_str!`
/// resolves relative to this source file, so the `.sql` files ship in the crate.
const FILES: &[(&str, &str)] = &[
    (
        "V0001__dispatch_queue.sql",
        include_str!("migrations/V0001__dispatch_queue.sql"),
    ),
    (
        "V0002__pending_input.sql",
        include_str!("migrations/V0002__pending_input.sql"),
    ),
    (
        "V0003__cross_thread_outbox.sql",
        include_str!("migrations/V0003__cross_thread_outbox.sql"),
    ),
    (
        "V0004__dispatch_claim_idx.sql",
        include_str!("migrations/V0004__dispatch_claim_idx.sql"),
    ),
    (
        "V0005__dispatch_lease_idx.sql",
        include_str!("migrations/V0005__dispatch_lease_idx.sql"),
    ),
    (
        "V0006__dispatch_owner_idx.sql",
        include_str!("migrations/V0006__dispatch_owner_idx.sql"),
    ),
    (
        "V0007__dispatch_thread_idx.sql",
        include_str!("migrations/V0007__dispatch_thread_idx.sql"),
    ),
    (
        "V0008__dispatch_dedupe_idx.sql",
        include_str!("migrations/V0008__dispatch_dedupe_idx.sql"),
    ),
    (
        "V0009__pending_run_idx.sql",
        include_str!("migrations/V0009__pending_run_idx.sql"),
    ),
    (
        "V0010__pending_thread_idx.sql",
        include_str!("migrations/V0010__pending_thread_idx.sql"),
    ),
    (
        "V0011__dispatch_sandbox_binding.sql",
        include_str!("migrations/V0011__dispatch_sandbox_binding.sql"),
    ),
    (
        "V0012__dispatch_one_running_per_thread.sql",
        include_str!("migrations/V0012__dispatch_one_running_per_thread.sql"),
    ),
    (
        "V0013__dispatch_lease_epoch.sql",
        include_str!("migrations/V0013__dispatch_lease_epoch.sql"),
    ),
    (
        "V0014__normalize_awaiting_state.sql",
        include_str!("migrations/V0014__normalize_awaiting_state.sql"),
    ),
    (
        "V0015__dispatch_worker_assignment.sql",
        include_str!("migrations/V0015__dispatch_worker_assignment.sql"),
    ),
    (
        "V0016__dispatch_completion.sql",
        include_str!("migrations/V0016__dispatch_completion.sql"),
    ),
    (
        "V0017__stream_checkpoint.sql",
        include_str!("migrations/V0017__stream_checkpoint.sql"),
    ),
    (
        "V0018__durable_cancellation_intent.sql",
        include_str!("migrations/V0018__durable_cancellation_intent.sql"),
    ),
    (
        "V0019__dispatch_operational_feed.sql",
        include_str!("migrations/V0019__dispatch_operational_feed.sql"),
    ),
    (
        "V0020__dispatch_attempt_credentials.sql",
        include_str!("migrations/V0020__dispatch_attempt_credentials.sql"),
    ),
    (
        "V0021__dispatch_operation_recorded_at.sql",
        include_str!("migrations/V0021__dispatch_operation_recorded_at.sql"),
    ),
    (
        "V0022__normalize_signed_millis.sql",
        include_str!("migrations/V0022__normalize_signed_millis.sql"),
    ),
];

const NONNEGATIVE_AUTHORITY_POSTGRES: &str =
    include_str!("migrations/V0023__nonnegative_authority.postgres.sql");
const NONNEGATIVE_AUTHORITY_SQLITE: &str =
    include_str!("migrations/V0023__nonnegative_authority.sqlite.sql");

/// Parse the version from a `Vnnnn__slug.sql` file name (`V0004__…` ⇒ 4). A name
/// that does not carry a positive version yields `0`, which [`Migration::new`]
/// rejects — so a mis-named file fails the bundle build loudly.
fn version_of(name: &str) -> i64 {
    name.trim_start_matches('V')
        .split("__")
        .next()
        .and_then(|digits| digits.parse::<i64>().ok())
        .unwrap_or(0)
}

/// The migration's description: the first `-- comment` line of the file, so the
/// human-readable summary lives with the DDL rather than in this source.
fn description_of(name: &str, contents: &str) -> String {
    contents
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("--").map(|rest| rest.trim().to_string()))
        .filter(|desc| !desc.is_empty())
        .unwrap_or_else(|| name.to_string())
}

/// Build the dispatch-schema migration bundle from the embedded `.sql` files.
pub fn dispatch_bundle() -> Result<MigrationBundle, MigrationError> {
    let mut migrations = FILES
        .iter()
        .map(|(name, contents)| {
            Migration::new(
                version_of(name),
                description_of(name, contents),
                contents.trim(),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    migrations.push(Migration::per_dialect(
        23,
        "enforce non-negative durable authority counters",
        NONNEGATIVE_AUTHORITY_POSTGRES.trim(),
        NONNEGATIVE_AUTHORITY_SQLITE.trim(),
    )?);
    MigrationBundle::new(BUNDLE_ID, migrations)
}

/// The runtime table prefix, mirrored from `postgres::NS`/`sqlite::NS` so the
/// render test can assert the prefixing without reaching into a backend module.
#[cfg(test)]
const NS: &str = "runtime";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_bundle_lints_clean() {
        // Causes: C1 every physical filename equals its registered identity; C2
        // V1..V22 are deterministic portable bodies; C3 V23 is one explicit
        // dialect pair. Effect E1 one lint-clean stream; any identity mismatch,
        // conditional body, or cross-bundle reference fails before connection.
        // Decision rule D1=C1+C2+C3=>E1.
        let bundle = dispatch_bundle().expect("bundle builds");
        awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).expect("bundle lints");
    }

    #[test]
    fn versions_parse_from_file_names() {
        let bundle = dispatch_bundle().expect("bundle builds");
        // Decision table: D1 empty ledger -> exact dense V1..V23; D2 exact
        // prefix -> only its suffix; D3 duplicate/gap -> bundle rejection.
        let versions: Vec<i64> = bundle.migrations().iter().map(|m| m.version()).collect();
        assert_eq!(versions, (1..=23).collect::<Vec<_>>());
    }

    #[test]
    fn v22_normalizes_legacy_negative_millis_during_forward_migration() {
        use awaken_scoped_migration::MigrationBundle;
        use awaken_scoped_migration_sqlite::SqliteMigrationRunner;
        use rusqlite::Connection;

        // CE-TM10 decision rules:
        // R1 published V1..V21 + non-negative millis -> V22 preserves the value;
        // R2 published V1..V21 + legacy negative millis -> V22 maps it to i64::MAX;
        // R3 current V1..V23 ledger -> reopening applies nothing.
        let conn = Connection::open_in_memory().expect("open sqlite");
        let full = dispatch_bundle().expect("bundle builds");
        let published = MigrationBundle::new(BUNDLE_ID, full.migrations()[..21].to_vec())
            .expect("published bundle");
        let runner = SqliteMigrationRunner::with_prefix(NS).expect("runner");
        runner
            .run_bundle(&conn, &published)
            .expect("apply published prefix");

        conn.execute_batch(
            "INSERT INTO runtime_dispatch
                 (run_id, thread_id, request, status, lease_until, dead_lettered_at)
                 VALUES ('negative', 'thread', '{}', 'pending', -1, -2),
                        ('positive', 'thread', '{}', 'pending', 7, 8);
             INSERT INTO runtime_pending
                 (message_id, run_id, thread_id, correlation_id, result, available_at)
                 VALUES ('negative', 'negative', 'thread', 'c1', '{}', -3),
                        ('positive', 'positive', 'thread', 'c2', '{}', 9);
             INSERT INTO runtime_dispatch_operation (run_id, operation, recorded_at_ms)
                 VALUES ('negative', '{}', -4), ('positive', '{}', 10);",
        )
        .expect("seed legacy rows");

        let applied = runner.run_bundle(&conn, &full).expect("apply V22-V23");
        assert_eq!(
            applied
                .iter()
                .map(|migration| migration.version)
                .collect::<Vec<_>>(),
            [22, 23]
        );
        let maximum = i64::MAX;
        assert_eq!(
            conn.query_row(
                "SELECT lease_until FROM runtime_dispatch WHERE run_id = 'negative'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            maximum
        );
        assert_eq!(
            conn.query_row(
                "SELECT dead_lettered_at FROM runtime_dispatch WHERE run_id = 'negative'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            maximum
        );
        assert_eq!(
            conn.query_row(
                "SELECT available_at FROM runtime_pending WHERE message_id = 'negative'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            maximum
        );
        assert_eq!(
            conn.query_row(
                "SELECT recorded_at_ms FROM runtime_dispatch_operation WHERE run_id = 'negative'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            maximum
        );
        assert_eq!(
            conn.query_row(
                "SELECT lease_until + dead_lettered_at FROM runtime_dispatch WHERE run_id = 'positive'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            15
        );
        assert!(runner.run_bundle(&conn, &full).expect("reopen").is_empty());
    }

    #[test]
    fn v23_rejects_negative_authority_on_insert_and_update() {
        use awaken_scoped_migration_sqlite::SqliteMigrationRunner;
        use rusqlite::Connection;

        let conn = Connection::open_in_memory().expect("open sqlite");
        let full = dispatch_bundle().expect("bundle builds");
        let runner = SqliteMigrationRunner::with_prefix(NS).expect("runner");
        runner.run_bundle(&conn, &full).expect("apply full bundle");

        let negative_dispatch = conn.execute(
            "INSERT INTO runtime_dispatch
                 (run_id, thread_id, request, status, attempt_count)
             VALUES ('negative', 'thread', '{}', 'pending', -1)",
            [],
        );
        assert!(negative_dispatch.is_err(), "negative insert must fail");

        conn.execute(
            "INSERT INTO runtime_dispatch (run_id, thread_id, request, status)
             VALUES ('valid', 'thread', '{}', 'pending')",
            [],
        )
        .expect("seed valid dispatch");
        assert!(
            conn.execute(
                "UPDATE runtime_dispatch SET lease_epoch = -1 WHERE run_id = 'valid'",
                [],
            )
            .is_err(),
            "negative update must fail"
        );

        let negative_pending = conn.execute(
            "INSERT INTO runtime_pending
                 (message_id, run_id, thread_id, correlation_id, result, revision)
             VALUES ('negative', 'valid', 'thread', 'correlation', '{}', -1)",
            [],
        );
        assert!(negative_pending.is_err(), "negative insert must fail");

        conn.execute(
            "INSERT INTO runtime_pending
                 (message_id, run_id, thread_id, correlation_id, result)
             VALUES ('valid', 'valid', 'thread', 'correlation', '{}')",
            [],
        )
        .expect("seed valid pending input");
        assert!(
            conn.execute(
                "UPDATE runtime_pending SET revision = -1 WHERE message_id = 'valid'",
                [],
            )
            .is_err(),
            "negative update must fail"
        );
    }

    #[test]
    fn v23_fails_closed_and_rolls_back_for_corrupt_existing_rows() {
        use awaken_scoped_migration::MigrationBundle;
        use awaken_scoped_migration_sqlite::SqliteMigrationRunner;
        use rusqlite::Connection;

        let conn = Connection::open_in_memory().expect("open sqlite");
        let full = dispatch_bundle().expect("bundle builds");
        let published = MigrationBundle::new(BUNDLE_ID, full.migrations()[..22].to_vec())
            .expect("published bundle");
        let runner = SqliteMigrationRunner::with_prefix(NS).expect("runner");
        runner
            .run_bundle(&conn, &published)
            .expect("apply published prefix");
        conn.execute(
            "INSERT INTO runtime_dispatch
                 (run_id, thread_id, request, status, epoch)
             VALUES ('corrupt', 'thread', '{}', 'pending', -1)",
            [],
        )
        .expect("seed corrupt legacy row");

        assert!(
            runner.run_bundle(&conn, &full).is_err(),
            "migration must reject corrupt legacy authority"
        );

        conn.execute(
            "UPDATE runtime_dispatch SET epoch = 0 WHERE run_id = 'corrupt'",
            [],
        )
        .expect("repair legacy row after rolled-back migration");
        let applied = runner
            .run_bundle(&conn, &full)
            .expect("apply invariant after repair");
        assert_eq!(
            applied
                .iter()
                .map(|migration| migration.version)
                .collect::<Vec<_>>(),
            [23]
        );
    }

    #[test]
    fn index_migrations_render_for_both_dialects() {
        use awaken_scoped_migration::Dialect;
        let bundle = dispatch_bundle().expect("bundle builds");

        let indexes: Vec<_> = bundle
            .migrations()
            .iter()
            .filter(|m| m.description().starts_with("index:"))
            .collect();
        assert_eq!(indexes.len(), 7, "seven claim/lease indexes");

        for migration in indexes {
            for dialect in [Dialect::Postgres, Dialect::Sqlite] {
                let sql = awaken_scoped_migration::render(migration.sql_for(dialect), dialect, NS);
                assert!(sql.contains("CREATE INDEX"), "{sql}");
                // Every index name and target table carries the runtime prefix so
                // co-located runtimes never collide, and no token survives.
                assert!(sql.contains(&format!("{NS}_")), "prefixed: {sql}");
                assert!(!sql.contains('{'), "no leftover token: {sql}");
            }
        }
    }
}
