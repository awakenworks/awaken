use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rusqlite::{Connection, Error as SqliteError, ErrorCode, OptionalExtension, params};

use super::schema::{NAMESPACE, bundle};
use super::{
    ApplicationAccessRecord, ApplicationAccessRepository, ApplicationAccessRepositoryError,
    ApplicationAccessTokenHash, ApplicationAccessTokenId, StoredApplicationAccessRow,
};

const SQLITE_WRITE_WAIT: Duration = Duration::from_secs(30);

pub(super) struct SqliteApplicationAccessRepository {
    pub(super) connection: Arc<Mutex<Connection>>,
}

impl SqliteApplicationAccessRepository {
    pub(super) fn open(path: &str) -> Result<Self, ApplicationAccessRepositoryError> {
        let connection = Connection::open(path).map_err(unavailable)?;
        Self::from_connection(connection)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn open_in_memory() -> Result<Self, ApplicationAccessRepositoryError> {
        let connection = Connection::open_in_memory().map_err(unavailable)?;
        Self::from_connection(connection)
    }

    fn from_connection(connection: Connection) -> Result<Self, ApplicationAccessRepositoryError> {
        connection
            .busy_timeout(SQLITE_WRITE_WAIT)
            .map_err(unavailable)?;
        awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NAMESPACE)
            .map_err(|error| ApplicationAccessRepositoryError::Unavailable(error.to_string()))?
            .run_bundle(
                &connection,
                &bundle().map_err(|error| {
                    ApplicationAccessRepositoryError::Unavailable(error.to_string())
                })?,
            )
            .map_err(|error| ApplicationAccessRepositoryError::Unavailable(error.to_string()))?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    /// Follow the existing SQLite adapter runtime model: synchronous rusqlite
    /// work runs on Tokio's shared blocking pool behind the connection mutex.
    /// This creates neither a nested runtime nor a repository-specific executor.
    async fn with_connection<T, F>(
        &self,
        operation: F,
    ) -> Result<T, ApplicationAccessRepositoryError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, ApplicationAccessRepositoryError> + Send + 'static,
    {
        let connection = self.connection.clone();
        tokio::task::spawn_blocking(move || {
            let connection = connection.lock().map_err(lock_unavailable)?;
            operation(&connection)
        })
        .await
        .map_err(|error| {
            ApplicationAccessRepositoryError::Unavailable(format!(
                "join application access SQLite operation: {error}"
            ))
        })?
    }
}

#[async_trait]
impl ApplicationAccessRepository for SqliteApplicationAccessRepository {
    async fn create(
        &self,
        record: ApplicationAccessRecord,
    ) -> Result<(), ApplicationAccessRepositoryError> {
        let row = record.stored_row()?;
        self.with_connection(move |connection| {
            connection
                .execute(
                    "INSERT INTO application_access_credential
                    (token_id, token_hash, workspace_id, created_at_unix_ms,
                     expires_at_unix_ms, revoked_at_unix_ms, grant_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        row.token_id,
                        row.token_hash,
                        row.workspace_id,
                        row.created_at_unix_ms,
                        row.expires_at_unix_ms,
                        row.revoked_at_unix_ms,
                        row.grant_json,
                    ],
                )
                .map_err(insert_error)?;
            Ok(())
        })
        .await
    }

    async fn find_by_token_hash(
        &self,
        token_hash: &ApplicationAccessTokenHash,
    ) -> Result<ApplicationAccessRecord, ApplicationAccessRepositoryError> {
        let token_hash = token_hash.as_str().to_owned();
        self.with_connection(move |connection| {
            let row = connection
                .query_row(
                    "SELECT token_id, token_hash, workspace_id, created_at_unix_ms,
                        expires_at_unix_ms, revoked_at_unix_ms, grant_json
                 FROM application_access_credential WHERE token_hash = ?1",
                    [token_hash],
                    |row| {
                        Ok(StoredApplicationAccessRow {
                            token_id: row.get(0)?,
                            token_hash: row.get(1)?,
                            workspace_id: row.get(2)?,
                            created_at_unix_ms: row.get(3)?,
                            expires_at_unix_ms: row.get(4)?,
                            revoked_at_unix_ms: row.get(5)?,
                            grant_json: row.get(6)?,
                        })
                    },
                )
                .optional()
                .map_err(read_error)?
                .ok_or(ApplicationAccessRepositoryError::NotFound)?;
            ApplicationAccessRecord::from_storage(row)
        })
        .await
    }

