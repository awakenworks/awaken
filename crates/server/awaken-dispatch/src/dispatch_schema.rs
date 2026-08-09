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
        include_str!("migrations/V0017__dispatch_worker_assignment.sql"),
    ),
    (
        "V0016__dispatch_completion.sql",
        include_str!("migrations/V0018__dispatch_completion.sql"),
    ),
    (
        "V0017__stream_checkpoint.sql",
        include_str!("migrations/V0019__stream_checkpoint.sql"),
    ),
    (
        "V0018__durable_cancellation_intent.sql",
        include_str!("migrations/V0020__durable_cancellation_intent.sql"),
    ),
    (
        "V0019__dispatch_operational_feed.sql",
        include_str!("migrations/V0021__dispatch_operational_feed.sql"),
    ),
    (
        "V0020__dispatch_attempt_credentials.sql",
        include_str!("migrations/V0022__dispatch_attempt_credentials.sql"),
    ),
    (
        "V0021__dispatch_operation_recorded_at.sql",
        include_str!("migrations/V0023__dispatch_operation_recorded_at.sql"),
    ),
    (
        "V0022__normalize_signed_millis.sql",
        include_str!("migrations/V0024__normalize_signed_millis.sql"),
    ),
];

/// Exact receipt aliases from the briefly published expanded V15..V24 stream.
/// Both streams end in the same schema: expanded V15 creates and V16 drops the
/// retired delegation table, then carries the canonical V15..V22 effects at
/// V17..V24. Aliases verify those already-written receipts without executing a
/// second SQL body; V23/V24 below close the canonical stream with no-op receipts.
const EXPANDED_NUMBERING_ALIASES: &[(i64, &str, &str)] = &[
    (
        15,
        "e835f0eaae0017589eda70e1f1049fc1d2e428e7ade63f42defdc2366c737a90",
        "7bd1db97f64a971e72ccc3505bedf1c8929577e6616eec092911e393cbb21e36",
    ),
    (
        16,
        "545591fc3352db71247bf94a31dc195a06b32ca4e1d3b52052601d463afd3a2c",
        "b69507cd88429793df7d48d30bf84246ff61873687be7938cd737a602776db6b",
    ),
    (
        17,
        "557e6d23c5f135df6932dd96d61f1fa997e84a686f9b48ceb264fa0e65d9a716",
        "29ca0a829096c3d059e9dfa8cfe819f19d1c9d049b3c73de55d62cd5a4884ea2",
    ),
    (
        18,
        "d1ab5678a30aa454ca8ba01de23511dc9e9d99f7c6f654b3d64841e4eeb2d72b",
        "d435f47b492c776b2daf777686f839abe556d1fbf29a072bbc4a79540006fe5a",
    ),
    (
        19,
        "378d530aacb8236ef6bc385615720b0ac74f66b6579e498b939d816cd48d6e4d",
        "e4c40d80876ed3748886d9a7c97f606bf73c5d8970a11825af81c78466b6ef47",
    ),
    (
        20,
        "19ea60ea79d9c80ec872cccbb9bb810458cf3b446d1b5581d208d1da7868a36a",
        "7b0f9f1fb51a4d3b48bd51f459dd40676ed1df82c669c606aa400a2bdd6692c1",
    ),
    (
        21,
        "75178da4f1b5e3c8ec83550ae11cc13efd26cb66cd16ea20459ec19d8586399b",
        "b22b633dadd0e98c8586446efa0173dc01352092eab7f2b0bcf88a9829e27f41",
    ),
    (
        22,
        "e609358db6e3dc1fd1e1548ecf6a9b3072510e33a0c28234d910f028b7720246",
        "50d0d9f9ab0fcfe9061949c716f3aceabd6047471f826f5edc4b2fa81c9e650f",
    ),
];

const EXPANDED_V23_CHECKSUM: &str =
    "1b51301f97d66b7ec7f206fffd37aa76ef8e1fabdeafca73bca76f1d32c949ae";
const EXPANDED_V24_CHECKSUM: &str =
    "a50d68aa5d72ff56b02970db76982f817ff6fba7523f0028dbaa6ed33ae488f3";

const NONNEGATIVE_AUTHORITY_POSTGRES: &str =
    include_str!("migrations/V0025__nonnegative_authority.postgres.sql");
