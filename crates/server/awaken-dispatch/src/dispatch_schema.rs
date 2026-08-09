//! The durable dispatch schema, shared by the Postgres and SQLite backends.
//!
//! One portable [`MigrationBundle`] using the migrator's dialect-neutral tokens,
//! so the *same* bundle drives both runners. The DDL itself is NOT encoded in
//! this source: every migration is a `.sql` file under `migrations/`, embedded at
//! build time with `include_str!` and turned into a [`Migration`] here. The file
//! name carries the version (`V0004__…` ⇒ version 4) and the first `-- comment`
//! line is its description, so adding or changing schema means adding or editing a
//! migration *file*, never a Rust string literal.
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
    let migrations = FILES
        .iter()
        .map(|(name, contents)| {
            let version = version_of(name);
            let description = description_of(name, contents);
            let sql = contents.trim();
            Migration::new(version, description, sql)
        })
        .collect::<Result<Vec<_>, _>>()?;
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
        // Cause/effect rule: deterministic V1..V22 bodies plus a valid scoped
        // receipt contract produce a lint-clean bundle; conditional DDL or a
        // malformed migration fails before either database runner executes it.
        let bundle = dispatch_bundle().expect("bundle builds");
        awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).expect("bundle lints");
    }

    #[test]
    fn versions_parse_from_file_names() {
        let bundle = dispatch_bundle().expect("bundle builds");
        // Cause/effect decision table: empty ledgers apply the immutable V1..V22
        // stream and any published prefix applies only the missing suffix.
        let versions: Vec<i64> = bundle.migrations().iter().map(|m| m.version()).collect();
        assert_eq!(versions, (1..=22).collect::<Vec<_>>());
    }

    #[test]
    fn deployed_dispatch_receipts_remain_the_canonical_history() {
        use std::collections::BTreeMap;

        use awaken_scoped_migration::{Dialect, plan};

        // These exact receipts were written by released V15..V22. Inserting two
        // transient migrations before them and renumbering their effects is not
        // a forward migration: it makes every deployed database fail startup.
        let applied = BTreeMap::from([
            (
                15,
                "e835f0eaae0017589eda70e1f1049fc1d2e428e7ade63f42defdc2366c737a90".into(),
            ),
            (
                16,
                "545591fc3352db71247bf94a31dc195a06b32ca4e1d3b52052601d463afd3a2c".into(),
            ),
            (
                17,
                "557e6d23c5f135df6932dd96d61f1fa997e84a686f9b48ceb264fa0e65d9a716".into(),
            ),
            (
                18,
                "d1ab5678a30aa454ca8ba01de23511dc9e9d99f7c6f654b3d64841e4eeb2d72b".into(),
            ),
            (
                19,
                "378d530aacb8236ef6bc385615720b0ac74f66b6579e498b939d816cd48d6e4d".into(),
            ),
            (
                20,
                "19ea60ea79d9c80ec872cccbb9bb810458cf3b446d1b5581d208d1da7868a36a".into(),
            ),
            (
                21,
                "75178da4f1b5e3c8ec83550ae11cc13efd26cb66cd16ea20459ec19d8586399b".into(),
            ),
            (
                22,
                "e609358db6e3dc1fd1e1548ecf6a9b3072510e33a0c28234d910f028b7720246".into(),
            ),
        ]);

        let bundle = dispatch_bundle().expect("bundle builds");
        let pending = plan(&bundle, &applied, Dialect::Sqlite).expect("deployed receipts verify");
        assert!(pending.iter().all(|migration| migration.version() < 15));
    }

    #[test]
    fn v22_normalizes_legacy_negative_millis_during_forward_migration() {
        use awaken_scoped_migration::MigrationBundle;
        use awaken_scoped_migration_sqlite::SqliteMigrationRunner;
        use rusqlite::Connection;

        // CE-TM10 decision rules:
        // R1 published V1..V21 + non-negative millis -> V22 preserves the value;
        // R2 published V1..V21 + legacy negative millis -> V22 maps it to i64::MAX;
        // R3 current V1..V22 ledger -> reopening applies nothing (runner idempotency).
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

        let applied = runner.run_bundle(&conn, &full).expect("apply V22");
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].version, 22);
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
