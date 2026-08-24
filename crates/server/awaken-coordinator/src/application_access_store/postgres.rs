use async_trait::async_trait;
use sqlx::Row;
use sqlx::postgres::PgPool;

use super::schema::{NAMESPACE, bundle};
use super::{
    ApplicationAccessRecord, ApplicationAccessRepository, ApplicationAccessRepositoryError,
    ApplicationAccessTokenHash, ApplicationAccessTokenId, StoredApplicationAccessRow,
};

pub(super) struct PostgresApplicationAccessRepository {
    pool: PgPool,
}

impl PostgresApplicationAccessRepository {
    pub(super) async fn migrate(url: &str) -> Result<(), ApplicationAccessRepositoryError> {
        let pool = PgPool::connect(url).await.map_err(unavailable)?;
        let result = Self::migrate_pool(&pool).await;
        pool.close().await;
        result
    }

    pub(super) async fn connect(url: &str) -> Result<Self, ApplicationAccessRepositoryError> {
        let pool = PgPool::connect(url).await.map_err(unavailable)?;
        Self::with_pool(pool).await
    }

    pub(super) async fn connect_existing(
        url: &str,
    ) -> Result<Self, ApplicationAccessRepositoryError> {
        let pool = PgPool::connect(url).await.map_err(unavailable)?;
        Self::with_existing_pool(pool).await
    }

    pub(super) async fn with_pool(pool: PgPool) -> Result<Self, ApplicationAccessRepositoryError> {
        Self::migrate_pool(&pool).await?;
        Ok(Self { pool })
    }

    pub(super) async fn migrate_pool(
        pool: &PgPool,
    ) -> Result<(), ApplicationAccessRepositoryError> {
        let bundle = bundle()
            .map_err(|error| ApplicationAccessRepositoryError::Unavailable(error.to_string()))?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(
            pool.clone(),
            NAMESPACE,
        )
        .map_err(|error| ApplicationAccessRepositoryError::Unavailable(error.to_string()))?
        .run_bundle(&bundle)
        .await
        .map_err(|error| ApplicationAccessRepositoryError::Unavailable(error.to_string()))?;
        Ok(())
    }

    pub(super) async fn with_existing_pool(
        pool: PgPool,
    ) -> Result<Self, ApplicationAccessRepositoryError> {
        let bundle = bundle()
            .map_err(|error| ApplicationAccessRepositoryError::Unavailable(error.to_string()))?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(
            pool.clone(),
            NAMESPACE,
        )
        .map_err(|error| ApplicationAccessRepositoryError::Unavailable(error.to_string()))?
        .verify_bundle(&bundle)
        .await
        .map_err(|error| ApplicationAccessRepositoryError::Unavailable(error.to_string()))?;
        Ok(Self { pool })
    }
}

#[async_trait]
impl ApplicationAccessRepository for PostgresApplicationAccessRepository {
    async fn create(
        &self,
        record: ApplicationAccessRecord,
    ) -> Result<(), ApplicationAccessRepositoryError> {
        let row = record.stored_row()?;
        sqlx::query(
            "INSERT INTO application_access_credential
                (token_id, token_hash, workspace_id, created_at_unix_ms,
                 expires_at_unix_ms, revoked_at_unix_ms, grant_json)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
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
        let row = sqlx::query(
            "SELECT token_id, token_hash, workspace_id, created_at_unix_ms,
                    expires_at_unix_ms, revoked_at_unix_ms, grant_json
             FROM application_access_credential WHERE token_hash = $1",
        )
        .bind(token_hash.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(unavailable)?
        .ok_or(ApplicationAccessRepositoryError::NotFound)?;
        ApplicationAccessRecord::from_storage(StoredApplicationAccessRow {
            token_id: row.try_get("token_id").map_err(corrupt_decode)?,
            token_hash: row.try_get("token_hash").map_err(corrupt_decode)?,
            workspace_id: row.try_get("workspace_id").map_err(corrupt_decode)?,
            created_at_unix_ms: row.try_get("created_at_unix_ms").map_err(corrupt_decode)?,
            expires_at_unix_ms: row.try_get("expires_at_unix_ms").map_err(corrupt_decode)?,
            revoked_at_unix_ms: row.try_get("revoked_at_unix_ms").map_err(corrupt_decode)?,
            grant_json: row.try_get("grant_json").map_err(corrupt_decode)?,
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
             SET revoked_at_unix_ms = COALESCE(revoked_at_unix_ms, $1)
             WHERE workspace_id = $2 AND token_id = $3",
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
        sqlx::query(
            "WITH candidates AS (
                 SELECT token_id FROM application_access_credential
                 WHERE expires_at_unix_ms < $1
                    OR (revoked_at_unix_ms IS NOT NULL
                        AND revoked_at_unix_ms < $1)
                 ORDER BY token_id
                 FOR UPDATE SKIP LOCKED
                 LIMIT $2
             )
             DELETE FROM application_access_credential AS credential
             USING candidates
             WHERE credential.token_id = candidates.token_id",
        )
        .bind(terminal_before_unix_ms)
        .bind(i64::from(limit))
        .execute(&self.pool)
        .await
        .map_err(unavailable)
        .map(|result| result.rows_affected())
    }
}

fn unavailable(error: sqlx::Error) -> ApplicationAccessRepositoryError {
    ApplicationAccessRepositoryError::Unavailable(error.to_string())
}

fn corrupt_decode(error: sqlx::Error) -> ApplicationAccessRepositoryError {
    ApplicationAccessRepositoryError::Corrupt(error.to_string())
}

fn insert_error(error: sqlx::Error) -> ApplicationAccessRepositoryError {
    if error
        .as_database_error()
        .is_some_and(|database_error| database_error.is_unique_violation())
    {
        ApplicationAccessRepositoryError::Invalid("token id or token hash already exists".into())
    } else {
        unavailable(error)
    }
}
