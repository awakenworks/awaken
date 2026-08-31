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

/// Once either published numbering stream reaches its terminal schema, every
/// future dispatch migration is appended here. The historical streams remain
/// immutable compatibility evidence; they are not parallel future writers.
#[cfg(feature = "durable")]
pub const CONVERGED_BUNDLE_ID: &str = "awaken.run_dispatch.converged";

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
const COMPLETION_FINGERPRINT: &str =
    include_str!("migrations/V0024__dispatch_completion_fingerprint.sql");
const COMPLETION_THREAD_AFFINITY: &str =
    include_str!("migrations/V0025__dispatch_completion_thread_affinity.sql");
const DISPATCH_RESERVATION_CLAIM: &str =
    include_str!("migrations/V0026__dispatch_reservation_claim.sql");
const PENDING_CONTEXT_MESSAGES: &str =
    include_str!("migrations/V0027__pending_context_messages.sql");
const PHYSICAL_ATTEMPT_SLOT_POSTGRES: &str =
    include_str!("migrations/V0002__physical_attempt_slot.postgres.sql");
const PHYSICAL_ATTEMPT_SLOT_SQLITE: &str =
    include_str!("migrations/V0002__physical_attempt_slot.sqlite.sql");

#[cfg(feature = "durable")]
const EXPANDED_V15_SQL: &str = include_str!("migrations/expanded/V0015__delegation_group.sql");
#[cfg(feature = "durable")]
const EXPANDED_V16_SQL: &str =
    include_str!("migrations/expanded/V0016__drop_legacy_delegation_group.sql");
#[cfg(feature = "durable")]
const EXPANDED_V15_CHECKSUM: &str =
    "7bd1db97f64a971e72ccc3505bedf1c8929577e6616eec092911e393cbb21e36";
#[cfg(feature = "durable")]
const EXPANDED_V16_CHECKSUM: &str =
    "b69507cd88429793df7d48d30bf84246ff61873687be7938cd737a602776db6b";
#[cfg(test)]
const COMPACT_V15_CHECKSUM: &str =
    "e835f0eaae0017589eda70e1f1049fc1d2e428e7ade63f42defdc2366c737a90";

#[cfg(feature = "durable")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublishedDispatchStream {
    Compact,
    Expanded,
}

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

fn portable_migration(
    version: i64,
    name: &str,
    contents: &str,
) -> Result<Migration, MigrationError> {
    Migration::new(version, description_of(name, contents), contents.trim())
}

/// Build the compact dispatch-schema history used by fresh databases and every
/// database that already recorded its V15 worker-assignment identity.
pub fn dispatch_bundle() -> Result<MigrationBundle, MigrationError> {
    let mut migrations = FILES
        .iter()
        .map(|(name, contents)| portable_migration(version_of(name), name, contents))
        .collect::<Result<Vec<_>, _>>()?;
    migrations.push(Migration::per_dialect(
        23,
        "enforce non-negative durable authority counters",
        NONNEGATIVE_AUTHORITY_POSTGRES.trim(),
        NONNEGATIVE_AUTHORITY_SQLITE.trim(),
    )?);
    migrations.push(Migration::new(
        24,
        "bind completion tombstones to canonical dispatch identity",
        COMPLETION_FINGERPRINT.trim(),
    )?);
    migrations.push(Migration::new(
        25,
        "retain completion Thread and parent Session affinity",
        COMPLETION_THREAD_AFFINITY.trim(),
    )?);
    migrations.push(Migration::new(
        26,
        "fence Session reservation recovery with ordinary Thread claims",
        DISPATCH_RESERVATION_CLAIM.trim(),
    )?);
    migrations.push(Migration::new(
        27,
        "retain stable context Messages on pending input",
        PENDING_CONTEXT_MESSAGES.trim(),
    )?);
    MigrationBundle::new(BUNDLE_ID, migrations)
}

