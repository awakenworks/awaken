//! The durable commit schema, shared by every store backend.
//!
//! The commit tables are a faithful projection of the staged `ThreadCommit`
//! (ADR-0008): the append-only commit/run-fact log (stored in the legacy `phase`
//! column, but representing state authority and the
//! fence, G31/G32), the message transcript, the state-command log, committed
//! events, the run-record cache (a projection of the latest fact, G32), and
//! active awaiting tickets. The schema is one portable [`MigrationBundle`] using
//! the migrator's dialect-neutral tokens, so the *same* bundle drives both the
//! Postgres (`sqlx`) and SQLite (`rusqlite`) runners. This crate names no SQL
//! driver — it owns the schema, the backends own the runner.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};
pub use awaken_store_runtime::{StoredU64, StoredU64Error};

/// The bundle id for the runtime commit schema. Scoped so it never collides with
/// another component's migrations in the same database.
pub const COMMIT_BUNDLE_ID: &str = "awaken.runtime_commit";

/// `(version, description, portable SQL)` for each commit table — one row per
/// field of the committed thread.
const COMMIT_SPECS: [(i64, &str, &str); 8] = [
    (
        1,
        "commit log: run-fact state authority and the monotonic fence",
        "CREATE TABLE {prefix}_commit (\
            sequence BIGINT PRIMARY KEY, \
            thread_id TEXT NOT NULL, \
            run_id TEXT NOT NULL, \
            phase {json} NOT NULL, \
            committed_at {timestamptz} NOT NULL DEFAULT {now})",
    ),
    (
        2,
        "thread transcript messages, in commit order",
        "CREATE TABLE {prefix}_message (\
            id {pk_autoinc}, \
            commit_sequence BIGINT NOT NULL, \
            thread_id TEXT NOT NULL, \
            data {json} NOT NULL)",
    ),
    (
        3,
        "state command log, replayed in order",
        "CREATE TABLE {prefix}_state_command (\
            id {pk_autoinc}, \
            commit_sequence BIGINT NOT NULL, \
            thread_id TEXT NOT NULL, \
            data {json} NOT NULL)",
    ),
    (
        4,
        "committed lifecycle/observability event stream (monotonic sequence); \
         NOT the message-truth source — messages live in {prefix}_message, state \
         in {prefix}_state_command, state authority/fence in {prefix}_commit",
        "CREATE TABLE {prefix}_event (\
            sequence BIGINT PRIMARY KEY, \
            run_id TEXT NOT NULL, \
            kind {json} NOT NULL, \
            payload {json} NOT NULL)",
    ),
    (
        5,
        "run record cache: a projection of the latest run fact",
        "CREATE TABLE {prefix}_run_record (\
            run_id TEXT PRIMARY KEY, \
            thread_id TEXT NOT NULL, \
            phase {json} NOT NULL, \
            updated_at {timestamptz} NOT NULL DEFAULT {now})",
    ),
    (
        6,
        "active awaiting tickets, present only while a run is awaiting",
        "CREATE TABLE {prefix}_waiting (\
            run_id TEXT PRIMARY KEY, \
            ticket {json} NOT NULL)",
    ),
    (
        7,
        "per-thread optimistic commit version",
        "CREATE TABLE {prefix}_thread_version (\
            thread_id TEXT PRIMARY KEY, \
            version BIGINT NOT NULL)",
    ),
    (
        8,
        "durable idempotency receipts for logical commit operations",
        "CREATE TABLE {prefix}_commit_receipt (\
            operation_run_id TEXT NOT NULL, \
            operation_ordinal BIGINT NOT NULL, \
            thread_id TEXT NOT NULL, \
            payload_hash TEXT NOT NULL, \
            commit_sequence BIGINT NOT NULL, \
            thread_version BIGINT NOT NULL, \
            PRIMARY KEY (operation_run_id, operation_ordinal))",
    ),
];

/// Portable migration-plan invariant shared by every bundle audit: versions are
/// the dense positive prefix `1..=n`. A dense stream is intentionally stronger
/// than merely increasing, preventing a skipped migration from being silently
/// treated as already applied.
#[must_use]
pub fn versions_are_dense_from_one(versions: &[i64]) -> bool {
    versions
        .iter()
        .enumerate()
        .all(|(index, version)| *version == index as i64 + 1)
}

/// One migration-runner decision over abstract versions. It advances by at most
/// one available version, never rolls back, and becomes an idempotent no-op at the
/// tip. Concrete runners repeat this kernel until `applied == available`.
#[must_use]
pub const fn migration_step(applied: u32, available: u32) -> u32 {
    if applied < available {
        applied + 1
    } else {
        applied
    }
}

