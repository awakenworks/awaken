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
        "V0015__delegation_group.sql",
        include_str!("migrations/V0015__delegation_group.sql"),
    ),
    (
        "V0016__drop_legacy_delegation_group.sql",
        include_str!("migrations/V0016__drop_legacy_delegation_group.sql"),
    ),
    (
        "V0017__dispatch_worker_assignment.sql",
        include_str!("migrations/V0017__dispatch_worker_assignment.sql"),
    ),
    (
        "V0018__dispatch_completion.sql",
        include_str!("migrations/V0018__dispatch_completion.sql"),
    ),
    (
        "V0019__stream_checkpoint.sql",
        include_str!("migrations/V0019__stream_checkpoint.sql"),
    ),
    (
        "V0020__durable_cancellation_intent.sql",
        include_str!("migrations/V0020__durable_cancellation_intent.sql"),
    ),
    (
        "V0021__dispatch_operational_feed.sql",
        include_str!("migrations/V0021__dispatch_operational_feed.sql"),
    ),
    (
        "V0022__dispatch_attempt_credentials.sql",
        include_str!("migrations/V0022__dispatch_attempt_credentials.sql"),
    ),
    (
        "V0023__dispatch_operation_recorded_at.sql",
        include_str!("migrations/V0023__dispatch_operation_recorded_at.sql"),
    ),
    (
        "V0024__normalize_signed_millis.sql",
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
            if version == 16 {
                Migration::published_legacy(
                    version,
                    description,
                    sql,
                    "b69507cd88429793df7d48d30bf84246ff61873687be7938cd737a602776db6b",
                )
            } else {
                Migration::new(version, description, sql)
            }
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
        // Cause/effect rule: deterministic V1..V24 bodies plus a valid scoped
        // receipt contract produce a lint-clean bundle; conditional DDL or a
        // malformed migration fails before either database runner executes it.
        let bundle = dispatch_bundle().expect("bundle builds");
        awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).expect("bundle lints");
    }

    #[test]
    fn versions_parse_from_file_names() {
        let bundle = dispatch_bundle().expect("bundle builds");
        // Cause/effect decision table: empty ledgers apply the immutable V1..V24
        // stream; any published prefix applies only the missing suffix; deleting
        // retired V15/V16 and shifting later versions breaks checksum proof.
        let versions: Vec<i64> = bundle.migrations().iter().map(|m| m.version()).collect();
        assert_eq!(versions, (1..=24).collect::<Vec<_>>());
    }

    #[test]
    fn compressed_dispatch_numbering_is_rejected_as_a_competing_history() {
        use std::collections::BTreeMap;

        use awaken_scoped_migration::{Dialect, plan};

        // R1 canonical V15 receipt -> verify; R2 compressed-track V15 receipt
        // (actually worker-assignment SQL) -> checksum mismatch at V15. The
        // latter cannot be merged because it maps a different effect to the same
        // version and would create a second migration source of truth.
        let applied = BTreeMap::from([(
            15,
            "e835f0eaae0017589eda70e1f1049fc1d2e428e7ade63f42defdc2366c737a90".into(),
        )]);

        let bundle = dispatch_bundle().expect("bundle builds");
        assert!(matches!(
            plan(&bundle, &applied, Dialect::Sqlite).unwrap_err(),
            awaken_scoped_migration::MigrationError::ChecksumMismatch { version: 15, .. }
        ));
    }

    #[test]
    fn v24_normalizes_legacy_negative_millis_during_forward_migration() {
        use awaken_scoped_migration::MigrationBundle;
        use awaken_scoped_migration_sqlite::SqliteMigrationRunner;
        use rusqlite::Connection;

        // CE-TM10 decision rules:
        // R1 published V1..V23 + non-negative millis -> V24 preserves the value;
        // R2 published V1..V23 + legacy negative millis -> V24 maps it to i64::MAX;
        // R3 current V1..V24 ledger -> reopening applies nothing (runner idempotency).
        let conn = Connection::open_in_memory().expect("open sqlite");
        let full = dispatch_bundle().expect("bundle builds");
        let published = MigrationBundle::new(BUNDLE_ID, full.migrations()[..23].to_vec())
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

        let applied = runner.run_bundle(&conn, &full).expect("apply V24");
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].version, 24);
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
