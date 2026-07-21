//! Process-start migration serialization for a shared Postgres database.
//!
//! PostgreSQL can race two concurrent `CREATE TABLE IF NOT EXISTS` statements while
//! it creates their implicit catalog rows. A fleet therefore takes one database-wide
//! advisory lock around composition-root startup migrations. The lock is deliberately
//! outside every bounded-context repository: repositories keep using the neutral
//! scoped-migration runner, while the process that composes several repositories owns
//! cross-process startup ordering.

use sqlx::Connection;
use sqlx::postgres::PgConnection;

const LOCK_SCOPE: &str = "awaken:postgres:start-migrations:v1";

/// A session-scoped Postgres advisory lock held by its dedicated connection.
///
/// Dropping the guard closes the connection and lets PostgreSQL release the lock.
/// [`release`](Self::release) is preferred so startup observes an unlock error.
pub struct PostgresMigrationLock {
    connection: PgConnection,
}

impl PostgresMigrationLock {
    /// Wait until this database's Awaken startup migrations may run exclusively.
    pub async fn acquire(url: &str) -> Result<Self, String> {
        let mut connection = PgConnection::connect(url)
            .await
            .map_err(|error| format!("connect Postgres migration lock: {error}"))?;
        sqlx::query("SELECT pg_advisory_lock(hashtextextended($1, 0))")
            .bind(LOCK_SCOPE)
            .execute(&mut connection)
            .await
            .map_err(|error| format!("acquire Postgres migration lock: {error}"))?;
        Ok(Self { connection })
    }

    /// Release the lock deterministically before the process starts serving.
    pub async fn release(mut self) -> Result<(), String> {
        let released: bool =
            sqlx::query_scalar("SELECT pg_advisory_unlock(hashtextextended($1, 0))")
                .bind(LOCK_SCOPE)
                .fetch_one(&mut self.connection)
                .await
                .map_err(|error| format!("release Postgres migration lock: {error}"))?;
        if !released {
            return Err("release Postgres migration lock: lock was not held".to_string());
        }
        self.connection
            .close()
            .await
            .map_err(|error| format!("close Postgres migration lock connection: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn lock_scope_is_versioned_and_product_owned() {
        assert!(LOCK_SCOPE.starts_with("awaken:"));
        assert!(LOCK_SCOPE.ends_with(":v1"));
    }

    #[tokio::test]
    async fn a_second_process_waits_until_the_first_releases() {
        let Ok(url) = std::env::var("AWAKEN_TEST_PG_URL") else {
            eprintln!("skip: AWAKEN_TEST_PG_URL unset");
            return;
        };
        let first = PostgresMigrationLock::acquire(&url)
            .await
            .expect("first process acquires the lock");
        let second_url = url.clone();
        let mut second =
            tokio::spawn(async move { PostgresMigrationLock::acquire(&second_url).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(150), &mut second)
                .await
                .is_err(),
            "the second process must wait while the first owns the lock"
        );
        first.release().await.expect("first process releases");
        let second = tokio::time::timeout(Duration::from_secs(5), second)
            .await
            .expect("second process wakes after release")
            .expect("second task joins")
            .expect("second process acquires");
        second.release().await.expect("second process releases");
    }
}
