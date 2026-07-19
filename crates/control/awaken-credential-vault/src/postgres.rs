//! Postgres adapters (feature `postgres`, ADR-0043) for the credential domain,
//! over the crate's own `credential` migration scope ([`credential_bundle`]) —
//! the network-DB siblings of the sqlite backends: [`PostgresCredentialRepo`]
//! persists the **secret-free** source/pool rows (serde in the `data {json}`
//! jsonb column, keyed columns for lookups), and [`PostgresSealedBlobStore`]
//! persists opaque sealed blobs (`nonce ‖ ciphertext`) in `{prefix}_secret`.
//!
//! As with sqlite, there is deliberately **no bare Postgres
//! [`SecretStore`](crate::SecretStore)** — it would write plaintext at rest. The
//! one durable secret path is the AEAD decorator over the blob port:
//! `SealedAeadSecretStore::over(&key, Arc::new(PostgresSealedBlobStore::connect(..).await?))`
//! (features `sealed-aead` + `postgres`), which stores only sealed bytes.

use sqlx::Row;
use sqlx::postgres::PgPool;
use sqlx::types::Json;

use crate::repo::CredentialRepo;
use crate::schema::credential_bundle;
use crate::{
    CredentialError, CredentialPool, CredentialPoolId, CredentialSource, CredentialSourceId,
    SealedBlobStore, SecretRef,
};

/// The credential component's table namespace (its bundle prefix).
const NS: &str = "credential";

/// Errors from connecting or migrating the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("connect: {0}")]
    Connect(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

async fn connect_migrated(url: &str) -> Result<PgPool, StoreError> {
    let pool = PgPool::connect(url)
        .await
        .map_err(|err| StoreError::Connect(err.to_string()))?;
    pool_migrated(pool).await
}

async fn pool_migrated(pool: PgPool) -> Result<PgPool, StoreError> {
    let bundle = credential_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
    awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
        .map_err(|err| StoreError::Migrate(err.to_string()))?
        .run_bundle(&bundle)
        .await
        .map_err(|err| StoreError::Migrate(err.to_string()))?;
    Ok(pool)
}

fn storage(err: impl std::fmt::Display) -> CredentialError {
    CredentialError::Storage(err.to_string())
}

/// A Postgres-backed [`CredentialRepo`] (secret-free rows only; the sealed
/// material goes through [`PostgresSealedBlobStore`]).
pub struct PostgresCredentialRepo {
    pool: PgPool,
}

impl PostgresCredentialRepo {
    /// Connect and apply the credential migrations under the `credential` namespace.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        Ok(Self {
            pool: connect_migrated(url).await?,
        })
    }

    /// Build from an existing pool: apply the credential migrations.
    pub async fn with_pool(pool: PgPool) -> Result<Self, StoreError> {
        Ok(Self {
            pool: pool_migrated(pool).await?,
        })
    }
}