/// Build the commit-schema migration bundle. The version stream is independent
/// and strictly increasing; later schema changes append new specs.
pub fn commit_bundle() -> Result<MigrationBundle, MigrationError> {
    let versions = COMMIT_SPECS.map(|(version, _, _)| version);
    assert!(
        versions_are_dense_from_one(&versions),
        "runtime commit migrations must be a dense prefix"
    );
    let mut migrations = COMMIT_SPECS
        .iter()
        .map(|(version, description, sql)| Migration::new(*version, *description, *sql))
        .collect::<Result<Vec<_>, _>>()?;
    migrations.push(Migration::per_dialect(
        9,
        "enforce non-negative runtime commit authority",
        "ALTER TABLE {prefix}_commit ADD CONSTRAINT {prefix}_commit_sequence_nonnegative CHECK (sequence >= 0);\
         ALTER TABLE {prefix}_message ADD CONSTRAINT {prefix}_message_commit_nonnegative CHECK (commit_sequence >= 0);\
         ALTER TABLE {prefix}_state_command ADD CONSTRAINT {prefix}_state_commit_nonnegative CHECK (commit_sequence >= 0);\
         ALTER TABLE {prefix}_event ADD CONSTRAINT {prefix}_event_sequence_nonnegative CHECK (sequence >= 0);\
         ALTER TABLE {prefix}_thread_version ADD CONSTRAINT {prefix}_thread_version_nonnegative CHECK (version >= 0);\
         ALTER TABLE {prefix}_commit_receipt ADD CONSTRAINT {prefix}_receipt_authority_nonnegative CHECK (operation_ordinal >= 0 AND commit_sequence >= 0 AND thread_version >= 0)",
        "CREATE TRIGGER {prefix}_commit_nonnegative_insert BEFORE INSERT ON {prefix}_commit WHEN NEW.sequence < 0 BEGIN SELECT RAISE(ABORT, 'negative commit sequence'); END;\
         CREATE TRIGGER {prefix}_commit_nonnegative_update BEFORE UPDATE OF sequence ON {prefix}_commit WHEN NEW.sequence < 0 BEGIN SELECT RAISE(ABORT, 'negative commit sequence'); END;\
         CREATE TRIGGER {prefix}_message_nonnegative_insert BEFORE INSERT ON {prefix}_message WHEN NEW.commit_sequence < 0 BEGIN SELECT RAISE(ABORT, 'negative message commit sequence'); END;\
         CREATE TRIGGER {prefix}_message_nonnegative_update BEFORE UPDATE OF commit_sequence ON {prefix}_message WHEN NEW.commit_sequence < 0 BEGIN SELECT RAISE(ABORT, 'negative message commit sequence'); END;\
         CREATE TRIGGER {prefix}_state_nonnegative_insert BEFORE INSERT ON {prefix}_state_command WHEN NEW.commit_sequence < 0 BEGIN SELECT RAISE(ABORT, 'negative state commit sequence'); END;\
         CREATE TRIGGER {prefix}_state_nonnegative_update BEFORE UPDATE OF commit_sequence ON {prefix}_state_command WHEN NEW.commit_sequence < 0 BEGIN SELECT RAISE(ABORT, 'negative state commit sequence'); END;\
         CREATE TRIGGER {prefix}_event_nonnegative_insert BEFORE INSERT ON {prefix}_event WHEN NEW.sequence < 0 BEGIN SELECT RAISE(ABORT, 'negative event sequence'); END;\
         CREATE TRIGGER {prefix}_event_nonnegative_update BEFORE UPDATE OF sequence ON {prefix}_event WHEN NEW.sequence < 0 BEGIN SELECT RAISE(ABORT, 'negative event sequence'); END;\
         CREATE TRIGGER {prefix}_thread_version_nonnegative_insert BEFORE INSERT ON {prefix}_thread_version WHEN NEW.version < 0 BEGIN SELECT RAISE(ABORT, 'negative thread version'); END;\
         CREATE TRIGGER {prefix}_thread_version_nonnegative_update BEFORE UPDATE OF version ON {prefix}_thread_version WHEN NEW.version < 0 BEGIN SELECT RAISE(ABORT, 'negative thread version'); END;\
         CREATE TRIGGER {prefix}_receipt_nonnegative_insert BEFORE INSERT ON {prefix}_commit_receipt WHEN NEW.operation_ordinal < 0 OR NEW.commit_sequence < 0 OR NEW.thread_version < 0 BEGIN SELECT RAISE(ABORT, 'negative commit receipt authority'); END;\
         CREATE TRIGGER {prefix}_receipt_nonnegative_update BEFORE UPDATE OF operation_ordinal, commit_sequence, thread_version ON {prefix}_commit_receipt WHEN NEW.operation_ordinal < 0 OR NEW.commit_sequence < 0 OR NEW.thread_version < 0 BEGIN SELECT RAISE(ABORT, 'negative commit receipt authority'); END",
    )?);
    MigrationBundle::new(COMMIT_BUNDLE_ID, migrations)
}

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    fn dense_migration_versions_are_strictly_increasing() {
        let versions = [
            kani::any::<i64>(),
            kani::any::<i64>(),
            kani::any::<i64>(),
            kani::any::<i64>(),
        ];
        if versions_are_dense_from_one(&versions) {
            assert!(versions.windows(2).all(|pair| pair[0] < pair[1]));
        }
    }

    #[kani::proof]
    fn migration_step_never_rolls_back_or_skips_a_version() {
        let applied = kani::any::<u32>();
        let available = kani::any::<u32>();
        let next = migration_step(applied, available);
        assert!(next >= applied);
        assert!(next <= applied.saturating_add(1));
    }

    #[kani::proof]
    fn replaying_a_fully_applied_migration_plan_is_a_noop() {
        let version = kani::any::<u32>();
        assert_eq!(migration_step(version, version), version);
        let older_plan = version.saturating_sub(1);
        assert_eq!(migration_step(version, older_plan), version);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_bundle_lints_clean() {
        let bundle = commit_bundle().expect("bundle builds");
        awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).expect("bundle lints");
    }

    #[test]
    fn commit_bundle_has_one_table_per_committed_field_plus_authority_constraint() {
        let bundle = commit_bundle().expect("bundle builds");
        assert_eq!(bundle.migrations().len(), 9);
    }

    // The migrator requires a strictly increasing version stream; a duplicated or
    // out-of-order version (a copy-paste slip when appending a spec) must be caught
    // here, not at first migration against a live DB. Pin the stream is dense 1..=9.
    #[test]
    fn commit_bundle_versions_are_dense_and_strictly_increasing() {
        let bundle = commit_bundle().expect("bundle builds");
        let versions: Vec<i64> = bundle.migrations().iter().map(|m| m.version()).collect();
        assert_eq!(
            versions,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9],
            "dense 1..=9, in order"
        );
        assert!(
            versions.windows(2).all(|w| w[0] < w[1]),
            "versions strictly increase"
        );
    }

    // The bundle id is the scoped runtime-commit namespace, so the same DB can host
    // other components' migrations without collision.
    #[test]
    fn commit_bundle_carries_the_scoped_id() {
        let bundle = commit_bundle().expect("bundle builds");
        assert_eq!(bundle.bundle_id(), COMMIT_BUNDLE_ID);
        assert_eq!(COMMIT_BUNDLE_ID, "awaken.runtime_commit");
    }

    // Every portable token in the schema resolves to a concrete, dialect-specific
    // form, and no `{...}` token survives rendering. This pins the token vocabulary
    // the schema relies on so a future column that introduces an unsupported token
    // (a typo, or a token the migrator does not expand) is caught here rather than
    // as a raw `{token}` reaching a live DDL statement. The two dialects render
    // differently (JSONB vs TEXT, etc.), so both are checked.
    #[test]
    fn every_portable_token_renders_to_a_concrete_dialect_form() {
        use awaken_scoped_migration::{Dialect, render};

        for (_, _, template) in COMMIT_SPECS {
            for (dialect, json, ts, now, pk) in [
                (
                    Dialect::Postgres,
                    "JSONB",
                    "TIMESTAMPTZ",
                    "now()",
                    "BIGSERIAL",
                ),
                (
                    Dialect::Sqlite,
                    "TEXT",
                    "TEXT",
                    "CURRENT_TIMESTAMP",
                    "AUTOINCREMENT",
                ),
            ] {
                let sql = render(template, dialect, "runtime");
                // No token leaks through: every `{...}` was expanded (prefix included).
                assert!(
                    !sql.contains('{') && !sql.contains('}'),
                    "unexpanded token in {dialect:?}: {sql}"
                );
                assert!(
                    sql.contains("runtime_"),
                    "{{prefix}} expanded in {dialect:?}"
                );
                if template.contains("{json}") {
                    assert!(
                        sql.contains(json),
                        "{{json}} → {json} in {dialect:?}: {sql}"
                    );
                }
                if template.contains("{timestamptz}") {
                    assert!(sql.contains(ts), "{{timestamptz}} → {ts} in {dialect:?}");
                }
                if template.contains("{now}") {
                    assert!(sql.contains(now), "{{now}} → {now} in {dialect:?}");
                }
                if template.contains("{pk_autoinc}") {
                    assert!(sql.contains(pk), "{{pk_autoinc}} → {pk} in {dialect:?}");
                }
            }
        }
    }
}
