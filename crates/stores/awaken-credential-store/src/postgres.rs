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
    ManagedCredentialLifecycle, ManagedCredentialMutationError, ManagedVault,
    ManagedVaultCredential, ManagedVaultMutationError, ManagedVaultRepo,
    admit_managed_credential_insert, admit_managed_credential_replacement,
    admit_managed_vault_replacement,
};
use awaken_credential_vault::repo::{
    CredentialMutationIntent, CredentialRepo, ManagedCredentialMutationPhase,
    ManagedCredentialOperation, ManagedCredentialRepository, ManagedCredentialRollout,
    PendingManagedCredentialMutation, managed_retirement_parent_admitted,
    managed_rollout_from_committed,
};
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

fn invalid_managed_pending(error: ManagedCredentialMutationError) -> CredentialError {
    match error {
        ManagedCredentialMutationError::Store(error) => error,
        error => CredentialError::MutationConflict(format!(
            "invalid durable Managed credential mutation: {error}"
        )),
    }
}

/// A Postgres-backed [`CredentialRepo`] (secret-free rows only; the sealed
/// material goes through [`PostgresSealedBlobStore`]).
#[derive(Clone)]
pub struct PostgresCredentialRepo {
    pool: PgPool,
}

#[async_trait::async_trait]
impl ManagedVaultRepo for PostgresCredentialRepo {
    async fn insert_vault(
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
        let changed = sqlx::query(&format!("INSERT INTO {p}_managed_vault (id, workspace_id, data) VALUES ($1, $2, $3) ON CONFLICT (id) DO NOTHING"))
            .bind(&vault.id).bind(&vault.workspace_id).bind(Json(&vault)).execute(&mut *tx).await.map_err(storage)?;
        if changed.rows_affected() == 0 {
            return Err(CredentialError::MutationConflict(
                "Managed Vault id already exists".into(),
            ));
        }
        tx.commit().await.map_err(storage)?;
        Ok(())
    }