#[async_trait::async_trait]
impl CredentialRepo for PostgresCredentialRepo {
    async fn put(&self, source: CredentialSource) -> Result<(), CredentialError> {
        let p = NS;
        sqlx::query(&format!(
            "INSERT INTO {p}_source (id, workspace_id, data) VALUES ($1, $2, $3) \
             ON CONFLICT (id) DO UPDATE SET \
             workspace_id = excluded.workspace_id, data = excluded.data"
        ))
        .bind(&source.id.0)
        .bind(&source.workspace_id)
        .bind(Json(&source))
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn get(&self, id: &CredentialSourceId) -> Result<CredentialSource, CredentialError> {
        let p = NS;
        let row = sqlx::query(&format!("SELECT data FROM {p}_source WHERE id = $1"))
            .bind(&id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(storage)?;
        let row = row.ok_or_else(|| CredentialError::SourceNotFound(id.0.clone()))?;
        let Json(source): Json<CredentialSource> = row.try_get("data").map_err(storage)?;
        Ok(source)
    }

    async fn list(&self, workspace_id: &str) -> Result<Vec<CredentialSource>, CredentialError> {
        let p = NS;
        let rows = sqlx::query(&format!(
            "SELECT data FROM {p}_source WHERE workspace_id = $1 ORDER BY id"
        ))
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        rows.into_iter()
            .map(|row| {
                let Json(source): Json<CredentialSource> = row.try_get("data").map_err(storage)?;
                Ok(source)
            })
            .collect()
    }

    async fn put_pool(&self, pool: CredentialPool) -> Result<(), CredentialError> {
        let p = NS;
        sqlx::query(&format!(
            "INSERT INTO {p}_pool (id, workspace_id, data) VALUES ($1, $2, $3) \
             ON CONFLICT (id) DO UPDATE SET \
             workspace_id = excluded.workspace_id, data = excluded.data"
        ))
        .bind(&pool.id.0)
        .bind(&pool.workspace_id)
        .bind(Json(&pool))
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn get_pool(&self, id: &CredentialPoolId) -> Result<CredentialPool, CredentialError> {
        let p = NS;
        let row = sqlx::query(&format!("SELECT data FROM {p}_pool WHERE id = $1"))
            .bind(&id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(storage)?;
        let row = row.ok_or_else(|| CredentialError::PoolNotFound(id.0.clone()))?;
        let Json(pool): Json<CredentialPool> = row.try_get("data").map_err(storage)?;
        Ok(pool)
    }

    async fn list_pools(&self, workspace_id: &str) -> Result<Vec<CredentialPool>, CredentialError> {
        let p = NS;
        let rows = sqlx::query(&format!(
            "SELECT data FROM {p}_pool WHERE workspace_id = $1 ORDER BY id"
        ))
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        rows.into_iter()
            .map(|row| {
                let Json(pool): Json<CredentialPool> = row.try_get("data").map_err(storage)?;
                Ok(pool)
            })
            .collect()
    }
}

/// A Postgres-backed [`SealedBlobStore`]: opaque `nonce ‖ ciphertext` blobs in
/// `{prefix}_secret` (a `bytea` column), keyed by [`SecretRef`]. Not a
/// `SecretStore` — compose it under `SealedAeadSecretStore::over` so only sealed
/// bytes ever hit the database.
pub struct PostgresSealedBlobStore {
    pool: PgPool,
}

impl PostgresSealedBlobStore {
    /// Connect and apply the credential migrations under the `credential` namespace.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        Ok(Self {
            pool: connect_migrated(url).await?,
        })
    }

    /// Build from an existing pool: apply the credential migrations.
    pub async fn with_pool(pool: PgPool) -> Result<Self, StoreError> {
        Ok(Self {
            pool: pool_migrated(pool).await?,
        })
    }
}

#[async_trait::async_trait]
impl SealedBlobStore for PostgresSealedBlobStore {
    async fn put_blob(&self, r: &SecretRef, blob: Vec<u8>) -> Result<(), CredentialError> {
        let p = NS;
        sqlx::query(&format!(
            "INSERT INTO {p}_secret (secret_ref, sealed) VALUES ($1, $2) \
             ON CONFLICT (secret_ref) DO UPDATE SET sealed = excluded.sealed"
        ))
        .bind(&r.0)
        .bind(&blob)
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn get_blob(&self, r: &SecretRef) -> Result<Vec<u8>, CredentialError> {
        let p = NS;
        let row = sqlx::query(&format!(
            "SELECT sealed FROM {p}_secret WHERE secret_ref = $1"
        ))
        .bind(&r.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        let row = row.ok_or_else(|| CredentialError::SecretNotFound(r.0.clone()))?;
        let blob: Vec<u8> = row.try_get("sealed").map_err(storage)?;
        Ok(blob)
    }

    async fn delete_blob(&self, r: &SecretRef) -> Result<(), CredentialError> {
        let p = NS;
        sqlx::query(&format!("DELETE FROM {p}_secret WHERE secret_ref = $1"))
            .bind(&r.0)
            .execute(&self.pool)
            .await
            .map_err(storage)?;
        Ok(())
    }
}
