//! The durable commit schema, shared by every store backend.
//!
//! The commit tables are a faithful projection of the staged `ThreadCommit`
//! (ADR-0008): the append-only commit/run-fact log (the phase authority and the
//! fence, G31/G32), the message transcript, the state-command log, committed
//! events, the run-record cache (a projection of the latest fact, G32), and
//! active waiting tickets. The schema is one portable [`MigrationBundle`] using
//! the migrator's dialect-neutral tokens, so the *same* bundle drives both the
//! Postgres (`sqlx`) and SQLite (`rusqlite`) runners. This crate names no SQL
//! driver — it owns the schema, the backends own the runner.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

/// The bundle id for the runtime commit schema. Scoped so it never collides with
/// another component's migrations in the same database.
pub const COMMIT_BUNDLE_ID: &str = "awaken.runtime_commit";

/// `(version, description, portable SQL)` for each commit table — one row per
/// field of the committed thread.
const COMMIT_SPECS: [(i64, &str, &str); 6] = [
    (
        1,
        "commit log: run-fact phase authority and the monotonic fence",
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
        "committed events with a monotonic sequence",
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
        "active waiting tickets, present only while a run is parked",
        "CREATE TABLE {prefix}_waiting (\
            run_id TEXT PRIMARY KEY, \
            ticket {json} NOT NULL)",
    ),
];

/// Build the commit-schema migration bundle. The version stream is independent
/// and strictly increasing; later schema changes append new specs.
pub fn commit_bundle() -> Result<MigrationBundle, MigrationError> {
    let migrations = COMMIT_SPECS
        .iter()
        .map(|(version, description, sql)| Migration::new(*version, *description, *sql))
        .collect::<Result<Vec<_>, _>>()?;
    MigrationBundle::new(COMMIT_BUNDLE_ID, migrations)
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
    fn commit_bundle_has_one_table_per_committed_field() {
        let bundle = commit_bundle().expect("bundle builds");
        assert_eq!(bundle.migrations().len(), 6);
    }

    // The migrator requires a strictly increasing version stream; a duplicated or
    // out-of-order version (a copy-paste slip when appending a spec) must be caught
    // here, not at first migration against a live DB. Pin the stream is dense 1..=6.
    #[test]
    fn commit_bundle_versions_are_dense_and_strictly_increasing() {
        let bundle = commit_bundle().expect("bundle builds");
        let versions: Vec<i64> = bundle.migrations().iter().map(|m| m.version()).collect();
        assert_eq!(versions, vec![1, 2, 3, 4, 5, 6], "dense 1..=6, in order");
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
}