    async fn revoke(
        &self,
        workspace_id: &str,
        id: &ApplicationAccessTokenId,
        revoked_at_unix_ms: u64,
    ) -> Result<(), ApplicationAccessRepositoryError> {
        let revoked_at_unix_ms = i64::try_from(revoked_at_unix_ms).map_err(|_| {
            ApplicationAccessRepositoryError::Invalid(
                "revoked_at_unix_ms exceeds i64 storage".into(),
            )
        })?;
        let workspace_id = workspace_id.to_owned();
        let id = id.to_string();
        self.with_connection(move |connection| {
            let changed = connection
                .execute(
                    "UPDATE application_access_credential
                 SET revoked_at_unix_ms = COALESCE(revoked_at_unix_ms, ?1)
                 WHERE workspace_id = ?2 AND token_id = ?3",
                    params![revoked_at_unix_ms, workspace_id, id],
                )
                .map_err(unavailable)?;
            if changed == 0 {
                return Err(ApplicationAccessRepositoryError::NotFound);
            }
            Ok(())
        })
        .await
    }

    async fn delete_terminal_before(
        &self,
        terminal_before_unix_ms: u64,
        limit: u32,
    ) -> Result<u64, ApplicationAccessRepositoryError> {
        let terminal_before_unix_ms = i64::try_from(terminal_before_unix_ms).map_err(|_| {
            ApplicationAccessRepositoryError::Invalid(
                "terminal_before_unix_ms exceeds i64 storage".into(),
            )
        })?;
        self.with_connection(move |connection| {
            let deleted = connection
                .execute(
                    "DELETE FROM application_access_credential
                     WHERE token_id IN (
                         SELECT token_id FROM application_access_credential
                         WHERE expires_at_unix_ms < ?1
                            OR (revoked_at_unix_ms IS NOT NULL
                                AND revoked_at_unix_ms < ?1)
                         ORDER BY token_id
                         LIMIT ?2
                     )",
                    params![terminal_before_unix_ms, i64::from(limit)],
                )
                .map_err(unavailable)?;
            u64::try_from(deleted).map_err(|_| {
                ApplicationAccessRepositoryError::Corrupt(
                    "negative SQLite retention deletion count".into(),
                )
            })
        })
        .await
    }
}

fn lock_unavailable<T>(error: std::sync::PoisonError<T>) -> ApplicationAccessRepositoryError {
    ApplicationAccessRepositoryError::Unavailable(error.to_string())
}

fn unavailable(error: rusqlite::Error) -> ApplicationAccessRepositoryError {
    ApplicationAccessRepositoryError::Unavailable(error.to_string())
}

fn read_error(error: SqliteError) -> ApplicationAccessRepositoryError {
    let corrupt = matches!(
        &error,
        SqliteError::FromSqlConversionFailure(..)
            | SqliteError::IntegralValueOutOfRange(..)
            | SqliteError::Utf8Error(..)
            | SqliteError::InvalidColumnIndex(..)
            | SqliteError::InvalidColumnName(..)
            | SqliteError::InvalidColumnType(..)
    ) || matches!(
        error.sqlite_error_code(),
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase)
    );
    if corrupt {
        ApplicationAccessRepositoryError::Corrupt(error.to_string())
    } else {
        unavailable(error)
    }
}

fn insert_error(error: rusqlite::Error) -> ApplicationAccessRepositoryError {
    if error.sqlite_error_code() == Some(ErrorCode::ConstraintViolation) {
        ApplicationAccessRepositoryError::Invalid("token id or token hash already exists".into())
    } else {
        unavailable(error)
    }
}
