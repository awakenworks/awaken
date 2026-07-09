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

const SPECS: [(i64, &str, &str); 10] = [
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
            epoch BIGINT NOT NULL DEFAULT 0, \
            dead_lettered_at BIGINT, \
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
    // Claim/lease indexes. Without these, every claim/renew/reap on the dispatch
    // queue is a sequential scan — fine for a handful of rows, quadratic once the
    // queue holds hundreds of thousands of active runs. Each index is prefixed
    // like its table so several runtimes can share one database without a name
    // collision. `status`, `lease_owner`, `thread_id`, and `dedupe_key` are all
    // equality predicates in the claim policy; the trailing columns match the
    // `ORDER BY` so the planner reads rows already in pick order.
    (
        4,
        "index: status-scoped claim ordering (fresh pick, parked wake, status scans)",
        "CREATE INDEX {prefix}_dispatch_claim_idx \
         ON {prefix}_dispatch (status, priority, created_at)",
    ),
    (
        5,
        "index: expired-lease recovery and reap by lease deadline",
        "CREATE INDEX {prefix}_dispatch_lease_idx \
         ON {prefix}_dispatch (status, lease_until)",
    ),
    (
        6,
        "index: renew all leases held by one owner",
        "CREATE INDEX {prefix}_dispatch_owner_idx ON {prefix}_dispatch (lease_owner)",
    ),
    (
        7,
        "index: per-thread supersession and parked-run lookup",
        "CREATE INDEX {prefix}_dispatch_thread_idx ON {prefix}_dispatch (thread_id)",
    ),
    (
        8,
        "index: dedupe-key existence check on enqueue",
        "CREATE INDEX {prefix}_dispatch_dedupe_idx ON {prefix}_dispatch (dedupe_key)",
    ),
    (
        9,
        "index: a run's pending input (claim hand-off, settle, cancel, wake test)",
        "CREATE INDEX {prefix}_pending_run_idx ON {prefix}_pending (run_id)",
    ),
    (
        10,
        "index: a thread's pending inbox listing",
        "CREATE INDEX {prefix}_pending_thread_idx ON {prefix}_pending (thread_id)",
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

    #[test]
    fn index_migrations_render_for_both_dialects() {
        use awaken_scoped_migration::Dialect;
        let bundle = dispatch_bundle().expect("bundle builds");
        // The three tables plus one index migration each for the seven hot
        // claim/lease/pending queries.
        assert_eq!(bundle.migrations().len(), 10);

        let indexes: Vec<_> = bundle
            .migrations()
            .iter()
            .filter(|m| m.description().starts_with("index:"))
            .collect();
        assert_eq!(indexes.len(), 7, "seven claim/lease indexes");

        for migration in indexes {
            for dialect in [Dialect::Postgres, Dialect::Sqlite] {
                let sql =
                    awaken_scoped_migration::render(migration.sql_for(dialect), dialect, NS);
                assert!(sql.contains("CREATE INDEX"), "{sql}");
                // Every index name and target table carries the runtime prefix so
                // co-located runtimes never collide, and no token survives.
                assert!(sql.contains(&format!("{NS}_")), "prefixed: {sql}");
                assert!(!sql.contains('{'), "no leftover token: {sql}");
            }
        }
    }
}

/// The runtime table prefix, mirrored from `postgres::NS`/`sqlite::NS` so the
/// render test can assert the prefixing without reaching into a backend module.
#[cfg(test)]
const NS: &str = "runtime";
