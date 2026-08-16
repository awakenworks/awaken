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

use crate::schema::credential_bundle;
use awaken_credential_contract::CredentialSourceId;
use awaken_credential_vault::catalog::{
    ManagedCredentialAdmissionError, ManagedVault, ManagedVaultCredential,
    ManagedVaultMutationError, ManagedVaultRepo, admit_managed_credential_insert,
    admit_managed_vault_replacement,
};
use awaken_credential_vault::repo::{CredentialMutationIntent, CredentialRepo};
use awaken_credential_vault::{
    CredentialError, CredentialPool, CredentialPoolId, CredentialSource, SealedBlobStore, SecretRef,
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
    #[error("schema: {0}")]
    Schema(String),
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

async fn connect_verified(url: &str) -> Result<PgPool, StoreError> {
    let pool = PgPool::connect(url)
        .await
        .map_err(|err| StoreError::Connect(err.to_string()))?;
    let bundle = credential_bundle().map_err(|err| StoreError::Schema(err.to_string()))?;
    awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
        .map_err(|err| StoreError::Schema(err.to_string()))?
        .verify_bundle(&bundle)
        .await
        .map_err(|err| StoreError::Schema(err.to_string()))?;
    Ok(pool)
}

/// Open the two credential adapters over one verified pool. This is the
/// canonical application startup path; it avoids checking or migrating the
/// same credential scope once per adapter.
pub async fn connect_existing_pair(
    url: &str,
) -> Result<(PostgresCredentialRepo, PostgresSealedBlobStore), StoreError> {
    let pool = connect_verified(url).await?;
    Ok((
        PostgresCredentialRepo { pool: pool.clone() },
        PostgresSealedBlobStore { pool },
    ))
}

/// Apply the credential bundle once and construct both adapters over the same
/// pool. Operational migration composition uses this instead of independently
/// opening the row and blob adapters.
pub async fn connect_migrated_pair(
    url: &str,
) -> Result<(PostgresCredentialRepo, PostgresSealedBlobStore), StoreError> {
    let pool = connect_migrated(url).await?;
    Ok((
        PostgresCredentialRepo { pool: pool.clone() },
        PostgresSealedBlobStore { pool },
    ))
}

fn storage(err: impl std::fmt::Display) -> CredentialError {
    CredentialError::Storage(err.to_string())
}

/// A Postgres-backed [`CredentialRepo`] (secret-free rows only; the sealed
/// material goes through [`PostgresSealedBlobStore`]).
#[derive(Clone)]
pub struct PostgresCredentialRepo {
    pool: PgPool,
}

#[async_trait::async_trait]
impl ManagedVaultRepo for PostgresCredentialRepo {
    async fn put_vault(
        &self,
        workspace_id: &str,
        vault: ManagedVault,
    ) -> Result<(), CredentialError> {
        if vault.workspace_id != workspace_id {
            return Err(CredentialError::InvalidSource(
                "Managed Vault workspace does not match its authority".into(),
            ));
        }
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let existing_owner = sqlx::query_scalar::<_, String>(&format!(
            "SELECT workspace_id FROM {p}_managed_vault WHERE id = $1 FOR UPDATE"
        ))
        .bind(&vault.id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        if existing_owner
            .as_deref()
            .is_some_and(|owner| owner != workspace_id)
        {
            return Err(CredentialError::InvalidSource(
                "Managed Vault id belongs to another workspace".into(),
            ));
        }
        let changed = sqlx::query(&format!("INSERT INTO {p}_managed_vault (id, workspace_id, data) VALUES ($1, $2, $3) ON CONFLICT (id) DO UPDATE SET data = excluded.data WHERE {p}_managed_vault.workspace_id = excluded.workspace_id"))
            .bind(&vault.id).bind(&vault.workspace_id).bind(Json(&vault)).execute(&mut *tx).await.map_err(storage)?;
        if changed.rows_affected() == 0 {
            return Err(CredentialError::InvalidSource(
                "Managed Vault id belongs to another workspace".into(),
            ));
        }
        tx.commit().await.map_err(storage)?;
        Ok(())
    }

    async fn replace_vault(
        &self,
        workspace_id: &str,
        expected_revision: u64,
        vault: ManagedVault,
    ) -> Result<(), ManagedVaultMutationError> {
        let p = NS;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| ManagedVaultMutationError::Store(storage(error)))?;
        let current = sqlx::query(&format!(
            "SELECT data FROM {p}_managed_vault WHERE workspace_id = $1 AND id = $2 FOR UPDATE"
        ))
        .bind(workspace_id)
        .bind(&vault.id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| ManagedVaultMutationError::Store(storage(error)))?
        .map(|row| {
            row.try_get::<Json<ManagedVault>, _>("data")
                .map(|Json(value)| value)
                .map_err(|error| ManagedVaultMutationError::Store(storage(error)))
        })
        .transpose()?;
        admit_managed_vault_replacement(workspace_id, current.as_ref(), expected_revision, &vault)?;
        sqlx::query(&format!(
            "UPDATE {p}_managed_vault SET data = $1 WHERE workspace_id = $2 AND id = $3"
        ))
        .bind(Json(&vault))
        .bind(workspace_id)
        .bind(&vault.id)
        .execute(&mut *tx)
        .await
        .map_err(|error| ManagedVaultMutationError::Store(storage(error)))?;
        tx.commit()
            .await
            .map_err(|error| ManagedVaultMutationError::Store(storage(error)))?;
        Ok(())
    }

    async fn get_vault(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Option<ManagedVault>, CredentialError> {
        let p = NS;
        let row = sqlx::query(&format!(
            "SELECT data FROM {p}_managed_vault WHERE workspace_id = $1 AND id = $2"
        ))
        .bind(workspace_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        row.map(|row| {
            row.try_get::<Json<ManagedVault>, _>("data")
                .map(|Json(value)| value)
                .map_err(storage)
        })
        .transpose()
    }

    async fn list_vaults(&self, workspace_id: &str) -> Result<Vec<ManagedVault>, CredentialError> {
        let p = NS;
        let rows = sqlx::query(&format!(
            "SELECT data FROM {p}_managed_vault WHERE workspace_id = $1 ORDER BY id"
        ))
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        rows.into_iter()
            .map(|row| {
                row.try_get::<Json<ManagedVault>, _>("data")
                    .map(|Json(value)| value)
                    .map_err(storage)
            })
            .collect()
    }

    async fn delete_vault(&self, workspace_id: &str, id: &str) -> Result<bool, CredentialError> {
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let parent_exists = sqlx::query_scalar::<_, i32>(&format!(
            "SELECT 1 FROM {p}_managed_vault WHERE workspace_id = $1 AND id = $2 FOR UPDATE"
        ))
        .bind(workspace_id)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .is_some();
        if !parent_exists {
            return Ok(false);
        }
        sqlx::query(&format!(
            "DELETE FROM {p}_managed_vault_credential WHERE workspace_id = $1 AND vault_id = $2"
        ))
        .bind(workspace_id)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        let removed = sqlx::query(&format!(
            "DELETE FROM {p}_managed_vault WHERE workspace_id = $1 AND id = $2"
        ))
        .bind(workspace_id)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(storage)?
        .rows_affected()
            > 0;
        tx.commit().await.map_err(storage)?;
        Ok(removed)
    }

    async fn put_vault_credential(
        &self,
        workspace_id: &str,
        credential: ManagedVaultCredential,
    ) -> Result<(), CredentialError> {
        if credential.workspace_id != workspace_id {
            return Err(CredentialError::InvalidSource(
                "Managed credential workspace does not match its authority".into(),
            ));
        }
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(storage)?;
        // Pair this shared parent lock with delete_vault's FOR UPDATE lock. The
        // parent check and child upsert then form one atomic admission step, so
        // a concurrent delete cannot leave an orphaned credential row.
        let parent_exists = sqlx::query_scalar::<_, i32>(&format!(
            "SELECT 1 FROM {p}_managed_vault WHERE workspace_id = $1 AND id = $2 FOR KEY SHARE"
        ))
        .bind(workspace_id)
        .bind(&credential.vault_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .is_some();
        if !parent_exists {
            return Err(CredentialError::InvalidSource(
                "Managed credential parent Vault is unavailable in this workspace".into(),
            ));
        }
        let changed = sqlx::query(&format!("INSERT INTO {p}_managed_vault_credential (id, vault_id, workspace_id, source_id, data) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (id) DO UPDATE SET vault_id = excluded.vault_id, source_id = excluded.source_id, data = excluded.data WHERE {p}_managed_vault_credential.workspace_id = excluded.workspace_id"))
            .bind(&credential.id).bind(&credential.vault_id).bind(&credential.workspace_id).bind(&credential.source_id.0).bind(Json(&credential)).execute(&mut *tx).await.map_err(storage)?;
        if changed.rows_affected() == 0 {
            return Err(CredentialError::InvalidSource(
                "Managed credential id belongs to another workspace".into(),
            ));
        }
        tx.commit().await.map_err(storage)?;
        Ok(())
    }

    async fn insert_vault_credential(
        &self,
        workspace_id: &str,
        credential: ManagedVaultCredential,
    ) -> Result<(), ManagedCredentialAdmissionError> {
        if credential.workspace_id != workspace_id {
            return Err(ManagedCredentialAdmissionError::WorkspaceMismatch);
        }
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(storage)?;
        // Every child insertion takes the parent write lock. Archive, update and
        // delete use the same row lock, making aggregate admission serializable
        // across replicas.
        let vault = sqlx::query(&format!(
            "SELECT data FROM {p}_managed_vault WHERE workspace_id = $1 AND id = $2 FOR UPDATE"
        ))
        .bind(workspace_id)
        .bind(&credential.vault_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .map(|row| {
            row.try_get::<Json<ManagedVault>, _>("data")
                .map(|Json(value)| value)
                .map_err(storage)
        })
        .transpose()?;
        let existing = sqlx::query(&format!(
            "SELECT data FROM {p}_managed_vault_credential WHERE workspace_id = $1 AND vault_id = $2 ORDER BY id"
        ))
        .bind(workspace_id)
        .bind(&credential.vault_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?
        .into_iter()
        .map(|row| {
            row.try_get::<Json<ManagedVaultCredential>, _>("data")
                .map(|Json(value)| value)
                .map_err(storage)
        })
        .collect::<Result<Vec<_>, _>>()?;
        admit_managed_credential_insert(workspace_id, vault.as_ref(), &existing, &credential)?;
        let inserted = sqlx::query(&format!(
            "INSERT INTO {p}_managed_vault_credential (id, vault_id, workspace_id, source_id, data) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (id) DO NOTHING"
        ))
        .bind(&credential.id)
        .bind(&credential.vault_id)
        .bind(&credential.workspace_id)
        .bind(&credential.source_id.0)
        .bind(Json(&credential))
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        if inserted.rows_affected() != 1 {
            return Err(ManagedCredentialAdmissionError::Store(
                CredentialError::InvalidSource("Managed credential id already exists".into()),
            ));
        }
        tx.commit().await.map_err(storage)?;
        Ok(())
    }

    async fn get_vault_credential(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Option<ManagedVaultCredential>, CredentialError> {
        let p = NS;
        let row = sqlx::query(&format!(
            "SELECT data FROM {p}_managed_vault_credential WHERE workspace_id = $1 AND id = $2"
        ))
        .bind(workspace_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        row.map(|row| {
            row.try_get::<Json<ManagedVaultCredential>, _>("data")
                .map(|Json(value)| value)
                .map_err(storage)
        })
        .transpose()
    }

    async fn get_vault_credential_by_source(
        &self,
        workspace_id: &str,
        source_id: &CredentialSourceId,
    ) -> Result<Option<ManagedVaultCredential>, CredentialError> {
        let p = NS;
        let row = sqlx::query(&format!(
            "SELECT data FROM {p}_managed_vault_credential WHERE workspace_id = $1 AND source_id = $2"
        ))
        .bind(workspace_id)
        .bind(&source_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        row.map(|row| {
            row.try_get::<Json<ManagedVaultCredential>, _>("data")
                .map(|Json(value)| value)
                .map_err(storage)
        })
        .transpose()
    }

    async fn list_vault_credentials(
        &self,
        workspace_id: &str,
        vault_id: &str,
    ) -> Result<Vec<ManagedVaultCredential>, CredentialError> {
        let p = NS;
        let rows = sqlx::query(&format!(
            "SELECT data FROM {p}_managed_vault_credential WHERE workspace_id = $1 AND vault_id = $2 ORDER BY id"
        ))
        .bind(workspace_id)
        .bind(vault_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        rows.into_iter()
            .map(|row| {
                row.try_get::<Json<ManagedVaultCredential>, _>("data")
                    .map(|Json(value)| value)
                    .map_err(storage)
            })
            .collect()
    }

    async fn delete_vault_credential(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<bool, CredentialError> {
        let p = NS;
        Ok(sqlx::query(&format!(
            "DELETE FROM {p}_managed_vault_credential WHERE workspace_id = $1 AND id = $2"
        ))
        .bind(workspace_id)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(storage)?
        .rows_affected()
            > 0)
    }
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

    async fn put_if_absent(
        &self,
        source: CredentialSource,
    ) -> Result<CredentialSource, CredentialError> {
        let p = NS;
        sqlx::query(&format!(
            "INSERT INTO {p}_source (id, workspace_id, data) VALUES ($1, $2, $3) \
             ON CONFLICT (id) DO NOTHING"
        ))
        .bind(&source.id.0)
        .bind(&source.workspace_id)
        .bind(Json(&source))
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        self.get(&source.id).await
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

    async fn begin_mutation(
        &self,
        intent: CredentialMutationIntent,
    ) -> Result<(), CredentialError> {
        sqlx::query(&format!(
            "INSERT INTO {NS}_creation_intent (source_id, data) VALUES ($1, $2) \
             ON CONFLICT (source_id) DO NOTHING"
        ))
        .bind(&intent.after.id.0)
        .bind(Json(&intent))
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        let row = sqlx::query(&format!(
            "SELECT data FROM {NS}_creation_intent WHERE source_id = $1"
        ))
        .bind(&intent.after.id.0)
        .fetch_one(&self.pool)
        .await
        .map_err(storage)?;
        let Json(durable): Json<CredentialMutationIntent> = row.try_get("data").map_err(storage)?;
        if durable != intent {
            return Err(CredentialError::MutationConflict(
                "another credential mutation is pending".into(),
            ));
        }
        Ok(())
    }

    async fn apply_mutation(
        &self,
        intent: &CredentialMutationIntent,
    ) -> Result<(), CredentialError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let row = sqlx::query(&format!(
            "SELECT data FROM {NS}_creation_intent WHERE source_id = $1 FOR UPDATE"
        ))
        .bind(&intent.after.id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .ok_or_else(|| {
            CredentialError::MutationConflict(
                "credential mutation has no matching durable intent".into(),
            )
        })?;
        let Json(durable): Json<CredentialMutationIntent> = row.try_get("data").map_err(storage)?;
        if durable != *intent {
            return Err(CredentialError::MutationConflict(
                "credential mutation does not match durable intent".into(),
            ));
        }
        let current = sqlx::query(&format!(
            "SELECT data FROM {NS}_source WHERE id = $1 FOR UPDATE"
        ))
        .bind(&intent.after.id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .map(|row| {
            let Json(source): Json<CredentialSource> = row.try_get("data").map_err(storage)?;
            Ok(source)
        })
        .transpose()?;
        if current.as_ref() == Some(&intent.after) {
            return tx.commit().await.map_err(storage);
        }
        if current != intent.before {
            return Err(CredentialError::MutationConflict(
                "credential revision changed during mutation".into(),
            ));
        }
        sqlx::query(&format!(
            "INSERT INTO {NS}_source (id, workspace_id, data) VALUES ($1, $2, $3) \
             ON CONFLICT (id) DO UPDATE SET workspace_id = excluded.workspace_id, data = excluded.data"
        ))
        .bind(&intent.after.id.0)
        .bind(&intent.after.workspace_id)
        .bind(Json(&intent.after))
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        tx.commit().await.map_err(storage)
    }

    async fn pending_mutations(&self) -> Result<Vec<CredentialMutationIntent>, CredentialError> {
        sqlx::query(&format!(
            "SELECT data FROM {NS}_creation_intent ORDER BY created_at, source_id"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?
        .into_iter()
        .map(|row| {
            let Json(intent): Json<CredentialMutationIntent> =
                row.try_get("data").map_err(storage)?;
            Ok(intent)
        })
        .collect()
    }

    async fn complete_mutation(&self, id: &CredentialSourceId) -> Result<(), CredentialError> {
        sqlx::query(&format!(
            "DELETE FROM {NS}_creation_intent WHERE source_id = $1"
        ))
        .bind(&id.0)
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn material_refs(&self) -> Result<Vec<SecretRef>, CredentialError> {
        let rows = sqlx::query(&format!("SELECT data FROM {NS}_source ORDER BY id"))
            .fetch_all(&self.pool)
            .await
            .map_err(storage)?;
        rows.into_iter()
            .map(|row| {
                let Json(source): Json<CredentialSource> = row.try_get("data").map_err(storage)?;
                Ok(source
                    .material_ref
                    .into_iter()
                    .chain(source.auxiliary_material_refs.into_values())
                    .collect::<Vec<_>>())
            })
            .collect::<Result<Vec<_>, CredentialError>>()
            .map(|items| items.into_iter().flatten().collect())
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

    async fn inventory_blobs(&self) -> Result<Vec<SecretRef>, CredentialError> {
        sqlx::query_scalar::<_, String>(&format!(
            "SELECT secret_ref FROM {NS}_secret ORDER BY secret_ref"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(storage)
        .map(|keys| keys.into_iter().map(SecretRef).collect())
    }
}
