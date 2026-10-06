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

use std::time::Duration;

mod clock_hold;
mod embedded_pool;
mod pool;
mod settle;
pub use clock_hold::held;
pub use embedded_pool::{EmbeddedConnection, EmbeddedPool, EmbeddedTransaction};
pub use pool::{EmbeddedSqlite, SqlitePoolError};

pub(crate) const WRITE_WAIT: Duration = Duration::from_secs(30);