    async fn ensure_vault(
        &self,
        workspace_id: &str,
        vault: ManagedVault,
    ) -> Result<ManagedVault, CredentialError> {
        if vault.workspace_id != workspace_id {
            return Err(CredentialError::InvalidSource(
                "Managed Vault workspace does not match its authority".into(),
            ));
        }
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(storage)?;
        sqlx::query(&format!(
            "INSERT INTO {p}_managed_vault (id, workspace_id, data) VALUES ($1, $2, $3) \
             ON CONFLICT (id) DO NOTHING"
        ))
        .bind(&vault.id)
        .bind(&vault.workspace_id)
        .bind(Json(&vault))
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        let row = sqlx::query(&format!(
            "SELECT workspace_id, data FROM {p}_managed_vault WHERE id = $1 FOR UPDATE"
        ))
        .bind(&vault.id)
        .fetch_one(&mut *tx)
        .await
        .map_err(storage)?;
        let owner: String = row.try_get("workspace_id").map_err(storage)?;
        if owner != workspace_id {
            return Err(CredentialError::InvalidSource(
                "Managed Vault id belongs to another workspace".into(),
            ));
        }
        let Json(durable): Json<ManagedVault> = row.try_get("data").map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        Ok(durable)
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
impl ManagedCredentialRepository for PostgresCredentialRepo {
    async fn begin_managed_mutation(
        &self,
        pending: PendingManagedCredentialMutation,
    ) -> Result<(), CredentialError> {
        pending
            .validate_for_begin()
            .map_err(invalid_managed_pending)?;
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let current_source = sqlx::query(&format!(
            "SELECT data FROM {NS}_source WHERE id = $1 FOR UPDATE"
        ))
        .bind(&pending.after_source.id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .map(|row| {
            row.try_get::<Json<CredentialSource>, _>("data")
                .map(|Json(value)| value)
                .map_err(storage)
        })
        .transpose()?;
        let current_child = sqlx::query(&format!(
            "SELECT data FROM {NS}_managed_vault_credential WHERE id = $1 FOR UPDATE"
        ))
        .bind(&pending.after_credential.id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .map(|row| {
            row.try_get::<Json<ManagedVaultCredential>, _>("data")
                .map(|Json(value)| value)
                .map_err(storage)
        })
        .transpose()?;
        if current_source != pending.before_source || current_child != pending.before_credential {
            return Err(CredentialError::MutationConflict(
                "Managed credential changed before its mutation was prepared".into(),
            ));
        }
        sqlx::query(&format!(
            "INSERT INTO {NS}_managed_credential_mutation (source_id, data) VALUES ($1, $2) \
             ON CONFLICT (source_id) DO NOTHING"
        ))
        .bind(&pending.after_source.id.0)
        .bind(Json(&pending))
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        let row = sqlx::query(&format!(
            "SELECT data FROM {NS}_managed_credential_mutation WHERE source_id = $1"
        ))
        .bind(&pending.after_source.id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .ok_or_else(|| {
            CredentialError::MutationConflict(
                "Managed credential mutation lost its pending fact".into(),
            )
        })?;
        let Json(durable): Json<PendingManagedCredentialMutation> =
            row.try_get("data").map_err(storage)?;
        if durable != pending {
            return Err(CredentialError::MutationConflict(
                "another Managed credential mutation is pending".into(),
            ));
        }
        tx.commit().await.map_err(storage)?;
        Ok(())
    }

    async fn commit_managed_mutation(
        &self,
        pending: &PendingManagedCredentialMutation,
    ) -> Result<PendingManagedCredentialMutation, ManagedCredentialMutationError> {
        pending.validate()?;
        let p = NS;
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let durable = sqlx::query(&format!(
            "SELECT data FROM {p}_managed_credential_mutation WHERE source_id = $1 FOR UPDATE"
        ))
        .bind(&pending.after_source.id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .map(|row| {
            row.try_get::<Json<PendingManagedCredentialMutation>, _>("data")
                .map(|Json(value)| value)
                .map_err(storage)
        })
        .transpose()?
        .ok_or_else(|| {
            ManagedCredentialMutationError::Store(CredentialError::MutationConflict(
                "Managed credential mutation has no durable pending fact".into(),
            ))
        })?;
        if (durable != *pending || pending.phase != ManagedCredentialMutationPhase::Ready)
            && (durable.phase != ManagedCredentialMutationPhase::Reclaiming
                || durable.operation_id != pending.operation_id)
        {
            return Err(ManagedCredentialMutationError::Store(
                CredentialError::MutationConflict(
                    "Managed credential mutation is not ready or does not match its durable fact"
                        .into(),
                ),
            ));
        }

        // Lock the aggregate root before any child row. Every root mutation uses
        // this order, preventing a child->root/root->child deadlock. Selecting by
        // the globally unique Vault id also locks a foreign-workspace owner so a
        // colliding id is observed, never treated as an absent local root.
        let vault = sqlx::query(&format!(
            "SELECT data FROM {p}_managed_vault WHERE id = $1 FOR UPDATE"
        ))
        .bind(&pending.after_credential.vault_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .map(|row| {
            row.try_get::<Json<ManagedVault>, _>("data")
                .map(|Json(value)| value)
                .map_err(storage)
        })
        .transpose()?;
        let current_source = sqlx::query(&format!(
            "SELECT data FROM {p}_source WHERE id = $1 FOR UPDATE"
        ))
        .bind(&pending.after_source.id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .map(|row| {
            row.try_get::<Json<CredentialSource>, _>("data")
                .map(|Json(value)| value)
                .map_err(storage)
        })
        .transpose()?;
        let current_child = sqlx::query(&format!(
            "SELECT data FROM {p}_managed_vault_credential WHERE id = $1 FOR UPDATE"
        ))
        .bind(&pending.after_credential.id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .map(|row| {
            row.try_get::<Json<ManagedVaultCredential>, _>("data")
                .map(|Json(value)| value)
                .map_err(storage)
        })
        .transpose()?;
        if durable.phase == ManagedCredentialMutationPhase::Reclaiming {
            if durable.writer_token != pending.writer_token
                || durable.writer_epoch != pending.writer_epoch
                || current_source.as_ref() != Some(&pending.after_source)
                || current_child.as_ref() != Some(&pending.after_credential)
            {
                return Err(ManagedCredentialMutationError::RevisionConflict);
            }
            tx.commit().await.map_err(storage)?;
            return Ok(durable);
        }
        if current_source != pending.before_source || current_child != pending.before_credential {
            return Err(ManagedCredentialMutationError::RevisionConflict);
        }
        let existing = sqlx::query(&format!(
            "SELECT data FROM {p}_managed_vault_credential \
             WHERE workspace_id = $1 AND vault_id = $2 AND id <> $3 ORDER BY id"
        ))
        .bind(&pending.after_credential.workspace_id)
        .bind(&pending.after_credential.vault_id)
        .bind(&pending.after_credential.id)
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
        match pending.operation {
            ManagedCredentialOperation::Create => admit_managed_credential_insert(
                &pending.after_credential.workspace_id,
                vault.as_ref(),
                &existing,
                &pending.after_credential,
            )?,
            ManagedCredentialOperation::Update => {
                let before = pending
                    .before_credential
                    .as_ref()
                    .ok_or(ManagedCredentialMutationError::NotFound)?;
                admit_managed_credential_replacement(
                    &pending.after_credential.workspace_id,
                    Some(before),
                    before.revision,
                    &pending.after_credential,
                )?;
                let mut admission = pending.after_credential.clone();
                admission.revision = 1;
                admission.lifecycle = ManagedCredentialLifecycle::Active;
                admit_managed_credential_insert(
                    &pending.after_credential.workspace_id,
                    vault.as_ref(),
                    &existing,
                    &admission,
                )?;
            }
            ManagedCredentialOperation::Archive | ManagedCredentialOperation::Delete => {
                let before = pending
                    .before_credential
                    .as_ref()
                    .ok_or(ManagedCredentialMutationError::NotFound)?;
                admit_managed_credential_replacement(
                    &pending.after_credential.workspace_id,
                    Some(before),
                    before.revision,
                    &pending.after_credential,
                )?;
                if !managed_retirement_parent_admitted(
                    pending.operation,
                    vault.as_ref(),
                    &pending.after_credential,
                ) {
                    return Err(if vault.is_some() {
                        ManagedCredentialMutationError::InvalidLifecycle
                    } else {
                        ManagedCredentialMutationError::NotFound
                    });
                }
            }
        }
        let source_changed = if pending.operation == ManagedCredentialOperation::Create {
            sqlx::query(&format!(
                "INSERT INTO {p}_source (id, workspace_id, data) VALUES ($1, $2, $3) \
                 ON CONFLICT (id) DO NOTHING"
            ))
            .bind(&pending.after_source.id.0)
            .bind(&pending.after_source.workspace_id)
            .bind(Json(&pending.after_source))
            .execute(&mut *tx)
            .await
            .map_err(storage)?
            .rows_affected()
        } else {
            let before = pending
                .before_source
                .as_ref()
                .ok_or(ManagedCredentialMutationError::NotFound)?;
            sqlx::query(&format!(
                "UPDATE {p}_source SET workspace_id = $1, data = $2 \
                 WHERE id = $3 AND workspace_id = $4 AND data = $5"
            ))
            .bind(&pending.after_source.workspace_id)
            .bind(Json(&pending.after_source))
            .bind(&pending.after_source.id.0)
            .bind(&before.workspace_id)
            .bind(Json(before))
            .execute(&mut *tx)
            .await
            .map_err(storage)?
            .rows_affected()
        };
        if source_changed != 1 {
            return Err(ManagedCredentialMutationError::RevisionConflict);
        }

        let child_changed = if pending.operation == ManagedCredentialOperation::Create {
            sqlx::query(&format!(
                "INSERT INTO {p}_managed_vault_credential \
                 (id, vault_id, workspace_id, source_id, data) VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT DO NOTHING"
            ))
            .bind(&pending.after_credential.id)
            .bind(&pending.after_credential.vault_id)
            .bind(&pending.after_credential.workspace_id)
            .bind(&pending.after_credential.source_id.0)
            .bind(Json(&pending.after_credential))
            .execute(&mut *tx)
            .await
            .map_err(storage)?
            .rows_affected()
        } else {
            let before = pending
                .before_credential
                .as_ref()
                .ok_or(ManagedCredentialMutationError::NotFound)?;
            sqlx::query(&format!(
                "UPDATE {p}_managed_vault_credential SET vault_id = $1, workspace_id = $2, \
                 source_id = $3, data = $4 WHERE id = $5 AND vault_id = $6 \
                 AND workspace_id = $7 AND source_id = $8 AND data = $9"
            ))
            .bind(&pending.after_credential.vault_id)
            .bind(&pending.after_credential.workspace_id)
            .bind(&pending.after_credential.source_id.0)
            .bind(Json(&pending.after_credential))
            .bind(&pending.after_credential.id)
            .bind(&before.vault_id)
            .bind(&before.workspace_id)
            .bind(&before.source_id.0)
            .bind(Json(before))
            .execute(&mut *tx)
            .await
            .map_err(storage)?
            .rows_affected()
        };
        if child_changed != 1 {
            return Err(ManagedCredentialMutationError::RevisionConflict);
        }
        let mut reclaiming = pending.clone();
        reclaiming.phase = ManagedCredentialMutationPhase::Reclaiming;
        sqlx::query(&format!(
            "UPDATE {p}_managed_credential_mutation SET data = $1 WHERE source_id = $2"
        ))
        .bind(Json(&reclaiming))
        .bind(&pending.after_source.id.0)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        if let Some(rollout) = managed_rollout_from_committed(pending) {
            sqlx::query(&format!(
                "INSERT INTO {p}_managed_credential_rollout (event_id, data) VALUES ($1, $2) \
                 ON CONFLICT (event_id) DO NOTHING"
            ))
            .bind(&rollout.id)
            .bind(Json(&rollout))
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
            let durable = sqlx::query(&format!(
                "SELECT data FROM {p}_managed_credential_rollout WHERE event_id = $1 FOR UPDATE"
            ))
            .bind(&rollout.id)
            .fetch_one(&mut *tx)
            .await
            .map_err(storage)?
            .try_get::<Json<ManagedCredentialRollout>, _>("data")
            .map(|Json(value)| value)
            .map_err(storage)?;
            if durable != rollout {
                return Err(ManagedCredentialMutationError::RevisionConflict);
            }
        }
        tx.commit().await.map_err(storage)?;
        Ok(reclaiming)
    }

    async fn mark_managed_mutation_ready(
        &self,
        pending: &PendingManagedCredentialMutation,
    ) -> Result<PendingManagedCredentialMutation, CredentialError> {
        pending.validate().map_err(invalid_managed_pending)?;
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let row = sqlx::query(&format!(
            "SELECT data FROM {NS}_managed_credential_mutation WHERE source_id = $1 FOR UPDATE"
        ))
        .bind(&pending.after_source.id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .ok_or_else(|| {
            CredentialError::MutationConflict(
                "Managed credential mutation has no durable pending fact".into(),
            )
        })?;
        let Json(mut durable): Json<PendingManagedCredentialMutation> =
            row.try_get("data").map_err(storage)?;
        if durable != *pending || pending.phase != ManagedCredentialMutationPhase::Writing {
            return Err(CredentialError::MutationConflict(
                "Managed credential ready transition does not match Writing".into(),
            ));
        }
        durable.phase = ManagedCredentialMutationPhase::Ready;
        sqlx::query(&format!(
            "UPDATE {NS}_managed_credential_mutation SET data = $1 WHERE source_id = $2"
        ))
        .bind(Json(&durable))
        .bind(&pending.after_source.id.0)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        Ok(durable)
    }

    async fn pending_managed_mutations(
        &self,
    ) -> Result<Vec<PendingManagedCredentialMutation>, CredentialError> {
        let rows = sqlx::query(&format!(
            "SELECT source_id, data::text AS data FROM {NS}_managed_credential_mutation \
             ORDER BY created_at, source_id"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        let mut pending = Vec::new();
        for row in rows {
            let source_id = row.try_get::<String, _>("source_id").map_err(storage)?;
            let data = row.try_get::<String, _>("data").map_err(storage)?;
            match serde_json::from_str::<PendingManagedCredentialMutation>(&data) {
                Ok(value) => match value.validate() {
                    Ok(()) => pending.push(value),
                    Err(error) => tracing::error!(
                        recovery_record = "managed_credential_mutation",
                        record_id = %source_id,
                        error = %error,
                        "retaining and isolating an invalid durable recovery record"
                    ),
                },
                Err(error) => tracing::error!(
                    recovery_record = "managed_credential_mutation",
                    record_id = %source_id,
                    error = %error,
                    "retaining and isolating an undecodable durable recovery record"
                ),
            }
        }
        Ok(pending)
    }

    async fn claim_expired_managed_mutation(
        &self,
        pending: &PendingManagedCredentialMutation,
        now_unix_ms: u64,
        lease_expires_at_unix_ms: u64,
    ) -> Result<Option<PendingManagedCredentialMutation>, CredentialError> {
        let Some(claimed) = pending.claim_after_expiry(now_unix_ms, lease_expires_at_unix_ms)?
        else {
            return Ok(None);
        };
        claimed.validate().map_err(invalid_managed_pending)?;

        let mut tx = self.pool.begin().await.map_err(storage)?;
        let durable = sqlx::query(&format!(
            "SELECT data FROM {NS}_managed_credential_mutation \
             WHERE source_id = $1 FOR UPDATE"
        ))
        .bind(&pending.after_source.id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .map(|row| {
            row.try_get::<Json<PendingManagedCredentialMutation>, _>("data")
                .map(|Json(value)| value)
                .map_err(storage)
        })
        .transpose()?;
        if durable.as_ref() != Some(pending)
            || durable
                .as_ref()
                .is_some_and(|value| value.phase != ManagedCredentialMutationPhase::Writing)
        {
            tx.commit().await.map_err(storage)?;
            return Ok(None);
        }
        let changed = sqlx::query(&format!(
            "UPDATE {NS}_managed_credential_mutation SET data = $1 \
             WHERE source_id = $2 AND data = $3"
        ))
        .bind(Json(&claimed))
        .bind(&pending.after_source.id.0)
        .bind(Json(pending))
        .execute(&mut *tx)
        .await
        .map_err(storage)?
        .rows_affected();
        if changed != 1 {
            tx.rollback().await.map_err(storage)?;
            return Ok(None);
        }
        tx.commit().await.map_err(storage)?;
        Ok(Some(claimed))
    }

    async fn abort_managed_mutation(
        &self,
        pending: &PendingManagedCredentialMutation,
    ) -> Result<PendingManagedCredentialMutation, CredentialError> {
        pending.validate().map_err(invalid_managed_pending)?;
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let durable = sqlx::query(&format!(
            "SELECT data FROM {NS}_managed_credential_mutation WHERE source_id = $1 FOR UPDATE"
        ))
        .bind(&pending.after_source.id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .map(|row| {
            row.try_get::<Json<PendingManagedCredentialMutation>, _>("data")
                .map(|Json(value)| value)
                .map_err(storage)
        })
        .transpose()?;
        let durable = durable.ok_or_else(|| {
            CredentialError::MutationConflict(
                "Managed credential abort has no durable pending fact".into(),
            )
        })?;
        let idempotent_reclaim =
            durable == *pending && pending.phase == ManagedCredentialMutationPhase::ReclaimingAbort;
        if !idempotent_reclaim
            && (durable != *pending
                || !matches!(
                    pending.phase,
                    ManagedCredentialMutationPhase::Writing | ManagedCredentialMutationPhase::Ready
                ))
        {
            return Err(CredentialError::MutationConflict(
                "Managed credential abort does not match its durable pending fact".into(),
            ));
        }
        let current_source = sqlx::query(&format!(
            "SELECT data FROM {NS}_source WHERE id = $1 FOR UPDATE"
        ))
        .bind(&pending.after_source.id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .map(|row| {
            row.try_get::<Json<CredentialSource>, _>("data")
                .map(|Json(v)| v)
                .map_err(storage)
        })
        .transpose()?;
        let current_child = sqlx::query(&format!(
            "SELECT data FROM {NS}_managed_vault_credential WHERE id = $1 FOR UPDATE"
        ))
        .bind(&pending.after_credential.id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .map(|row| {
            row.try_get::<Json<ManagedVaultCredential>, _>("data")
                .map(|Json(v)| v)
                .map_err(storage)
        })
        .transpose()?;
        if current_source != pending.before_source || current_child != pending.before_credential {
            return Err(CredentialError::MutationConflict(
                "cannot abort a published or superseded Managed credential mutation".into(),
            ));
        }
        if idempotent_reclaim {
            tx.commit().await.map_err(storage)?;
            return Ok(durable);
        }
        let mut reclaiming = pending.clone();
        reclaiming.phase = ManagedCredentialMutationPhase::ReclaimingAbort;
        let changed = sqlx::query(&format!(
            "UPDATE {NS}_managed_credential_mutation SET data = $1 \
             WHERE source_id = $2 AND data = $3"
        ))
        .bind(Json(&reclaiming))
        .bind(&pending.after_source.id.0)
        .bind(Json(pending))
        .execute(&mut *tx)
        .await
        .map_err(storage)?
        .rows_affected();
        if changed != 1 {
            return Err(CredentialError::MutationConflict(
                "Managed credential abort lost its exact durable pending fact".into(),
            ));
        }
        tx.commit().await.map_err(storage)?;
        Ok(reclaiming)
    }

    async fn complete_managed_mutation(
        &self,
        pending: &PendingManagedCredentialMutation,
    ) -> Result<(), CredentialError> {
        pending.validate().map_err(invalid_managed_pending)?;
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let durable = sqlx::query(&format!(
            "SELECT data FROM {NS}_managed_credential_mutation WHERE source_id = $1 FOR UPDATE"
        ))
        .bind(&pending.after_source.id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .map(|row| {
            row.try_get::<Json<PendingManagedCredentialMutation>, _>("data")
                .map(|Json(value)| value)
                .map_err(storage)
        })
        .transpose()?;
        let Some(durable) = durable else {
            tx.commit().await.map_err(storage)?;
            return Ok(());
        };
        let current_source = sqlx::query(&format!(
            "SELECT data FROM {NS}_source WHERE id = $1 FOR UPDATE"
        ))
        .bind(&pending.after_source.id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .map(|row| {
            row.try_get::<Json<CredentialSource>, _>("data")
                .map(|Json(v)| v)
                .map_err(storage)
        })
        .transpose()?;
        let current_child = sqlx::query(&format!(
            "SELECT data FROM {NS}_managed_vault_credential WHERE id = $1 FOR UPDATE"
        ))
        .bind(&pending.after_credential.id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .map(|row| {
            row.try_get::<Json<ManagedVaultCredential>, _>("data")
                .map(|Json(v)| v)
                .map_err(storage)
        })
        .transpose()?;
        let truth_matches_phase = match pending.phase {
            ManagedCredentialMutationPhase::Reclaiming => {
                current_source.as_ref() == Some(&pending.after_source)
                    && current_child.as_ref() == Some(&pending.after_credential)
            }
            ManagedCredentialMutationPhase::ReclaimingAbort => {
                current_source == pending.before_source
                    && current_child == pending.before_credential
            }
            ManagedCredentialMutationPhase::Writing | ManagedCredentialMutationPhase::Ready => {
                false
            }
        };
        if durable != *pending || !truth_matches_phase {
            return Err(CredentialError::MutationConflict(
                "Managed credential completion does not match its durable cleanup truth".into(),
            ));
        }
        let deleted = sqlx::query(&format!(
            "DELETE FROM {NS}_managed_credential_mutation WHERE source_id = $1 AND data = $2"
        ))
        .bind(&pending.after_source.id.0)
        .bind(Json(pending))
        .execute(&mut *tx)
        .await
        .map_err(storage)?
        .rows_affected();
        if deleted != 1 {
            return Err(CredentialError::MutationConflict(
                "Managed credential completion lost its exact durable cleanup fact".into(),
            ));
        }
        tx.commit().await.map_err(storage)
    }

    async fn managed_rollout(
        &self,
        event_id: &str,
    ) -> Result<Option<ManagedCredentialRollout>, CredentialError> {
        sqlx::query(&format!(
            "SELECT data FROM {NS}_managed_credential_rollout WHERE event_id = $1"
        ))
        .bind(event_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?
        .map(|row| {
            row.try_get::<Json<ManagedCredentialRollout>, _>("data")
                .map(|Json(value)| value)
                .map_err(storage)
        })
        .transpose()
    }

    async fn pending_managed_rollouts(
        &self,
    ) -> Result<Vec<ManagedCredentialRollout>, CredentialError> {
        let rows = sqlx::query(&format!(
            "SELECT event_id, data::text AS data FROM {NS}_managed_credential_rollout \
             ORDER BY created_at, event_id"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        let mut rollouts = Vec::new();
        for row in rows {
            let event_id = row.try_get::<String, _>("event_id").map_err(storage)?;
            let data = row.try_get::<String, _>("data").map_err(storage)?;
            match serde_json::from_str(&data) {
                Ok(value) => rollouts.push(value),
                Err(error) => tracing::error!(
                    recovery_record = "managed_credential_rollout",
                    record_id = %event_id,
                    error = %error,
                    "retaining and isolating an undecodable durable recovery record"
                ),
            }
        }
        Ok(rollouts)
    }

    async fn complete_managed_rollout(
        &self,
        rollout: &ManagedCredentialRollout,
    ) -> Result<(), CredentialError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let durable = sqlx::query(&format!(
            "SELECT data FROM {NS}_managed_credential_rollout WHERE event_id = $1 FOR UPDATE"
        ))
        .bind(&rollout.id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        .map(|row| {
            row.try_get::<Json<ManagedCredentialRollout>, _>("data")
                .map(|Json(value)| value)
                .map_err(storage)
        })
        .transpose()?;
        let Some(durable) = durable else {
            tx.commit().await.map_err(storage)?;
            return Ok(());
        };
        if durable != *rollout {
            return Err(CredentialError::MutationConflict(
                "Managed credential rollout acknowledgement is stale".into(),
            ));
        }
        sqlx::query(&format!(
            "DELETE FROM {NS}_managed_credential_rollout WHERE event_id = $1"
        ))
        .bind(&rollout.id)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        tx.commit().await.map_err(storage)
    }

    async fn pending_managed_vault_deletions(&self) -> Result<Vec<ManagedVault>, CredentialError> {
        let rows = sqlx::query(&format!(
            "SELECT data FROM {NS}_managed_vault ORDER BY workspace_id, id"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        rows.into_iter()
            .map(|row| {
                row.try_get::<Json<ManagedVault>, _>("data")
                    .map(|Json(value)| value)
                    .map_err(storage)
            })
            .filter_map(|vault| match vault {
                Ok(vault) if vault.deletion_requested() => Some(Ok(vault)),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
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
    ) -> Result<bool, CredentialError> {
        let inserted = sqlx::query(&format!(
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
        Ok(inserted.rows_affected() == 1)
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
        let changed = if let Some(before) = intent.before.as_ref() {
            sqlx::query(&format!(
                "UPDATE {NS}_source SET workspace_id = $1, data = $2 \
                 WHERE id = $3 AND workspace_id = $4 AND data = $5"
            ))
            .bind(&intent.after.workspace_id)
            .bind(Json(&intent.after))
            .bind(&intent.after.id.0)
            .bind(&before.workspace_id)
            .bind(Json(before))
            .execute(&mut *tx)
            .await
            .map_err(storage)?
            .rows_affected()
        } else {
            sqlx::query(&format!(
                "INSERT INTO {NS}_source (id, workspace_id, data) VALUES ($1, $2, $3) \
                 ON CONFLICT (id) DO NOTHING"
            ))
            .bind(&intent.after.id.0)
            .bind(&intent.after.workspace_id)
            .bind(Json(&intent.after))
            .execute(&mut *tx)
            .await
            .map_err(storage)?
            .rows_affected()
        };
        if changed != 1 {
            return Err(CredentialError::MutationConflict(
                "credential revision changed during mutation".into(),
            ));
        }
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
