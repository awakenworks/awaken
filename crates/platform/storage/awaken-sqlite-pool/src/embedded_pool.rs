// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: Static owner: the one handle through which embedded stores reach SQLite (ADR-0087).
// @formal reason: Dynamic path: every query, transaction and held connection keeps a paused clock still
// @formal reason: for its whole span, including its wait for the one connection, and the handle counts
// @formal reason: as live for its file until dropped. Evidence: the pool's admission, paused-clock and
// @formal reason: settle decision-table tests; this is a product-requirement classification, not a
// @formal reason: direct formal-proof claim.
//! The handle an embedded store holds: a one-connection sqlx pool that keeps a
//! paused clock still for the whole span of each piece of SQLite work.
//!
//! Each query, transaction and held connection takes its hold when it is
//! requested, so the hold also covers the wait for the one connection. That wait
//! includes the moment sqlx spends returning the previous caller's connection on
//! its own thread, which a hold taken per lent-out connection would leave open.

use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use futures_core::future::BoxFuture;
use futures_core::stream::BoxStream;
use futures_util::StreamExt as _;
use sqlx::pool::PoolConnection;
use sqlx::sqlite::{
    Sqlite, SqliteConnection, SqlitePool, SqliteQueryResult, SqliteRow, SqliteStatement,
    SqliteTypeInfo,
};
use sqlx::{Describe, Either, Execute, Executor, SqlSafeStr, SqlStr, Transaction};

use crate::clock_hold::{ClockHold, held};
use crate::settle::LiveFile;

/// An embedded store's SQLite handle. Clones share the one connection.
#[derive(Clone)]
pub struct EmbeddedPool {
    inner: Arc<Inner>,
}

struct Inner {
    pool: SqlitePool,
    _live: Option<LiveFile>,
    database: Option<crate::identity::DatabaseFile>,
}

impl fmt::Debug for EmbeddedPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EmbeddedPool").finish_non_exhaustive()
    }
}

impl EmbeddedPool {
    pub(crate) fn new(
        pool: SqlitePool,
        live: Option<LiveFile>,
        database: Option<crate::identity::DatabaseFile>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                pool,
                _live: live,
                database,
            }),
        }
    }

    /// Whether both handles observe the same opened main database.
    ///
    /// Separate private memory databases and independent files are distinct.
    /// A removed or replaced file is an error, never evidence for transferring
    /// an existing transaction. This observation grants no domain authority.
    pub fn shares_database(&self, other: &Self) -> Result<bool, sqlx::Error> {
        match (&self.inner.database, &other.inner.database) {
            (Some(left), Some(right)) => left.same_current_file(right),
            (None, None) => Ok(Arc::ptr_eq(&self.inner, &other.inner)),
            _ => Ok(false),
        }
    }

    /// Start a deferred transaction.
    pub async fn begin(&self) -> Result<EmbeddedTransaction, sqlx::Error> {
        let hold = ClockHold::take();
        let transaction = self.inner.pool.begin().await?;
        Ok(EmbeddedTransaction {
            transaction,
            _hold: hold,
        })
    }

    /// Start a transaction with an explicit statement, such as `BEGIN IMMEDIATE`.
    /// Dynamic SQL must first pass the caller's explicit SQLx safety assertion.
    ///
    /// ```compile_fail,E0277
    /// async fn unreviewed(pool: &awaken_sqlite_pool::EmbeddedPool, sql: String) {
    ///     let _ = pool.begin_with(sql).await;
    /// }
    /// ```
    pub async fn begin_with(
        &self,
        statement: impl SqlSafeStr,
    ) -> Result<EmbeddedTransaction, sqlx::Error> {
        let hold = ClockHold::take();
        let transaction = self.inner.pool.begin_with(statement).await?;
        Ok(EmbeddedTransaction {
            transaction,
            _hold: hold,
        })
    }

    /// Hold the one connection for several statements.
    pub async fn acquire(&self) -> Result<EmbeddedConnection, sqlx::Error> {
        let hold = ClockHold::take();
        let connection = self.inner.pool.acquire().await?;
        Ok(EmbeddedConnection {
            connection,
            _hold: hold,
        })
    }

    /// Close the connection and wait until SQLite has closed it.
    pub async fn close(&self) {
        held(self.inner.pool.close()).await;
    }

    /// The underlying pool, for APIs that take an sqlx pool, such as the schema
    /// migration runner. Run work on it inside [`crate::held`].
    #[must_use]
    pub fn sqlx(&self) -> &SqlitePool {
        &self.inner.pool
    }
}

impl<'c> Executor<'c> for &'c EmbeddedPool {
    type Database = Sqlite;

    fn fetch_many<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> BoxStream<'e, Result<Either<SqliteQueryResult, SqliteRow>, sqlx::Error>>
    where
        'c: 'e,
        E: 'q + Execute<'q, Sqlite>,
    {
        let hold = ClockHold::take();
        Box::pin(self.inner.pool.fetch_many(query).map(move |item| {
            let _held = &hold;
            item
        }))
    }

    fn fetch_optional<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> BoxFuture<'e, Result<Option<SqliteRow>, sqlx::Error>>
    where
        'c: 'e,
        E: 'q + Execute<'q, Sqlite>,
    {
        let hold = ClockHold::take();
        let fetch = self.inner.pool.fetch_optional(query);
        Box::pin(async move {
            let _held = hold;
            fetch.await
        })
    }

    fn prepare_with<'e>(
        self,
        sql: SqlStr,
        parameters: &'e [SqliteTypeInfo],
    ) -> BoxFuture<'e, Result<SqliteStatement, sqlx::Error>>
    where
        'c: 'e,
    {
        let hold = ClockHold::take();
        let prepare = self.inner.pool.prepare_with(sql, parameters);
        Box::pin(async move {
            let _held = hold;
            prepare.await
        })
    }

    fn describe<'e>(self, sql: SqlStr) -> BoxFuture<'e, Result<Describe<Sqlite>, sqlx::Error>>
    where
        'c: 'e,
    {
        let hold = ClockHold::take();
        let describe = self.inner.pool.describe(sql);
        Box::pin(async move {
            let _held = hold;
            describe.await
        })
    }
}

/// A transaction on the store's connection. Dropping it rolls back.
pub struct EmbeddedTransaction {
    transaction: Transaction<'static, Sqlite>,
    _hold: Option<ClockHold>,
}

impl EmbeddedTransaction {
    pub async fn commit(self) -> Result<(), sqlx::Error> {
        self.transaction.commit().await
    }

    pub async fn rollback(self) -> Result<(), sqlx::Error> {
        self.transaction.rollback().await
    }
}

impl Deref for EmbeddedTransaction {
    type Target = SqliteConnection;

    fn deref(&self) -> &SqliteConnection {
        &self.transaction
    }
}

impl DerefMut for EmbeddedTransaction {
    fn deref_mut(&mut self) -> &mut SqliteConnection {
        &mut self.transaction
    }
}

/// The store's connection, held for several statements.
pub struct EmbeddedConnection {
    connection: PoolConnection<Sqlite>,
    _hold: Option<ClockHold>,
}

impl Deref for EmbeddedConnection {
    type Target = SqliteConnection;

    fn deref(&self) -> &SqliteConnection {
        &self.connection
    }
}

impl DerefMut for EmbeddedConnection {
    fn deref_mut(&mut self) -> &mut SqliteConnection {
        &mut self.connection
    }
}
