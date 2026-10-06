// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: SQLite adapter for the sole Coordinator application-access repository; this is
// @formal reason: product-requirement classification, not a formal-proof claim.
use async_trait::async_trait;
use awaken_scoped_migration::sqlite::SqliteMigrationRunner;
use awaken_sqlite_pool::{EmbeddedPool, EmbeddedSqlite};

use super::schema::{NAMESPACE, bundle};
use super::{
    ApplicationAccessRecord, ApplicationAccessRepository, ApplicationAccessRepositoryError,
    ApplicationAccessTokenHash, ApplicationAccessTokenId, StoredApplicationAccessRow,
};

pub(super) struct SqliteApplicationAccessRepository {
    pool: EmbeddedPool,
}

impl SqliteApplicationAccessRepository {
    pub(super) async fn open(path: &str) -> Result<Self, ApplicationAccessRepositoryError> {
        Self::open_at(EmbeddedSqlite::file(path)).await
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(super) async fn open_in_memory() -> Result<Self, ApplicationAccessRepositoryError> {
        Self::open_at(EmbeddedSqlite::memory()).await
    }

    async fn open_at(location: EmbeddedSqlite) -> Result<Self, ApplicationAccessRepositoryError> {
        let pool = location.open().await.map_err(unavailable)?;
        let bundle = bundle().map_err(unavailable)?;
        awaken_sqlite_pool::held(
            SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), NAMESPACE)
                .map_err(unavailable)?
                .run_bundle(&bundle),
        )
        .await
        .map_err(unavailable)?;
        Ok(Self { pool })
    }
}

type StoredRow = (String, String, String, i64, i64, Option<i64>, String);

#[async_trait]
impl ApplicationAccessRepository for SqliteApplicationAccessRepository {
    async fn create(
        &self,
        record: ApplicationAccessRecord,
    ) -> Result<(), ApplicationAccessRepositoryError> {
        let row = record.stored_row()?;
        sqlx::query(
            "INSERT INTO application_access_credential
                (token_id, token_hash, workspace_id, created_at_unix_ms,
                 expires_at_unix_ms, revoked_at_unix_ms, grant_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(row.token_id)
        .bind(row.token_hash)
        .bind(row.workspace_id)
        .bind(row.created_at_unix_ms)
        .bind(row.expires_at_unix_ms)
        .bind(row.revoked_at_unix_ms)
        .bind(row.grant_json)
        .execute(&self.pool)
        .await
        .map_err(insert_error)?;
        Ok(())
    }

    async fn find_by_token_hash(
        &self,
        token_hash: &ApplicationAccessTokenHash,
    ) -> Result<ApplicationAccessRecord, ApplicationAccessRepositoryError> {
        let (
            token_id,
            token_hash,
            workspace_id,
            created_at_unix_ms,
            expires_at_unix_ms,
            revoked_at_unix_ms,
            grant_json,
        ) = sqlx::query_as::<_, StoredRow>(
            "SELECT token_id, token_hash, workspace_id, created_at_unix_ms,
                    expires_at_unix_ms, revoked_at_unix_ms, grant_json
             FROM application_access_credential WHERE token_hash = ?1",
        )
        .bind(token_hash.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(read_error)?
        .ok_or(ApplicationAccessRepositoryError::NotFound)?;
        ApplicationAccessRecord::from_storage(StoredApplicationAccessRow {
            token_id,
            token_hash,
            workspace_id,
            created_at_unix_ms,
            expires_at_unix_ms,
            revoked_at_unix_ms,
            grant_json,
        })
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
        let changed = sqlx::query(
            "UPDATE application_access_credential
             SET revoked_at_unix_ms = COALESCE(revoked_at_unix_ms, ?1)
             WHERE workspace_id = ?2 AND token_id = ?3",
        )
        .bind(revoked_at_unix_ms)
        .bind(workspace_id)
        .bind(id.to_string())
        .execute(&self.pool)
        .await
        .map_err(unavailable)?
        .rows_affected();
        if changed == 0 {
            return Err(ApplicationAccessRepositoryError::NotFound);
        }
        Ok(())
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
        let deleted = sqlx::query(
            "DELETE FROM application_access_credential
             WHERE token_id IN (
                 SELECT token_id FROM application_access_credential
                 WHERE expires_at_unix_ms < ?1
                    OR (revoked_at_unix_ms IS NOT NULL
                        AND revoked_at_unix_ms < ?1)
                 ORDER BY token_id
                 LIMIT ?2
             )",
        )
        .bind(terminal_before_unix_ms)
        .bind(i64::from(limit))
        .execute(&self.pool)
        .await
        .map_err(unavailable)?;
        Ok(deleted.rows_affected())
    }
}

fn unavailable(error: impl std::fmt::Display) -> ApplicationAccessRepositoryError {
    ApplicationAccessRepositoryError::Unavailable(error.to_string())
}

/// The primary SQLite result code (the low byte of an extended code).
fn primary_code(error: &sqlx::Error) -> Option<i32> {
    error
        .as_database_error()
        .and_then(|error| error.code())
        .and_then(|code| code.parse::<i32>().ok())
        .map(|code| code & 0xff)
}

/// A row that cannot be decoded, or a file that is not an intact database, is
/// corrupt; every other read failure leaves the authority unavailable.
fn read_error(error: sqlx::Error) -> ApplicationAccessRepositoryError {
    const SQLITE_CORRUPT: i32 = 11;
    const SQLITE_NOTADB: i32 = 26;
    let corrupt = matches!(
        &error,
        sqlx::Error::ColumnDecode { .. }
            | sqlx::Error::Decode(..)
            | sqlx::Error::ColumnIndexOutOfBounds { .. }
            | sqlx::Error::ColumnNotFound(..)
    ) || matches!(primary_code(&error), Some(SQLITE_CORRUPT | SQLITE_NOTADB));
    if corrupt {
        ApplicationAccessRepositoryError::Corrupt(error.to_string())
    } else {
        unavailable(error)
    }
}

fn insert_error(error: sqlx::Error) -> ApplicationAccessRepositoryError {
    const SQLITE_CONSTRAINT: i32 = 19;
    if primary_code(&error) == Some(SQLITE_CONSTRAINT) {
        ApplicationAccessRepositoryError::Invalid("token id or token hash already exists".into())
    } else {
        unavailable(error)
    }
}
