//! The durable commit schema, managed by `awaken-scoped-migration`.
//!
//! One scoped [`MigrationBundle`] owns the runtime's commit tables. Each table
//! mirrors a field of [`CommittedThread`](crate::CommittedThread): the append-only
//! commit/run-fact log (the phase authority and the fence, G31/G32), the message
//! transcript, the state-command log, committed events, the run-record cache (a
//! projection of the latest fact, G32), and active waiting tickets. The SQL uses
//! the migrator's portable tokens so the same bundle could target SQLite, but the
//! runtime ships only the Postgres runner.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

/// The bundle id for the runtime commit schema. Scoped so it never collides with
/// another component's migrations in the same database.
pub const BUNDLE_ID: &str = "awaken.runtime_commit";

/// `(version, description, portable SQL)` for each commit table — one row per
/// field of the committed thread. Data-driven so the bundle is one mapping over
/// the specs rather than repeated construction.
const SPECS: [(i64, &str, &str); 6] = [
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
    let migrations = SPECS
        .iter()
        .map(|(version, description, sql)| Migration::new(*version, *description, *sql))
        .collect::<Result<Vec<_>, _>>()?;
    MigrationBundle::new(BUNDLE_ID, migrations)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_bundle_lints_clean() {
        let bundle = commit_bundle().expect("bundle builds");
        // lint enforces unique strictly-increasing versions and bundle
        // independence (no migration references a table it does not create).
        awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).expect("bundle lints");
    }

    #[test]
    fn commit_bundle_has_one_table_per_committed_field() {
        let bundle = commit_bundle().expect("bundle builds");
        assert_eq!(bundle.migrations().len(), 6);
    }
}