/// Reconstruct the exact expanded V15..V24 stream that was already published
/// before compact numbering existed, then append each later schema effect once
/// at V25..V29. SQL bodies shared with the compact stream are referenced from
/// the same constants; only their immutable historical version identities vary.
#[cfg(feature = "durable")]
pub(crate) fn expanded_dispatch_bundle() -> Result<MigrationBundle, MigrationError> {
    let mut migrations = FILES
        .iter()
        .take(14)
        .map(|(name, contents)| portable_migration(version_of(name), name, contents))
        .collect::<Result<Vec<_>, _>>()?;
    migrations.push(Migration::published_legacy(
        15,
        description_of("V0015__delegation_group.sql", EXPANDED_V15_SQL),
        EXPANDED_V15_SQL.trim(),
        EXPANDED_V15_CHECKSUM,
    )?);
    migrations.push(Migration::published_legacy(
        16,
        description_of("V0016__drop_legacy_delegation_group.sql", EXPANDED_V16_SQL),
        EXPANDED_V16_SQL.trim(),
        EXPANDED_V16_CHECKSUM,
    )?);
    migrations.extend(
        FILES
            .iter()
            .skip(14)
            .map(|(name, contents)| portable_migration(version_of(name) + 2, name, contents))
            .collect::<Result<Vec<_>, _>>()?,
    );
    migrations.push(Migration::per_dialect(
        25,
        "enforce non-negative durable authority counters",
        NONNEGATIVE_AUTHORITY_POSTGRES.trim(),
        NONNEGATIVE_AUTHORITY_SQLITE.trim(),
    )?);
    migrations.push(Migration::new(
        26,
        "bind completion tombstones to canonical dispatch identity",
        COMPLETION_FINGERPRINT.trim(),
    )?);
    migrations.push(Migration::new(
        27,
        "retain completion Thread and parent Session affinity",
        COMPLETION_THREAD_AFFINITY.trim(),
    )?);
    migrations.push(Migration::new(
        28,
        "fence Session reservation recovery with ordinary Thread claims",
        DISPATCH_RESERVATION_CLAIM.trim(),
    )?);
    migrations.push(Migration::new(
        29,
        "retain stable context Messages on pending input",
        PENDING_CONTEXT_MESSAGES.trim(),
    )?);
    MigrationBundle::new(BUNDLE_ID, migrations)
}

#[cfg(feature = "durable")]
fn published_dispatch_stream(v15_checksum: Option<&str>) -> PublishedDispatchStream {
    if v15_checksum == Some(EXPANDED_V15_CHECKSUM) {
        PublishedDispatchStream::Expanded
    } else {
        PublishedDispatchStream::Compact
    }
}

/// Select one immutable published history from its V15 receipt. Prefixes below
/// V15 are common and safely continue on the compact stream; an unknown V15 is
/// deliberately sent to the compact bundle so the migration runner reports the
/// ordinary checksum mismatch without any special bypass.
#[cfg(feature = "durable")]
pub(crate) fn selected_dispatch_bundle(
    v15_checksum: Option<&str>,
) -> Result<MigrationBundle, MigrationError> {
    match published_dispatch_stream(v15_checksum) {
        PublishedDispatchStream::Compact => dispatch_bundle(),
        PublishedDispatchStream::Expanded => expanded_dispatch_bundle(),
    }
}