const NONNEGATIVE_AUTHORITY_SQLITE: &str =
    include_str!("migrations/V0025__nonnegative_authority.sqlite.sql");

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
            let version = version_of(name);
            let description = description_of(name, contents);
            let sql = contents.trim();
            if let Some((_, expected, alias)) = EXPANDED_NUMBERING_ALIASES
                .iter()
                .find(|(published_version, _, _)| *published_version == version)
            {
                Migration::published_legacy_with_aliases(
                    version,
                    description,
                    sql,
                    *expected,
                    [*alias],
                )
            } else {
                Migration::new(version, description, sql)
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    migrations.push(Migration::published_legacy_with_aliases(
        23,
        "converge the published expanded dispatch numbering track",
        "SELECT 1",
        "97ed3e2d59266fadc3c1c326e933a93de56258d0543aabd0b1a184b545fb892b",
        [EXPANDED_V23_CHECKSUM],
    )?);
    migrations.push(Migration::published_legacy_with_aliases(
        24,
        "seal the converged dispatch migration history",
        "SELECT 1",
        "20f5655bfa2b46c96897d26777b8c0642d790f6e7b4eee64f728fdeda2ae3175",
        [EXPANDED_V24_CHECKSUM],
    )?);
    migrations.push(Migration::per_dialect(
        25,
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
        // Cause/effect rule: deterministic V1..V22 bodies, two convergence
        // receipts, one explicit per-dialect invariant, and a valid scoped
        // receipt contract produce a lint-clean bundle; malformed migration
        // metadata fails before either database runner executes it.
        let bundle = dispatch_bundle().expect("bundle builds");
        awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).expect("bundle lints");
    }

    #[test]
    fn versions_parse_from_file_names() {
        let bundle = dispatch_bundle().expect("bundle builds");
        // Cause/effect decision table: empty ledgers apply one immutable V1..V25
        // stream and any published prefix applies only the missing suffix.
        let versions: Vec<i64> = bundle.migrations().iter().map(|m| m.version()).collect();
        assert_eq!(versions, (1..=25).collect::<Vec<_>>());
    }

    #[test]
    fn published_dispatch_numbering_tracks_converge_once() {
        use std::collections::BTreeMap;

        use awaken_scoped_migration::{Dialect, plan};

        /* Published-history cause/effect decision table. Causes: C1 an empty
         * ledger; C2 the canonical compressed V1..V22 ledger; C3 the expanded
         * V1..V24 ledger whose V15/V16 net effect is empty; C4 an unrecognized
         * receipt. Effects: E1 execute only canonical SQL; E2 append V23/V24
         * convergence receipts and V25; E3 append only V25 to the expanded
         * terminal ledger; E4 fail closed. Rules: H1 C1=>E1; H2 C2=>E2; H3 C3=>E3;
         * H4 C4=>E4. Constraint: both published tracks must already have the
         * same terminal schema; aliases verify receipts and never select SQL. */
        let bundle = dispatch_bundle().expect("bundle builds");
        let canonical = bundle
            .migrations()
            .iter()
            .take(22)
            .map(|migration| (migration.version(), migration.checksum_for(Dialect::Sqlite)))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            plan(&bundle, &canonical, Dialect::Sqlite)
                .expect("H2 canonical ledger")
                .iter()
                .map(|migration| migration.version())
                .collect::<Vec<_>>(),
            [23, 24, 25]
        );

        let mut expanded = canonical
            .iter()
            .filter(|(version, _)| **version < 15)
            .map(|(version, checksum)| (*version, checksum.clone()))
            .collect::<BTreeMap<_, _>>();
        expanded.extend(
            EXPANDED_NUMBERING_ALIASES
                .iter()
                .map(|(version, _, alias)| (*version, (*alias).to_string())),
        );
        expanded.insert(23, EXPANDED_V23_CHECKSUM.into());
        expanded.insert(24, EXPANDED_V24_CHECKSUM.into());
        assert_eq!(
            plan(&bundle, &expanded, Dialect::Sqlite)
                .expect("H3 expanded ledger")
                .iter()
                .map(|migration| migration.version())
                .collect::<Vec<_>>(),
            [25]
        );

        expanded.insert(23, "unknown-receipt".into());
        assert!(matches!(
            plan(&bundle, &expanded, Dialect::Sqlite).unwrap_err(),
            awaken_scoped_migration::MigrationError::ChecksumMismatch { version: 23, .. }
        ));
    }

    #[test]
    fn v22_normalizes_legacy_negative_millis_during_forward_migration() {
        use awaken_scoped_migration::MigrationBundle;
        use awaken_scoped_migration_sqlite::SqliteMigrationRunner;
        use rusqlite::Connection;

        // CE-TM10 decision rules:
        // R1 published V1..V21 + non-negative millis -> V22 preserves the value;
        // R2 published V1..V21 + legacy negative millis -> V22 maps it to i64::MAX;
        // R3 current V1..V22 ledger -> reopening appends only the convergence
        // receipts; the normalization itself is never replayed.
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

        let applied = runner.run_bundle(&conn, &full).expect("apply V22-V25");
        assert_eq!(
            applied
                .iter()
                .map(|migration| migration.version)
                .collect::<Vec<_>>(),
            [22, 23, 24, 25]
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
    fn v25_rejects_negative_authority_on_insert_and_update() {
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
    fn v25_fails_closed_and_rolls_back_for_corrupt_existing_rows() {
        use awaken_scoped_migration::MigrationBundle;
        use awaken_scoped_migration_sqlite::SqliteMigrationRunner;
        use rusqlite::Connection;

        let conn = Connection::open_in_memory().expect("open sqlite");
        let full = dispatch_bundle().expect("bundle builds");
        let published = MigrationBundle::new(BUNDLE_ID, full.migrations()[..24].to_vec())
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
            [25]
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
