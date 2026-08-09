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
            Migration::new(
                version_of(name),
                description_of(name, contents),
                contents.trim(),
            )
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
        // Cause/effect decision table: V0015 applied + V0016 receipt absent =>
        // deterministic legacy-table removal; V0016 receipt present => skip;
        // missing V0015 table => fail closed and do not record a false receipt.
        let bundle = dispatch_bundle().expect("bundle builds");
        awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).expect("bundle lints");
    }

    #[test]
    fn versions_parse_from_file_names() {
        let bundle = dispatch_bundle().expect("bundle builds");
        // Cause/effect decision table: empty ledgers apply the immutable V1..V22
        // stream; any published prefix applies only the missing suffix; renumbering
        // a published migration breaks checksum/version proof.
        let versions: Vec<i64> = bundle.migrations().iter().map(|m| m.version()).collect();
        assert_eq!(versions, (1..=22).collect::<Vec<_>>());
    }

    #[test]
    fn deployed_sqlite_v21_ledger_upgrades_without_historical_checksum_drift() {
        use std::collections::BTreeMap;

        use awaken_scoped_migration::{Dialect, plan};

        let applied = BTreeMap::from([
            (
                1,
                "4402194702a7ceceb311387a13e990f3f220115317c3c1fd01dad85f14e37e42".into(),
            ),
            (
                2,
                "1e4904c1f32b8ee83462f463d059ce753a5eaab0dbe68aee3b5d3b3e6a80afb4".into(),
            ),
            (
                3,
                "f405cf7bf61d1b773034df2f4cdb48e578df292866c3e6e20bd462208fa7c432".into(),
            ),
            (
                4,
                "805fb3fc6b1880622181c91610e69e6a48a767322581890a5783ab2027d75f42".into(),
            ),
            (
                5,
                "4ee458f848725d5954c4976d767b7770e7f8e3a78626039339b304a446a64ab5".into(),
            ),
            (
                6,
                "00fbe3e570ced9d02402fc2672ec041532f6d201e707e6926a34025f3477298e".into(),
            ),
            (
                7,
                "e84574c7f0ac8e61c06fae90d6cf28f8d1e2b18452c921e18ff16f2825bbd53e".into(),
            ),
            (
                8,
                "95fa074118ff817a4e2013c786702996834c11afb3277e2f352ad4c1d2b596b7".into(),
            ),
            (
                9,
                "3572e4fc3db8cb7547bc929f14076f89064b00a98bc42b72e209814ddbca9e59".into(),
            ),
            (
                10,
                "2e827cd2bf01f393003e842491ee234238b7c62dee604b509fb6737e19e17c77".into(),
            ),
            (
                11,
                "eb01f0925369cf5d4c6dc245b4501aab52c184e8b865019627921549f8d88476".into(),
            ),
            (
                12,
                "4fe317147f29a0e1038db50ce1deaec5ac84a98ade13313b03f1eaf7269b0eb8".into(),
            ),
            (
                13,
                "0967126353167c6b3bef5221de02ad1d2eb21a1447748f2188f7041c7c09f517".into(),
            ),
            (
                14,
                "3c02fbb35c0dc5c54d8dbc590c62f03a72297695e4088c73b04d3be9b88f8598".into(),
            ),
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
        ]);

        let bundle = dispatch_bundle().expect("bundle builds");
        let pending = plan(&bundle, &applied, Dialect::Sqlite).expect("published ledger verifies");
        assert_eq!(
            pending
                .into_iter()
                .map(awaken_scoped_migration::Migration::version)
                .collect::<Vec<_>>(),
            vec![22]
        );
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
