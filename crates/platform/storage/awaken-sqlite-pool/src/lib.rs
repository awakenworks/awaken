// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: Static owner: the one embedded SQLite connection policy shared by persistent store
// @formal reason: adapters (ADR-0087). Dynamic path: every store opens its one-connection sqlx pool through
// @formal reason: this crate, so every connection receives the same bounded wait and foreign-key policy,
// @formal reason: while a process-serialized one-time WAL transition prevents a redundant journal-mode
// @formal reason: write from racing another store's migration transaction. Evidence:
// @formal reason: store.backends.schema_and_adapter_conformance plus the pool's policy, admission,
// @formal reason: paused-clock and settle decision-table tests; this is a product-requirement
// @formal reason: classification, not a direct formal-proof claim.
//! Canonical SQLite connection policy for embedded process stores.
//!
//! Store adapters continue to own schemas, migrations, transactions, and domain
//! error mapping. This crate owns only connection-local process policy so two
//! adapters opening the same file cannot silently disagree about locking or
//! foreign-key enforcement. Stores reach SQLite only through
//! [`EmbeddedSqlite`] and the [`EmbeddedPool`] handle it opens.
//!
//! This owner qualifies the bundled SQLite build. Replacing it with an external
//! system library is outside this connection policy's supported build contract.

use std::time::Duration;

mod clock_hold;
mod embedded_pool;
mod identity;
mod pool;
mod settle;
pub use clock_hold::held;
pub use embedded_pool::{EmbeddedConnection, EmbeddedPool, EmbeddedTransaction};
pub use pool::{EmbeddedSqlite, SqlitePoolError};

pub(crate) const WRITE_WAIT: Duration = Duration::from_secs(30);

// The direct native dependency makes the floor part of standalone consumers'
// graphs as well as the complete process graph. No IAM dependency is required.
const SQLITE_VERSION_FLOOR: i32 = 3_054_001;
const _: () = assert!(libsqlite3_sys::SQLITE_VERSION_NUMBER >= SQLITE_VERSION_FLOOR);

#[cfg(test)]
mod version_tests {
    #[tokio::test]
    async fn bundled_driver_matches_the_security_qualified_headers() {
        let pool = super::EmbeddedSqlite::memory().open().await.unwrap();
        let version: String = sqlx::query_scalar("SELECT sqlite_version()")
            .fetch_one(&pool)
            .await
            .unwrap();
        let parts = version
            .split('.')
            .map(str::parse::<i32>)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            parts.len(),
            3,
            "SQLite must report its exact library version"
        );
        let number = parts[0]
            .checked_mul(1_000_000)
            .and_then(|major| {
                parts[1]
                    .checked_mul(1_000)
                    .and_then(|minor| major.checked_add(minor))
            })
            .and_then(|value| value.checked_add(parts[2]))
            .unwrap();
        assert!(number >= super::SQLITE_VERSION_FLOOR);
        assert_eq!(
            number,
            libsqlite3_sys::SQLITE_VERSION_NUMBER,
            "bundled headers and loaded library must be the same version"
        );
        pool.close().await;
    }
}