/// The sole append point after either immutable historical stream has reached
/// its terminal schema. V1 is a durable convergence receipt; new schema changes
/// start at V2 instead of extending both historical numberings.
#[cfg(feature = "durable")]
pub(crate) fn converged_dispatch_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        CONVERGED_BUNDLE_ID,
        vec![
            Migration::new(
                1,
                "seal the converged dispatch migration history",
                "SELECT 1",
            )?,
            Migration::per_dialect(
                2,
                "retain one exact physical executor until quiescence",
                PHYSICAL_ATTEMPT_SLOT_POSTGRES.trim(),
                PHYSICAL_ATTEMPT_SLOT_SQLITE.trim(),
            )?,
        ],
    )
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
        // Causes: C1 compact and expanded published bodies retain their exact
        // immutable identities; C2 shared SQL is referenced once; C3 only the
        // converged bundle may own future migrations. Effects: E1 every bundle
        // lints independently; E2 a changed legacy body/checksum, duplicate
        // version, or conditional new SQL fails before a runner executes it.
        // Decision rule D1=C1+C2+C3=>E1; D2=!C1|!C2|!C3=>E2.
        let compact = dispatch_bundle().expect("compact bundle builds");
        let expanded = expanded_dispatch_bundle().expect("expanded bundle builds");
        for bundle in [compact, expanded] {
            awaken_scoped_migration::lint(std::slice::from_ref(&bundle))
                .expect("dispatch bundle lints");
        }
        // The convergence ledger is deliberately a sequential continuation of
        // either historical ledger for this same bounded context, not an
        // independent component bundle. The generic linter cannot express an
        // inherited table owner, so this lint-only projection declares the one
        // existing Dispatch table and then validates the exact V2 bodies. Real
        // SQLite/Postgres migration tests below execute V2 after both histories.
        let converged = converged_dispatch_bundle().expect("converged bundle builds");
        let mut lineage = vec![
            Migration::new(
                1,
                "declare inherited dispatch table ownership for lint",
                "CREATE TABLE {prefix}_dispatch (run_id TEXT)",
            )
            .expect("lint owner marker"),
        ];
        lineage.extend(converged.migrations()[1..].iter().cloned());
        let lineage = MigrationBundle::new("awaken.run_dispatch.converged_lint", lineage)
            .expect("lint lineage builds");
        awaken_scoped_migration::lint(&[lineage]).expect("converged lineage lints");
    }

    #[test]
    fn published_numbering_selection_completes_each_history_once() {
        use std::collections::BTreeMap;

        use awaken_scoped_migration::{Dialect, MigrationError, plan};

        /* Published-history cause/effect table:
         * | Rule | durable V15 receipt | durable prefix | selected history | effect |
         * | H1 | absent/common prefix | V0..V14 | compact | append compact suffix |
         * | H2 | compact checksum | V15..V27 | compact | append only missing suffix |
         * | H3 | expanded checksum | V15..V29 | expanded | append only missing suffix |
         * | H4 | unknown checksum | V15 | compact fail-closed | no migration |
         * Constraints: selection reads only the immutable receipt; SQL bodies
         * shared by both histories have one source; after either terminal, the
         * converged bundle is the sole future append point.
         */
        let compact = dispatch_bundle().expect("compact");
        let expanded = expanded_dispatch_bundle().expect("expanded");
        assert_eq!(
            compact
                .migrations()
                .iter()
                .map(|migration| migration.version())
                .collect::<Vec<_>>(),
            (1..=27).collect::<Vec<_>>(),
            "H1/H2 compact dense history"
        );
        assert_eq!(
            expanded
                .migrations()
                .iter()
                .map(|migration| migration.version())
                .collect::<Vec<_>>(),
            (1..=29).collect::<Vec<_>>(),
            "H3 expanded dense history"
        );
        assert_eq!(
            published_dispatch_stream(None),
            PublishedDispatchStream::Compact,
            "H1"
        );
        assert_eq!(
            published_dispatch_stream(Some(COMPACT_V15_CHECKSUM)),
            PublishedDispatchStream::Compact,
            "H2"
        );
        assert_eq!(
            published_dispatch_stream(Some(EXPANDED_V15_CHECKSUM)),
            PublishedDispatchStream::Expanded,
            "H3"
        );

        for (bundle, terminal) in [(&compact, 27usize), (&expanded, 29usize)] {
            for prefix in 0..=terminal {
                let applied = bundle
                    .migrations()
                    .iter()
                    .take(prefix)
                    .map(|migration| (migration.version(), migration.checksum_for(Dialect::Sqlite)))
                    .collect::<BTreeMap<_, _>>();
                assert_eq!(
                    plan(bundle, &applied, Dialect::Sqlite)
                        .expect("published prefix completes")
                        .len(),
                    terminal - prefix,
                    "H1-H3 prefix {prefix}"
                );
            }
        }

        let mut unknown = compact
            .migrations()
            .iter()
            .take(15)
            .map(|migration| (migration.version(), migration.checksum_for(Dialect::Sqlite)))
            .collect::<BTreeMap<_, _>>();
        unknown.insert(15, "f".repeat(64));
        assert!(
            matches!(
                plan(
                    &selected_dispatch_bundle(Some(&"f".repeat(64))).expect("H4 bundle"),
                    &unknown,
                    Dialect::Sqlite
                ),
                Err(MigrationError::ChecksumMismatch { version: 15, .. })
            ),
            "H4"
        );
    }

    #[test]
    fn expanded_sqlite_history_reaches_the_converged_schema_and_reopens() {
        use awaken_scoped_migration::MigrationBundle;
        use awaken_scoped_migration_sqlite::SqliteMigrationRunner;
        use rusqlite::Connection;

        /* Real-storage cause/effect table:
         * E1 exact expanded V1..V24 ledger -> append V25..V29 and convergence;
         * E2 completed expanded ledger -> reopen with zero writes;
         * E3 missing later columns/triggers before cutover -> one final schema
         * containing completion identity/affinity, reservation claim, pending
         * context, and non-negative authority enforcement.
         * Rules S1=expanded-prefix=>E1+E3; S2=terminal=>E2.
         */
        let connection = Connection::open_in_memory().expect("sqlite");
        let expanded = expanded_dispatch_bundle().expect("expanded");
        let published = MigrationBundle::new(BUNDLE_ID, expanded.migrations()[..24].to_vec())
            .expect("published expanded prefix");
        let runner = SqliteMigrationRunner::with_prefix(NS).expect("runner");
        runner
            .run_bundle(&connection, &published)
            .expect("seed expanded V1..V24");
        let selected = selected_dispatch_bundle(Some(EXPANDED_V15_CHECKSUM)).expect("S1 select");
        assert_eq!(
            runner
                .run_bundle(&connection, &selected)
                .expect("S1 finish")
                .iter()
                .map(|migration| migration.version)
                .collect::<Vec<_>>(),
            [25, 26, 27, 28, 29],
            "S1/E1"
        );
        assert_eq!(
            runner
                .run_bundle(
                    &connection,
                    &converged_dispatch_bundle().expect("converged"),
                )
                .expect("S1 convergence")
                .iter()
                .map(|migration| migration.version)
                .collect::<Vec<_>>(),
            [1, 2],
            "S1/E1"
        );
        for (table, column) in [
            ("runtime_dispatch_completion", "request_fingerprint"),
            ("runtime_dispatch_completion", "thread_id"),
            ("runtime_pending", "context_messages"),
            ("runtime_dispatch", "active_attempt_epoch"),
        ] {
            let present = connection
                .prepare(&format!("PRAGMA table_info({table})"))
                .and_then(|mut statement| {
                    statement
                        .query_map([], |row| row.get::<_, String>(1))?
                        .collect::<Result<Vec<_>, _>>()
                })
                .expect("S1 inspect schema")
                .iter()
                .any(|observed| observed == column);
            assert!(present, "S1/E3 {table}.{column}");
        }
        let running_index: String = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'runtime_dispatch_one_running_idx'",
                [],
                |row| row.get(0),
            )
            .expect("S1 inspect reservation claim fence");
        assert!(
            running_index.contains("reservation_running"),
            "S1/E3 reservation claim fence"
        );
        assert!(
            connection
                .execute(
                    "INSERT INTO runtime_dispatch (run_id, thread_id, request, status, attempt_count) VALUES ('negative', 'thread', '{}', 'pending', -1)",
                    [],
                )
                .is_err(),
            "S1/E3 non-negative authority"
        );
        assert!(
            runner
                .run_bundle(&connection, &selected)
                .expect("S2 published")
                .is_empty()
                && runner
                    .run_bundle(
                        &connection,
                        &converged_dispatch_bundle().expect("converged"),
                    )
                    .expect("S2 converged")
                    .is_empty(),
            "S2/E2"
        );
    }

    #[test]
    fn versions_parse_from_file_names() {
        // Causes: C1 the bundle contains the published migration filenames;
        // C2 a filename could be missing, duplicated, or out of order. Effects:
        // E1 C1 yields the exact dense V1..V27 ledger; E2 C2 is rejected by the
        // bundle constructor/lint. Constraint/Invariant: filename identity is
        // the migration version authority. Decision rule: this test covers the
        // valid dense-ledger rule; bundle lint covers each invalid construction.
        let bundle = dispatch_bundle().expect("bundle builds");
        // Decision table: D1 empty ledger -> exact dense V1..V27; D2 exact
        // prefix -> only its suffix; D3 duplicate/gap -> bundle rejection.
        let versions: Vec<i64> = bundle.migrations().iter().map(|m| m.version()).collect();
        assert_eq!(versions, (1..=27).collect::<Vec<_>>());
    }

    #[test]
    fn v22_normalizes_legacy_negative_millis_during_forward_migration() {
        use awaken_scoped_migration::MigrationBundle;
        use awaken_scoped_migration_sqlite::SqliteMigrationRunner;
        use rusqlite::Connection;

        // CE-TM10 decision rules:
        // R1 published V1..V21 + non-negative millis -> V22 preserves the value;
        // R2 published V1..V21 + legacy negative millis -> V22 maps it to i64::MAX;
        // R3 current V1..V27 ledger -> reopening applies nothing.
        // Causes: C1 a pre-V22 row stores non-negative or legacy negative
        // milliseconds; C2 the current ledger is reopened. Effects: E1 preserve
        // valid time, E2 normalize invalid legacy time, E3 apply no migration on
        // reopen. Constraint/Invariant: a forward migration may repair legacy
        // sentinel values but never change already-valid time. Decision rule:
        // execute R1-R3 to cover both value classes and idempotent reopening.
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

        let applied = runner.run_bundle(&conn, &full).expect("apply V22-V27");
        assert_eq!(
            applied
                .iter()
                .map(|migration| migration.version)
                .collect::<Vec<_>>(),
            [22, 23, 24, 25, 26, 27]
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

        // Test design. Causes: C1 a pre-V23 authority row has a negative epoch;
        // C2 the V23 constraint migration runs. Effects: E1 the migration fails;
        // E2 its schema/data changes roll back atomically. Constraint/Invariant:
        // corrupt fencing authority must never be coerced into a valid epoch.
        // Decision rule: seed the invalid legacy partition, require failure, then
        // prove both the old ledger and row remain intact.
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
            [23, 24, 25, 26, 27]
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

    #[test]
    fn reservation_repair_shares_the_existing_thread_claim_fence() {
        // Cause/effect decision table: C1=V26 rendered for Postgres/SQLite;
        // E1=the one existing per-Thread unique index fences both ordinary
        // `running` and repair `reservation_running`; E2=no second table or
        // parallel ownership index is introduced. R1=C1(Postgres)=>E1+E2;
        // R2=C1(SQLite)=>E1+E2.
        // Constraint/Invariant: ordinary execution and reservation repair share
        // one per-Thread ownership fence. Decision rule: render R1 and R2 and
        // require the shared predicate while forbidding another table.
        use awaken_scoped_migration::Dialect;
        let bundle = dispatch_bundle().expect("bundle builds");
        let migration = bundle
            .migrations()
            .iter()
            .find(|migration| migration.version() == 26)
            .expect("V26 exists");
        for dialect in [Dialect::Postgres, Dialect::Sqlite] {
            let sql = awaken_scoped_migration::render(migration.sql_for(dialect), dialect, NS);
            assert!(sql.contains("CREATE UNIQUE INDEX"), "R1-R2/E1: {sql}");
            assert!(
                sql.contains("status IN ('running', 'reservation_running')"),
                "R1-R2/E1: {sql}"
            );
            assert!(!sql.contains("CREATE TABLE"), "R1-R2/E2: {sql}");
        }
    }
}
