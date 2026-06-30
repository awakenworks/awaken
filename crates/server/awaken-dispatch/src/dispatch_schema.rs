//! The durable dispatch schema, shared by the Postgres and SQLite backends.
//!
//! One portable [`MigrationBundle`] using the migrator's dialect-neutral tokens,
//! so the *same* bundle drives both runners. Two tables back the two aggregates:
//! `{prefix}_dispatch` is the run-dispatch queue (one row per accepted run with
//! its claim/lease state) and `{prefix}_pending` is the thread's pending input.

use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};

/// Bundle id for the durable dispatch schema. Scoped so it never collides with
/// the commit schema (`awaken.runtime_commit`) in a shared database.
pub const BUNDLE_ID: &str = "awaken.run_dispatch";

const SPECS: [(i64, &str, &str); 3] = [
    (
        1,
        "run-dispatch queue: one row per accepted run with claim/lease state",
        "CREATE TABLE {prefix}_dispatch (\
            run_id TEXT PRIMARY KEY, \
            thread_id TEXT NOT NULL, \
            request {json} NOT NULL, \
            status TEXT NOT NULL, \
            lease_owner TEXT, \
            lease_until BIGINT, \
            attempt_count BIGINT NOT NULL DEFAULT 0, \
            priority BIGINT NOT NULL DEFAULT 0, \
            dedupe_key TEXT, \
            created_at {timestamptz} NOT NULL DEFAULT {now})",
    ),
    (
        2,
        "thread pending input, delivered to the matching waiting-ticket correlation",
        "CREATE TABLE {prefix}_pending (\
            message_id TEXT PRIMARY KEY, \
            run_id TEXT NOT NULL, \
            thread_id TEXT NOT NULL, \
            correlation_id TEXT NOT NULL, \
            result {json} NOT NULL, \
            revision BIGINT NOT NULL DEFAULT 1, \
            available_at BIGINT, \
            created_at {timestamptz} NOT NULL DEFAULT {now})",
    ),
    (
        3,
        "cross-thread outbox: staged deliveries awaiting relay to a target thread",
        "CREATE TABLE {prefix}_outbox (\
            message_id TEXT PRIMARY KEY, \
            payload {json} NOT NULL, \
            created_at {timestamptz} NOT NULL DEFAULT {now})",
    ),
];

/// Build the dispatch-schema migration bundle.
pub fn dispatch_bundle() -> Result<MigrationBundle, MigrationError> {
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
    fn dispatch_bundle_lints_clean() {
        let bundle = dispatch_bundle().expect("bundle builds");
        awaken_scoped_migration::lint(std::slice::from_ref(&bundle)).expect("bundle lints");
    }
}
