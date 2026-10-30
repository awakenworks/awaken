// @formal entrypoint credential.managed_abort_cleanup_remains_durable_until_unpublished_material_is_removed
// @formal entrypoint credential.managed_creation_begin_rejects_every_published_source_or_child_identity
// @formal entrypoint credential.managed_creation_publishes_source_and_child_atomically
// @formal entrypoint credential.managed_mutation_requires_exact_source_and_child_successor_fences
// @formal entrypoint credential.managed_rollout_event_id_replay_requires_exact_payload_and_colliding_ack_is_rejected
// @formal entrypoint credential.managed_rollout_outbox_is_atomic_with_exact_pair_publication
// @formal entrypoint credential.managed_vault_child_admission_requires_active_parent_capacity_and_unique_key
// @formal entrypoint credential.managed_vault_delete_fences_children_waits_for_rollout_and_is_absorbing
// @formal entrypoint credential.managed_vault_replacement_rejects_stale_and_non_successor_revisions
// @formal entrypoint credential.material_is_always_published_or_intent_tracked
// @formal entrypoint credential.rotation_revision_cas
// @formal entrypoint credential.vault_workspace_authority_admission_is_exact_and_non_widening
// @formal evidence credential.managed_creation_expired_owner_takeover_fences_abstract_transitions_and_conditionally_converges
//! SQLite adapters (feature `sqlite`, ADR-0043 sqlite-repos) for the credential
//! domain, over the crate's own `credential` migration scope
//! ([`credential_bundle`]): [`SqliteCredentialRepo`] persists the **secret-free**
//! source/pool rows (serde in the `data {json}` column, keyed columns for
//! lookups), and [`SqliteSealedBlobStore`] persists opaque sealed blobs in
//! `{prefix}_secret`.
//!
//! Deliberately **no bare SQLite [`SecretStore`](crate::SecretStore)** is
//! exposed: it would write plaintext at rest — a footgun this module refuses to
//! offer. The one durable secret path is the AEAD decorator over the blob port:
//! `SealedAeadSecretStore::over(&key, Arc::new(SqliteSealedBlobStore::open(..).await?))`
//! (features `sealed-aead` + `sqlite`), which stores only `nonce ‖ ciphertext`.
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation
    )
)]

use awaken_scoped_migration::sqlite::SqliteMigrationRunner;
use awaken_sqlite_pool::EmbeddedPool;
use awaken_sqlite_pool::EmbeddedSqlite;
use sqlx::SqliteConnection;

use crate::schema::credential_bundle;
use awaken_credential_contract::CredentialSourceId;
use awaken_credential_vault::catalog::{
    ManagedCredentialLifecycle, ManagedCredentialMutationError, ManagedVault,
    ManagedVaultCredential, ManagedVaultMutationError, ManagedVaultRepo,
    admit_managed_credential_insert, admit_managed_credential_replacement,
    admit_managed_vault_replacement, managed_vault_workspace_admitted,
};
use awaken_credential_vault::repo::{
    CredentialMaterialMutationPhase, CredentialMutationCommitMode, CredentialMutationIntent,
    CredentialRepo, ManagedCredentialOperation, ManagedCredentialRepository,
    ManagedCredentialRollout, ManagedCredentialRolloutIdentityDecision,
    PendingManagedCredentialMutation, managed_credential_commit_plan,
    managed_credential_rollout_identity_decision, managed_retirement_parent_admitted,
};
use awaken_credential_vault::{
    CredentialError, CredentialPool, CredentialPoolId, CredentialSource, SealedBlobStore, SecretRef,
};

/// The credential component's table namespace (its bundle prefix).
const NS: &str = "credential";
/// Errors from opening or migrating the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("open: {0}")]
    Open(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

/// Apply the shared `credential` scoped bundle to a pool. Both stores key the
/// same scope, so this is the migration half they share; the wrap-without-migrate
/// half is each store's `over`.
async fn run_migrations(pool: &EmbeddedPool) -> Result<(), StoreError> {
    let bundle = credential_bundle().map_err(|err| StoreError::Migrate(err.to_string()))?;
    awaken_sqlite_pool::held(
        SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), NS)
            .map_err(|err| StoreError::Migrate(err.to_string()))?
            .run_bundle(&bundle),
    )
    .await
    .map_err(|err| StoreError::Migrate(err.to_string()))?;
    Ok(())
}

fn storage(err: impl std::fmt::Display) -> CredentialError {
    CredentialError::Storage(err.to_string())
}

fn vault_storage(err: impl std::fmt::Display) -> ManagedVaultMutationError {
    ManagedVaultMutationError::Store(storage(err))
}

async fn open_pool(location: EmbeddedSqlite) -> Result<EmbeddedPool, StoreError> {
    location
        .open()
        .await
        .map_err(|err| StoreError::Open(err.to_string()))
}

/// Apply the credential bundle once and construct both adapters over the same
/// serialized pool. This is the canonical application startup path: the
/// metadata and sealed-material halves are one credential persistence boundary,
/// so they must not create competing SQLite writers or repeat migration work.
pub async fn open_migrated_pair(
    path: &str,
) -> Result<(SqliteCredentialRepo, SqliteSealedBlobStore), StoreError> {
    let pool = open_pool(EmbeddedSqlite::file(path)).await?;
    run_migrations(&pool).await?;
    Ok((
        SqliteCredentialRepo { pool: pool.clone() },
        SqliteSealedBlobStore { pool },
    ))
}

/// One JSON row by primary key, deserialized; `not_found` shapes the miss.
async fn get_row<T: serde::de::DeserializeOwned>(
    conn: &mut SqliteConnection,
    sql: &str,
    key: &str,
    not_found: impl FnOnce(String) -> CredentialError,
) -> Result<T, CredentialError> {
    let data: Option<String> = sqlx::query_scalar(sql)
        .bind(key)
        .fetch_optional(&mut *conn)
        .await
        .map_err(storage)?;
    let data = data.ok_or_else(|| not_found(key.to_string()))?;
    serde_json::from_str(&data).map_err(storage)
}

/// All JSON rows of one workspace, in insertion order.
async fn list_rows<T: serde::de::DeserializeOwned>(
    conn: &mut SqliteConnection,
    sql: &str,
    workspace_id: &str,
) -> Result<Vec<T>, CredentialError> {
    let rows: Vec<String> = sqlx::query_scalar(sql)
        .bind(workspace_id)
        .fetch_all(&mut *conn)
        .await
        .map_err(storage)?;
    rows.into_iter()
        .map(|data| serde_json::from_str(&data).map_err(storage))
        .collect()
}

/// A SQLite-backed [`CredentialRepo`] (secret-free rows only; the sealed
/// material goes through [`SqliteSealedBlobStore`]).
#[derive(Clone)]
pub struct SqliteCredentialRepo {
    pool: EmbeddedPool,
}

#[async_trait::async_trait]
impl ManagedVaultRepo for SqliteCredentialRepo {
    async fn insert_vault(
        &self,
        workspace_id: &str,
        vault: ManagedVault,
    ) -> Result<(), CredentialError> {
        if !managed_vault_workspace_admitted(workspace_id, vault.workspace_id.as_str(), None) {
            return Err(CredentialError::InvalidSource(
                "Managed Vault workspace does not match its authority".into(),
            ));
        }
        let p = NS;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage)?;
        let owner: Option<String> = sqlx::query_scalar(&format!(
            "SELECT workspace_id FROM {p}_managed_vault WHERE id = ?1"
        ))
        .bind(&vault.id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        if !managed_vault_workspace_admitted(
            workspace_id,
            vault.workspace_id.as_str(),
            owner.as_deref(),
        ) {
            return Err(CredentialError::InvalidSource(
                "Managed Vault id belongs to another workspace".into(),
            ));
        }
        let data = serde_json::to_string(&vault).map_err(storage)?;
        let changed = sqlx::query(&format!(
            "INSERT OR IGNORE INTO {p}_managed_vault (id, workspace_id, data) VALUES (?1, ?2, ?3)"
        ))
        .bind(&vault.id)
        .bind(&vault.workspace_id)
        .bind(data)
        .execute(&mut *tx)
        .await
        .map_err(storage)?
        .rows_affected();
        if changed == 0 {
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
        if !managed_vault_workspace_admitted(workspace_id, vault.workspace_id.as_str(), None) {
            return Err(CredentialError::InvalidSource(
                "Managed Vault workspace does not match its authority".into(),
            ));
        }
        let p = NS;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage)?;
        sqlx::query(&format!(
            "INSERT OR IGNORE INTO {p}_managed_vault (id, workspace_id, data) VALUES (?1, ?2, ?3)"
        ))
        .bind(&vault.id)
        .bind(&vault.workspace_id)
        .bind(serde_json::to_string(&vault).map_err(storage)?)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        let (owner, data) = sqlx::query_as::<_, (String, String)>(&format!(
            "SELECT workspace_id, data FROM {p}_managed_vault WHERE id = ?1"
        ))
        .bind(&vault.id)
        .fetch_one(&mut *tx)
        .await
        .map_err(storage)?;
        if !managed_vault_workspace_admitted(
            workspace_id,
            vault.workspace_id.as_str(),
            Some(owner.as_str()),
        ) {
            return Err(CredentialError::InvalidSource(
                "Managed Vault id belongs to another workspace".into(),
            ));
        }
        let durable = serde_json::from_str(&data).map_err(storage)?;
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
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(vault_storage)?;
        let current = sqlx::query_scalar::<_, String>(&format!(
            "SELECT data FROM {p}_managed_vault WHERE workspace_id = ?1 AND id = ?2"
        ))
        .bind(workspace_id)
        .bind(&vault.id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(vault_storage)?
        .map(|data| serde_json::from_str::<ManagedVault>(&data).map_err(vault_storage))
        .transpose()?;
        admit_managed_vault_replacement(workspace_id, current.as_ref(), expected_revision, &vault)?;
        let data = serde_json::to_string(&vault).map_err(vault_storage)?;
        sqlx::query(&format!(
            "UPDATE {p}_managed_vault SET data = ?1 WHERE workspace_id = ?2 AND id = ?3"
        ))
        .bind(data)
        .bind(workspace_id)
        .bind(&vault.id)
        .execute(&mut *tx)
        .await
        .map_err(vault_storage)?;
        tx.commit().await.map_err(vault_storage)?;
        Ok(())
    }

    async fn get_vault(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Option<ManagedVault>, CredentialError> {
        let p = NS;
        let data: Option<String> = sqlx::query_scalar(&format!(
            "SELECT data FROM {p}_managed_vault WHERE workspace_id = ?1 AND id = ?2"
        ))
        .bind(workspace_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        data.map(|data| serde_json::from_str(&data).map_err(storage))
            .transpose()
    }

    async fn list_vaults(&self, workspace_id: &str) -> Result<Vec<ManagedVault>, CredentialError> {
        let p = NS;
        let mut conn = self.pool.acquire().await.map_err(storage)?;
        list_rows(
            &mut conn,
            &format!("SELECT data FROM {p}_managed_vault WHERE workspace_id = ?1 ORDER BY id"),
            workspace_id,
        )
        .await
    }

    async fn get_vault_credential(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Option<ManagedVaultCredential>, CredentialError> {
        let p = NS;
        let data: Option<String> = sqlx::query_scalar(&format!(
            "SELECT data FROM {p}_managed_vault_credential WHERE workspace_id = ?1 AND id = ?2"
        ))
        .bind(workspace_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        data.map(|data| serde_json::from_str(&data).map_err(storage))
            .transpose()
    }

    async fn get_vault_credential_by_source(
        &self,
        workspace_id: &str,
        source_id: &CredentialSourceId,
    ) -> Result<Option<ManagedVaultCredential>, CredentialError> {
        let p = NS;
        let data: Option<String> = sqlx::query_scalar(&format!(
            "SELECT data FROM {p}_managed_vault_credential WHERE workspace_id = ?1 AND source_id = ?2"
        ))
        .bind(workspace_id)
        .bind(&source_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        data.map(|data| serde_json::from_str(&data).map_err(storage))
            .transpose()
    }

    async fn list_vault_credentials(
        &self,
        workspace_id: &str,
        vault_id: &str,
    ) -> Result<Vec<ManagedVaultCredential>, CredentialError> {
        let p = NS;
        let rows: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT data FROM {p}_managed_vault_credential WHERE workspace_id = ?1 AND vault_id = ?2 ORDER BY id"
        ))
        .bind(workspace_id)
        .bind(vault_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        rows.into_iter()
            .map(|data| serde_json::from_str(&data).map_err(storage))
            .collect()
    }
}

impl SqliteCredentialRepo {
    /// Open (or create) a database file and apply the credential migrations
    /// (one-step convenience for a store-owned database).
    pub async fn open(path: &str) -> Result<Self, StoreError> {
        let store = Self::over(open_pool(EmbeddedSqlite::file(path)).await?);
        store.ensure_schema().await?;
        Ok(store)
    }

    /// Open a private in-memory database for tests and scenario fixtures.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn open_in_memory() -> Result<Self, StoreError> {
        let store = Self::over(open_pool(EmbeddedSqlite::memory()).await?);
        store.ensure_schema().await?;
        Ok(store)
    }

    /// Wrap an existing pool **without migrating**. Call [`Self::ensure_schema`],
    /// or let a unified migration pipeline own the `credential` scope so this store
    /// shares the caller's database.
    pub fn over(pool: EmbeddedPool) -> Self {
        Self { pool }
    }

    /// Apply the `credential` scoped migration bundle (idempotent). Optional: skip it
    /// when the schema is owned externally.
    pub async fn ensure_schema(&self) -> Result<(), StoreError> {
        run_migrations(&self.pool).await
    }
}

mod managed;

#[async_trait::async_trait]
impl CredentialRepo for SqliteCredentialRepo {
    async fn put(&self, source: CredentialSource) -> Result<(), CredentialError> {
        let p = NS;
        let data = serde_json::to_string(&source).map_err(storage)?;
        sqlx::query(&format!(
            "INSERT INTO {p}_source (id, workspace_id, data) VALUES (?1, ?2, ?3) \
             ON CONFLICT(id) DO UPDATE SET \
             workspace_id = excluded.workspace_id, data = excluded.data"
        ))
        .bind(&source.id.0)
        .bind(&source.workspace_id)
        .bind(data)
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
        let data = serde_json::to_string(&source).map_err(storage)?;
        let mut conn = self.pool.acquire().await.map_err(storage)?;
        sqlx::query(&format!(
            "INSERT OR IGNORE INTO {p}_source (id, workspace_id, data) VALUES (?1, ?2, ?3)"
        ))
        .bind(&source.id.0)
        .bind(&source.workspace_id)
        .bind(data)
        .execute(&mut *conn)
        .await
        .map_err(storage)?;
        get_row(
            &mut conn,
            &format!("SELECT data FROM {p}_source WHERE id = ?1"),
            &source.id.0,
            CredentialError::SourceNotFound,
        )
        .await
    }

    async fn get(&self, id: &CredentialSourceId) -> Result<CredentialSource, CredentialError> {
        let p = NS;
        let mut conn = self.pool.acquire().await.map_err(storage)?;
        get_row(
            &mut conn,
            &format!("SELECT data FROM {p}_source WHERE id = ?1"),
            &id.0,
            CredentialError::SourceNotFound,
        )
        .await
    }

    async fn list(&self, workspace_id: &str) -> Result<Vec<CredentialSource>, CredentialError> {
        let p = NS;
        let mut conn = self.pool.acquire().await.map_err(storage)?;
        list_rows(
            &mut conn,
            &format!("SELECT data FROM {p}_source WHERE workspace_id = ?1 ORDER BY rowid"),
            workspace_id,
        )
        .await
    }

    async fn begin_mutation(
        &self,
        intent: CredentialMutationIntent,
        now_unix_ms: u64,
    ) -> Result<bool, CredentialError> {
        intent.validate_for_begin(now_unix_ms)?;
        let p = NS;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage)?;
        let durable_data: Option<String> = sqlx::query_scalar(&format!(
            "SELECT data FROM {p}_creation_intent WHERE source_id = ?1"
        ))
        .bind(&intent.after.id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        if let Some(data) = durable_data {
            let durable: CredentialMutationIntent = serde_json::from_str(&data).map_err(storage)?;
            if !durable.matches_logical_command(&intent) {
                return Err(CredentialError::MutationConflict(
                    "another credential mutation is pending".into(),
                ));
            }
            tx.commit().await.map_err(storage)?;
            return Ok(false);
        }

        let current_after_data: Option<String> =
            sqlx::query_scalar(&format!("SELECT data FROM {p}_source WHERE id = ?1"))
                .bind(&intent.after.id.0)
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage)?;
        let current_after: Option<CredentialSource> = current_after_data
            .map(|data| serde_json::from_str(&data).map_err(storage))
            .transpose()?;
        let before_matches = if intent.is_distinct_replacement() {
            let before = intent.before.as_ref().ok_or_else(|| {
                CredentialError::InvalidSource(
                    "distinct replacement requires an exact predecessor".into(),
                )
            })?;
            let current_before_data: Option<String> =
                sqlx::query_scalar(&format!("SELECT data FROM {p}_source WHERE id = ?1"))
                    .bind(&before.id.0)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(storage)?;
            let current_before: Option<CredentialSource> = current_before_data
                .map(|data| serde_json::from_str(&data).map_err(storage))
                .transpose()?;
            current_after.is_none() && current_before.as_ref() == Some(before)
        } else {
            current_after.as_ref() == intent.before.as_ref()
        };
        if !before_matches {
            return Err(CredentialError::MutationConflict(
                "credential changed before its material mutation was prepared".into(),
            ));
        }
        let data = serde_json::to_string(&intent).map_err(storage)?;
        let inserted = sqlx::query(&format!(
            "INSERT OR IGNORE INTO {p}_creation_intent (source_id, data) VALUES (?1, ?2)"
        ))
        .bind(&intent.after.id.0)
        .bind(data)
        .execute(&mut *tx)
        .await
        .map_err(storage)?
        .rows_affected();
        let durable_data: String = sqlx::query_scalar(&format!(
            "SELECT data FROM {p}_creation_intent WHERE source_id = ?1"
        ))
        .bind(&intent.after.id.0)
        .fetch_one(&mut *tx)
        .await
        .map_err(storage)?;
        let durable: CredentialMutationIntent =
            serde_json::from_str(&durable_data).map_err(storage)?;
        if !durable.matches_logical_command(&intent) {
            return Err(CredentialError::MutationConflict(
                "another credential mutation is pending".into(),
            ));
        }
        tx.commit().await.map_err(storage)?;
        Ok(inserted == 1)
    }

    async fn mark_mutation_ready(
        &self,
        intent: &CredentialMutationIntent,
    ) -> Result<CredentialMutationIntent, CredentialError> {
        intent.validate()?;
        let ready = intent.with_material_ready()?;
        let ready_for_store = ready.clone();
        let p = NS;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage)?;
        let durable_data: Option<String> = sqlx::query_scalar(&format!(
            "SELECT data FROM {p}_creation_intent WHERE source_id = ?1"
        ))
        .bind(&intent.after.id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        let durable: CredentialMutationIntent = durable_data
            .map(|data| serde_json::from_str(&data).map_err(storage))
            .transpose()?
            .ok_or_else(|| {
                CredentialError::MutationConflict(
                    "credential material mutation has no durable pending fact".into(),
                )
            })?;
        if &durable != intent {
            return Err(CredentialError::MutationConflict(
                "credential ready transition does not match its durable Writing owner".into(),
            ));
        }
        let data = serde_json::to_string(&ready_for_store).map_err(storage)?;
        sqlx::query(&format!(
            "UPDATE {p}_creation_intent SET data = ?2 WHERE source_id = ?1"
        ))
        .bind(&intent.after.id.0)
        .bind(data)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        Ok(ready_for_store)
    }

    async fn apply_mutation(
        &self,
        intent: &CredentialMutationIntent,
    ) -> Result<CredentialMutationIntent, CredentialError> {
        let p = NS;
        // This mutation reads its durable intent/current revision before it
        // writes. In WAL mode a deferred transaction cannot upgrade an old
        // read snapshot after a sibling adapter commits; SQLite returns BUSY
        // immediately instead of honoring the configured busy timeout. Take
        // the write reservation up front so bounded sibling contention waits
        // at transaction start and the read-to-write snapshot stays valid.
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage)?;
        let intent_data: Option<String> = sqlx::query_scalar(&format!(
            "SELECT data FROM {p}_creation_intent WHERE source_id = ?1"
        ))
        .bind(&intent.after.id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        let durable: CredentialMutationIntent = intent_data
            .map(|data| serde_json::from_str(&data).map_err(storage))
            .transpose()?
            .ok_or_else(|| {
                CredentialError::MutationConflict(
                    "credential mutation has no matching durable intent".into(),
                )
            })?;
        let current_data: Option<String> =
            sqlx::query_scalar(&format!("SELECT data FROM {p}_source WHERE id = ?1"))
                .bind(&intent.after.id.0)
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage)?;
        let current: Option<CredentialSource> = current_data
            .map(|data| serde_json::from_str(&data).map_err(storage))
            .transpose()?;
        let current_before = if let Some(before) = intent
            .before
            .as_ref()
            .filter(|before| before.id != intent.after.id)
        {
            let data: Option<String> =
                sqlx::query_scalar(&format!("SELECT data FROM {p}_source WHERE id = ?1"))
                    .bind(&before.id.0)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(storage)?;
            data.map(|data| serde_json::from_str(&data).map_err(storage))
                .transpose()?
        } else {
            None
        };
        let plan = intent.publication_plan(&durable, current.as_ref(), current_before.as_ref())?;
        if plan.mode == CredentialMutationCommitMode::ExactReplay {
            tx.commit().await.map_err(storage)?;
            return Ok(plan.reclaiming_mutation);
        }
        let id = plan.source.id.0.clone();
        let workspace_id = plan.source.workspace_id.clone();
        let data = serde_json::to_string(&plan.source).map_err(storage)?;
        match plan.mode {
            CredentialMutationCommitMode::Insert => {
                sqlx::query(&format!(
                    "INSERT INTO {p}_source (id, workspace_id, data) VALUES (?1, ?2, ?3)"
                ))
                .bind(&id)
                .bind(&workspace_id)
                .bind(&data)
                .execute(&mut *tx)
                .await
                .map_err(storage)?;
            }
            CredentialMutationCommitMode::Replace => {
                sqlx::query(&format!(
                    "INSERT INTO {p}_source (id, workspace_id, data) VALUES (?1, ?2, ?3) \
                     ON CONFLICT(id) DO UPDATE SET workspace_id = excluded.workspace_id, data = excluded.data"
                ))
                .bind(&id)
                .bind(&workspace_id)
                .bind(&data)
                .execute(&mut *tx)
                .await
                .map_err(storage)?;
            }
            CredentialMutationCommitMode::ExactReplay => unreachable!("returned above"),
        }
        let reclaiming_data = serde_json::to_string(&plan.reclaiming_mutation).map_err(storage)?;
        sqlx::query(&format!(
            "UPDATE {p}_creation_intent SET data = ?2 WHERE source_id = ?1"
        ))
        .bind(&intent.after.id.0)
        .bind(reclaiming_data)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        Ok(plan.reclaiming_mutation)
    }

    async fn pending_mutations(&self) -> Result<Vec<CredentialMutationIntent>, CredentialError> {
        let p = NS;
        let rows = sqlx::query_as::<_, (String, String)>(&format!(
            "SELECT source_id, data FROM {p}_creation_intent ORDER BY created_at, source_id"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        rows.into_iter()
            .map(|(source_id, data)| {
                let intent =
                    serde_json::from_str::<CredentialMutationIntent>(&data).map_err(storage)?;
                intent.validate_durable_key(&source_id)?;
                Ok(intent)
            })
            .collect()
    }

    async fn claim_expired_mutation(
        &self,
        intent: &CredentialMutationIntent,
        now_unix_ms: u64,
        lease_expires_at_unix_ms: u64,
    ) -> Result<Option<CredentialMutationIntent>, CredentialError> {
        let Some(claimed) = intent.claim_after_expiry(now_unix_ms, lease_expires_at_unix_ms)?
        else {
            return Ok(None);
        };
        let claimed_for_store = claimed.clone();
        let p = NS;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage)?;
        let durable_data: Option<String> = sqlx::query_scalar(&format!(
            "SELECT data FROM {p}_creation_intent WHERE source_id = ?1"
        ))
        .bind(&intent.after.id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        let Some(data) = durable_data else {
            tx.commit().await.map_err(storage)?;
            return Ok(None);
        };
        let durable: CredentialMutationIntent = serde_json::from_str(&data).map_err(storage)?;
        // Successful claim already requires Writing; full snapshot equality
        // also fences phase changes before this locked write.
        if &durable != intent {
            tx.commit().await.map_err(storage)?;
            return Ok(None);
        }
        let data = serde_json::to_string(&claimed_for_store).map_err(storage)?;
        sqlx::query(&format!(
            "UPDATE {p}_creation_intent SET data = ?2 WHERE source_id = ?1"
        ))
        .bind(&intent.after.id.0)
        .bind(data)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        Ok(Some(claimed_for_store))
    }

    async fn abort_mutation(
        &self,
        intent: &CredentialMutationIntent,
    ) -> Result<CredentialMutationIntent, CredentialError> {
        intent.validate()?;
        let p = NS;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage)?;
        let durable_data: Option<String> = sqlx::query_scalar(&format!(
            "SELECT data FROM {p}_creation_intent WHERE source_id = ?1"
        ))
        .bind(&intent.after.id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        let durable: CredentialMutationIntent = durable_data
            .map(|data| serde_json::from_str(&data).map_err(storage))
            .transpose()?
            .ok_or_else(|| {
                CredentialError::MutationConflict(
                    "credential abort has no durable pending fact".into(),
                )
            })?;
        let current_after_data: Option<String> =
            sqlx::query_scalar(&format!("SELECT data FROM {p}_source WHERE id = ?1"))
                .bind(&intent.after.id.0)
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage)?;
        let current_after: Option<CredentialSource> = current_after_data
            .map(|data| serde_json::from_str(&data).map_err(storage))
            .transpose()?;
        let abort_for_store = intent.admit_abort(&durable, current_after.as_ref())?;
        let data = serde_json::to_string(&abort_for_store).map_err(storage)?;
        sqlx::query(&format!(
            "UPDATE {p}_creation_intent SET data = ?2 WHERE source_id = ?1"
        ))
        .bind(&intent.after.id.0)
        .bind(data)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        Ok(abort_for_store)
    }

    async fn complete_mutation(
        &self,
        intent: &CredentialMutationIntent,
    ) -> Result<(), CredentialError> {
        intent.validate()?;
        let p = NS;
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage)?;
        let durable_data: Option<String> = sqlx::query_scalar(&format!(
            "SELECT data FROM {p}_creation_intent WHERE source_id = ?1"
        ))
        .bind(&intent.after.id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        let Some(data) = durable_data else {
            tx.commit().await.map_err(storage)?;
            return Ok(());
        };
        let durable: CredentialMutationIntent = serde_json::from_str(&data).map_err(storage)?;
        let current_after_data: Option<String> =
            sqlx::query_scalar(&format!("SELECT data FROM {p}_source WHERE id = ?1"))
                .bind(&intent.after.id.0)
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage)?;
        let current_after: Option<CredentialSource> = current_after_data
            .map(|data| serde_json::from_str(&data).map_err(storage))
            .transpose()?;
        intent.admit_completion(&durable, current_after.as_ref())?;
        sqlx::query(&format!(
            "DELETE FROM {p}_creation_intent WHERE source_id = ?1"
        ))
        .bind(&intent.after.id.0)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        tx.commit().await.map_err(storage)
    }

    async fn material_refs(&self) -> Result<Vec<SecretRef>, CredentialError> {
        let p = NS;
        let rows: Vec<String> =
            sqlx::query_scalar(&format!("SELECT data FROM {p}_source ORDER BY id"))
                .fetch_all(&self.pool)
                .await
                .map_err(storage)?;
        let sources: Vec<CredentialSource> = rows
            .into_iter()
            .map(|row| serde_json::from_str(&row).map_err(storage))
            .collect::<Result<_, _>>()?;
        Ok(sources
            .into_iter()
            .flat_map(|source| {
                source
                    .material_ref
                    .into_iter()
                    .chain(source.auxiliary_material_refs.into_values())
            })
            .collect())
    }

    async fn put_pool(&self, pool: CredentialPool) -> Result<(), CredentialError> {
        let p = NS;
        let data = serde_json::to_string(&pool).map_err(storage)?;
        sqlx::query(&format!(
            "INSERT INTO {p}_pool (id, workspace_id, data) VALUES (?1, ?2, ?3) \
             ON CONFLICT(id) DO UPDATE SET \
             workspace_id = excluded.workspace_id, data = excluded.data"
        ))
        .bind(&pool.id.0)
        .bind(&pool.workspace_id)
        .bind(data)
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn get_pool(&self, id: &CredentialPoolId) -> Result<CredentialPool, CredentialError> {
        let p = NS;
        let mut conn = self.pool.acquire().await.map_err(storage)?;
        get_row(
            &mut conn,
            &format!("SELECT data FROM {p}_pool WHERE id = ?1"),
            &id.0,
            CredentialError::PoolNotFound,
        )
        .await
    }

    async fn list_pools(&self, workspace_id: &str) -> Result<Vec<CredentialPool>, CredentialError> {
        let p = NS;
        let mut conn = self.pool.acquire().await.map_err(storage)?;
        list_rows(
            &mut conn,
            &format!("SELECT data FROM {p}_pool WHERE workspace_id = ?1 ORDER BY rowid"),
            workspace_id,
        )
        .await
    }
}

/// A SQLite-backed [`SealedBlobStore`]: opaque `nonce ‖ ciphertext` blobs in
/// `{prefix}_secret`, keyed by [`SecretRef`]. Not a `SecretStore` — compose it
/// under `SealedAeadSecretStore::over` so only sealed bytes ever hit the disk.
pub struct SqliteSealedBlobStore {
    pool: EmbeddedPool,
}

impl SqliteSealedBlobStore {
    /// Open (or create) a database file and apply the credential migrations
    /// (one-step convenience for a store-owned database).
    pub async fn open(path: &str) -> Result<Self, StoreError> {
        let store = Self::over(open_pool(EmbeddedSqlite::file(path)).await?);
        store.ensure_schema().await?;
        Ok(store)
    }

    /// Open a private in-memory database for tests and scenario fixtures.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn open_in_memory() -> Result<Self, StoreError> {
        let store = Self::over(open_pool(EmbeddedSqlite::memory()).await?);
        store.ensure_schema().await?;
        Ok(store)
    }

    /// Wrap an existing pool **without migrating**. Call [`Self::ensure_schema`],
    /// or let a unified migration pipeline own the `credential` scope so this store
    /// shares the caller's database.
    pub fn over(pool: EmbeddedPool) -> Self {
        Self { pool }
    }

    /// Apply the `credential` scoped migration bundle (idempotent). Optional: skip it
    /// when the schema is owned externally.
    pub async fn ensure_schema(&self) -> Result<(), StoreError> {
        run_migrations(&self.pool).await
    }
}

#[async_trait::async_trait]
impl SealedBlobStore for SqliteSealedBlobStore {
    async fn put_blob(&self, r: &SecretRef, blob: Vec<u8>) -> Result<(), CredentialError> {
        let p = NS;
        let affected = sqlx::query(&format!(
            "INSERT INTO {p}_secret (secret_ref, sealed) VALUES (?1, ?2) \
             ON CONFLICT(secret_ref) DO UPDATE SET sealed = excluded.sealed \
             WHERE {p}_secret.closed = 0"
        ))
        .bind(&r.0)
        .bind(blob)
        .execute(&self.pool)
        .await
        .map_err(storage)?
        .rows_affected();
        if affected == 0 {
            return Err(CredentialError::MutationConflict(
                "material reference is closed".into(),
            ));
        }
        Ok(())
    }

    async fn get_blob(&self, r: &SecretRef) -> Result<Vec<u8>, CredentialError> {
        let p = NS;
        let blob: Option<Vec<u8>> = sqlx::query_scalar(&format!(
            "SELECT sealed FROM {p}_secret WHERE secret_ref = ?1 AND closed = 0"
        ))
        .bind(&r.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        blob.ok_or_else(|| CredentialError::SecretNotFound(r.0.clone()))
    }

    async fn delete_blob(&self, r: &SecretRef) -> Result<(), CredentialError> {
        let p = NS;
        sqlx::query(&format!(
            "INSERT INTO {p}_secret (secret_ref, sealed, closed) VALUES (?1, ?2, 1) \
             ON CONFLICT(secret_ref) DO UPDATE SET sealed = excluded.sealed, closed = 1"
        ))
        .bind(&r.0)
        .bind(Vec::<u8>::new())
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn inventory_blobs(&self) -> Result<Vec<SecretRef>, CredentialError> {
        let p = NS;
        let rows: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT secret_ref FROM {p}_secret WHERE closed = 0 ORDER BY secret_ref"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        Ok(rows.into_iter().map(SecretRef).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_credential_vault::{CredentialKind, CredentialStatus};
    use std::time::Duration;

    fn source(id: &str) -> CredentialSource {
        CredentialSource {
            id: CredentialSourceId(id.into()),
            replacement_of: None,
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            descriptor: None,
            provider_id: Some("anthropic".into()),
            protocol_endpoint_id: None,
            env_key: Some("ANTHROPIC_API_KEY".into()),
            material_ref: None,
            auxiliary_material_refs: Default::default(),
            oauth_command: None,
            worker_local_binding: None,
            status: CredentialStatus::Active,
            version: 1,
        }
    }

    /// Both stores' injection seam: a caller-owned pool, `over` wraps it WITHOUT
    /// migrating, `ensure_schema` applies the shared `credential` scope — so a unified
    /// migration pipeline can own the schema and these stores share the caller's
    /// database (mirrors the resource-family stores). Round-trips the repo and the
    /// sealed blob store, which key the same scope.
    #[tokio::test]
    async fn over_then_ensure_schema_round_trips_on_a_caller_owned_connection() {
        let repo = SqliteCredentialRepo::over(EmbeddedSqlite::memory().open().await.unwrap());
        repo.ensure_schema().await.unwrap();
        repo.put(source("cred:a")).await.unwrap();
        assert_eq!(repo.list("ws").await.unwrap().len(), 1);

        let blobs = SqliteSealedBlobStore::over(EmbeddedSqlite::memory().open().await.unwrap());
        blobs.ensure_schema().await.unwrap();
        let r = SecretRef("sec:a".into());
        blobs.put_blob(&r, b"sealed".to_vec()).await.unwrap();
        assert_eq!(blobs.get_blob(&r).await.unwrap(), b"sealed");
    }

    /// A sibling writer on its own pool that holds `BEGIN IMMEDIATE` (after
    /// running `setup` inside it) until the returned connection commits.
    async fn sibling_writer(
        path: &str,
        setup: Option<&'static str>,
    ) -> (EmbeddedPool, awaken_sqlite_pool::EmbeddedConnection) {
        let pool = EmbeddedSqlite::file(path).open().await.unwrap();
        let mut connection = pool.acquire().await.unwrap();
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *connection)
            .await
            .unwrap();
        if let Some(secret_ref) = setup {
            sqlx::query("INSERT INTO credential_secret (secret_ref, sealed) VALUES (?1, ?2)")
                .bind(secret_ref)
                .bind(&b"sealed"[..])
                .execute(&mut *connection)
                .await
                .unwrap();
        }
        (pool, connection)
    }

    /// Commit the sibling writer after a short hold, concurrently with the
    /// store operation that must wait for it.
    fn release_after_short_hold(
        mut connection: awaken_sqlite_pool::EmbeddedConnection,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            // awaken-allow(wait): R2/R3/R5 hold the sibling write lock for a short, fixed span so the store must busy-wait for it.
            tokio::time::sleep(Duration::from_millis(100)).await;
            sqlx::query("COMMIT")
                .execute(&mut *connection)
                .await
                .unwrap();
        })
    }

    #[tokio::test]
    async fn file_stores_wait_for_short_lived_sibling_writer_contention() {
        // Cause/effect decision table:
        // | Rule | adapter | sibling write lock | Effect |
        // | R1 | metadata | absent | normal write succeeds (covered by conformance) |
        // | R2 | metadata | short-lived | waits, then writes without leaking SQLITE_BUSY |
        // | R3 | sealed blob | short-lived | waits, then writes without leaking SQLITE_BUSY |
        // | R4 | metadata read | long-lived | WAL snapshot reads without waiting for the writer |
        // | R5 | mutation read-to-write | short-lived | reserves the writer first, waits, then commits without SQLITE_BUSY |
        // A lock held longer than the bounded timeout still fails closed; this test
        // covers the transient write contention and concurrent provisioning reads
        // produced by sibling adapters in one host.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credential.db");
        let path = path.to_string_lossy().into_owned();
        let repo = SqliteCredentialRepo::open(&path).await.unwrap();
        let blobs = SqliteSealedBlobStore::open(&path).await.unwrap();

        for write in ["metadata", "blob"] {
            let (_blocker, connection) = sibling_writer(&path, None).await;
            let blocker = release_after_short_hold(connection);
            if write == "metadata" {
                repo.put(source("cred:contended")).await.unwrap();
            } else {
                blobs
                    .put_blob(&SecretRef("sec:contended".into()), b"sealed".to_vec())
                    .await
                    .unwrap();
            }
            blocker.await.unwrap();
        }

        let mut contended = source("cred:mutation-contended");
        contended.provider_id = Some("provider-before".into());
        repo.put(contended.clone()).await.unwrap();
        let mut updated = contended.clone();
        updated.provider_id = Some("provider-after".into());
        updated.version += 1;
        let intent =
            CredentialMutationIntent::prepare(Some(contended), updated.clone(), 1_000).unwrap();
        repo.begin_mutation(intent.clone(), 1_000).await.unwrap();
        let (_blocker, connection) = sibling_writer(&path, Some("sec:mutation-writer")).await;
        let blocker = release_after_short_hold(connection);
        repo.apply_mutation(&intent).await.unwrap();
        blocker.await.unwrap();
        assert_eq!(
            repo.get(&CredentialSourceId("cred:mutation-contended".into()))
                .await
                .unwrap(),
            updated,
            "R5"
        );

        repo.put(source("cred:read-during-write")).await.unwrap();
        let (_blocker, mut connection) = sibling_writer(&path, Some("sec:writer")).await;
        let read = tokio::time::timeout(
            Duration::from_millis(500),
            repo.get(&CredentialSourceId("cred:read-during-write".into())),
        )
        .await
        .expect("WAL reader must not wait for a sibling writer")
        .unwrap();
        assert_eq!(read.id.0, "cred:read-during-write");
        sqlx::query("COMMIT")
            .execute(&mut *connection)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn migrated_pair_owns_one_credential_persistence_boundary() {
        // Cause/effect decision table:
        // | Rule | startup path | metadata write | sealed write | Effect |
        // | R1 | canonical pair | succeeds | succeeds | both halves share one migrated scope |
        // | R2 | standalone seam | succeeds | n/a | retained for isolated adapter tests |
        // | R3 | standalone seam | n/a | succeeds | retained for isolated adapter tests |
        // Standalone coverage lives above; this rule prevents application
        // composition from reopening and migrating one bounded context twice.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credential.db");
        let path = path.to_string_lossy().into_owned();
        let (repo, blobs) = open_migrated_pair(&path).await.unwrap();

        repo.put(source("cred:paired")).await.unwrap();
        let reference = SecretRef("sec:paired".into());
        blobs
            .put_blob(&reference, b"sealed".to_vec())
            .await
            .unwrap();

        assert_eq!(
            repo.get(&CredentialSourceId("cred:paired".into()))
                .await
                .unwrap()
                .id
                .0,
            "cred:paired"
        );
        assert_eq!(blobs.get_blob(&reference).await.unwrap(), b"sealed");
    }

    fn managed_credential(source_id: &str, credential_id: &str) -> ManagedVaultCredential {
        use awaken_credential_vault::catalog::{ManagedCredentialAuth, ManagedCredentialLifecycle};
        ManagedVaultCredential {
            id: credential_id.into(),
            vault_id: "vlt-managed-atomic".into(),
            workspace_id: "ws".into(),
            source_id: CredentialSourceId(source_id.into()),
            auth: ManagedCredentialAuth::StaticBearer {
                mcp_server_url: "https://mcp.example.com/atomic".into(),
            },
            metadata: Default::default(),
            display_name: None,
            created_at: "2026-09-17T00:00:00Z".into(),
            updated_at: "2026-09-17T00:00:00Z".into(),
            revision: 1,
            lifecycle: ManagedCredentialLifecycle::Active,
        }
    }

    fn managed_vault() -> ManagedVault {
        ManagedVault {
            id: "vlt-managed-atomic".into(),
            workspace_id: "ws".into(),
            display_name: "Managed atomic".into(),
            metadata: Default::default(),
            created_at: "2026-09-17T00:00:00Z".into(),
            updated_at: "2026-09-17T00:00:00Z".into(),
            archived_at: None,
            deletion: None,
            revision: 1,
        }
    }

    async fn seed_managed_child(repo: &SqliteCredentialRepo, credential: &ManagedVaultCredential) {
        sqlx::query(
            "INSERT INTO credential_managed_vault_credential \
             (id, vault_id, workspace_id, source_id, data) VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(&credential.id)
        .bind(&credential.vault_id)
        .bind(&credential.workspace_id)
        .bind(&credential.source_id.0)
        .bind(serde_json::to_string(credential).unwrap())
        .execute(&repo.pool)
        .await
        .unwrap();
    }

    /// Collision/acknowledgement cause/effect table. C1 an exact live pair is
    /// ready to update; C2 its would-be event id is already occupied by a
    /// different payload; C3 an acknowledgement presents a payload different
    /// from that durable event. Effects are E1 transaction conflict with the
    /// pair unchanged, E2 the original event retained, and E3 stale-ack
    /// rejection with that event still retained. Rules: SR1 C1+C2 -> E1+E2;
    /// SR2 C2+C3 -> E3+E2.
    #[tokio::test]
    async fn managed_update_event_collision_rolls_back_pair_and_stale_ack_is_rejected() {
        use awaken_credential_vault::InMemorySecretStore;
        use awaken_credential_vault::repo::{
            CredentialMaterialPatch, ManagedCredentialRepository, update_managed_credential,
        };

        let repo = SqliteCredentialRepo::open_in_memory().await.unwrap();
        repo.insert_vault("ws", managed_vault()).await.unwrap();
        let before_source = source("cred:managed-collision");
        let before_child = managed_credential(&before_source.id.0, "crd-managed-collision");
        repo.put(before_source.clone()).await.unwrap();
        seed_managed_child(&repo, &before_child).await;

        let event_id = format!(
            "managed-update:{}:{}:{}",
            before_source.id.0, before_source.version, before_child.revision
        );
        let conflicting = ManagedCredentialRollout {
            id: event_id.clone(),
            workspace_id: "other-workspace".into(),
            vault_id: "other-vault".into(),
            credential_id: "other-credential".into(),
            source_id: CredentialSourceId("other-source".into()),
            source_version: 99,
            credential_revision: 99,
            operation: ManagedCredentialOperation::Update,
        };
        sqlx::query(
            "INSERT INTO credential_managed_credential_rollout (event_id, data) VALUES (?1, ?2)",
        )
        .bind(&event_id)
        .bind(serde_json::to_string(&conflicting).unwrap())
        .execute(&repo.pool)
        .await
        .unwrap();

        let mut after_child = before_child.clone();
        after_child.display_name = Some("rotated".into());
        let result = update_managed_credential(
            before_child.clone(),
            after_child,
            CredentialMaterialPatch::default(),
            true,
            "2026-09-17T00:00:01Z".into(),
            &InMemorySecretStore::new(),
            &repo,
            awaken_time::system_clock().as_ref(),
        )
        .await;
        assert!(matches!(
            result,
            Err(ManagedCredentialMutationError::RevisionConflict)
        ));
        assert_eq!(repo.get(&before_source.id).await.unwrap(), before_source);
        assert_eq!(
            repo.get_vault_credential("ws", &before_child.id)
                .await
                .unwrap(),
            Some(before_child.clone())
        );
        assert_eq!(
            repo.pending_managed_rollouts().await.unwrap(),
            vec![conflicting.clone()]
        );
        assert_eq!(
            repo.managed_rollout(&conflicting.id).await.unwrap(),
            Some(conflicting.clone())
        );

        let stale_ack = ManagedCredentialRollout {
            id: conflicting.id.clone(),
            workspace_id: "ws".into(),
            vault_id: before_child.vault_id,
            credential_id: before_child.id,
            source_id: before_child.source_id,
            source_version: 2,
            credential_revision: 2,
            operation: ManagedCredentialOperation::Update,
        };
        assert!(matches!(
            repo.complete_managed_rollout(&stale_ack).await,
            Err(CredentialError::MutationConflict(_))
        ));
        assert_eq!(
            repo.pending_managed_rollouts().await.unwrap(),
            vec![conflicting.clone()]
        );
        assert_eq!(
            repo.managed_rollout(&conflicting.id).await.unwrap(),
            Some(conflicting)
        );
    }

    #[tokio::test]
    async fn mutation_decode_failure_blocks_the_batch_but_rollout_decode_is_isolated() {
        use awaken_credential_vault::repo::ManagedCredentialRepository;

        // Cause/effect decision table:
        // R1 decoded current + decoded future mutation rows -> both are returned so the
        //    recovery owner can reject the future row before SecretStore I/O;
        // R2 any undecodable mutation row -> the whole mutation scan fails and every row
        //    remains durable for operator repair;
        // R3 an undecodable rollout row -> that independent rollout is isolated while a
        //    healthy rollout remains visible to its existing recovery owner.
        let repo = SqliteCredentialRepo::open_in_memory().await.unwrap();
        let healthy_source = source("cred:healthy-pending");
        let healthy_child = managed_credential(&healthy_source.id.0, "crd-healthy-pending");
        let healthy_pending =
            PendingManagedCredentialMutation::create(healthy_source, healthy_child, 1_000).unwrap();
        let future_source = source("cred:future");
        let future_child = managed_credential(&future_source.id.0, "crd-future");
        let mut future_pending = serde_json::to_value(
            PendingManagedCredentialMutation::create(future_source, future_child, 1_000).unwrap(),
        )
        .unwrap();
        future_pending["format_version"] = serde_json::json!(2);
        let healthy_rollout = ManagedCredentialRollout {
            id: "managed-update:healthy-rollout".into(),
            workspace_id: "ws".into(),
            vault_id: "vlt-managed-atomic".into(),
            credential_id: "crd-healthy-rollout".into(),
            source_id: CredentialSourceId("cred:healthy-rollout".into()),
            source_version: 2,
            credential_revision: 2,
            operation: ManagedCredentialOperation::Update,
        };
        {
            let pending_sql = "INSERT INTO credential_managed_credential_mutation \
                               (source_id, data) VALUES (?1, ?2)";
            sqlx::query(pending_sql)
                .bind(&healthy_pending.after_source.id.0)
                .bind(serde_json::to_string(&healthy_pending).unwrap())
                .execute(&repo.pool)
                .await
                .unwrap();
            sqlx::query(pending_sql)
                .bind("cred:future")
                .bind(serde_json::to_string(&future_pending).unwrap())
                .execute(&repo.pool)
                .await
                .unwrap();
            let rollout_sql = "INSERT INTO credential_managed_credential_rollout \
                               (event_id, data) VALUES (?1, ?2)";
            sqlx::query(rollout_sql)
                .bind(&healthy_rollout.id)
                .bind(serde_json::to_string(&healthy_rollout).unwrap())
                .execute(&repo.pool)
                .await
                .unwrap();
            sqlx::query(rollout_sql)
                .bind("managed-update:future-rollout")
                .bind(r#"{"format_version":2}"#)
                .execute(&repo.pool)
                .await
                .unwrap();
        }

        let decoded = repo.pending_managed_mutations().await.unwrap();
        assert_eq!(decoded.len(), 2);
        assert!(decoded.contains(&healthy_pending));
        assert!(decoded.iter().any(|pending| pending.validate().is_err()));

        sqlx::query(
            "INSERT INTO credential_managed_credential_mutation (source_id, data) \
             VALUES (?1, ?2)",
        )
        .bind("cred:malformed")
        .bind("{not-json")
        .execute(&repo.pool)
        .await
        .unwrap();
        assert!(matches!(
            repo.pending_managed_mutations().await,
            Err(CredentialError::Storage(_))
        ));
        assert_eq!(
            repo.pending_managed_rollouts().await.unwrap(),
            vec![healthy_rollout]
        );
        let poison_count: i64 = sqlx::query_scalar(
            "SELECT (SELECT COUNT(*) FROM credential_managed_credential_mutation \
                     WHERE source_id IN ('cred:malformed', 'cred:future')) + \
                    (SELECT COUNT(*) FROM credential_managed_credential_rollout \
                     WHERE event_id = 'managed-update:future-rollout')",
        )
        .fetch_one(&repo.pool)
        .await
        .unwrap();
        assert_eq!(poison_count, 3, "isolated rows remain available for repair");
    }

    #[tokio::test]
    async fn abort_keeps_durable_cleanup_authority_until_exact_completion() {
        use awaken_credential_vault::repo::ManagedCredentialRepository;

        let repo = SqliteCredentialRepo::open_in_memory().await.unwrap();
        let pending = PendingManagedCredentialMutation::create(
            source("cred:abort-cleanup"),
            managed_credential("cred:abort-cleanup", "crd-abort-cleanup"),
            1_000,
        )
        .unwrap();
        repo.begin_managed_mutation(pending.clone(), 1_000)
            .await
            .unwrap();

        let reclaiming = repo.abort_managed_mutation(&pending).await.unwrap();
        assert_eq!(
            reclaiming.material_fence.phase,
            CredentialMaterialMutationPhase::ReclaimingAbort
        );
        assert_eq!(
            repo.pending_managed_mutations().await.unwrap(),
            vec![reclaiming.clone()],
            "external cleanup remains crash-recoverable"
        );
        assert!(matches!(
            repo.abort_managed_mutation(&pending).await,
            Err(CredentialError::MutationConflict(_))
        ));
        assert_eq!(
            repo.abort_managed_mutation(&reclaiming).await.unwrap(),
            reclaiming,
            "the exact cleanup fact is idempotent"
        );
        repo.complete_managed_mutation(&reclaiming).await.unwrap();
        assert!(repo.pending_managed_mutations().await.unwrap().is_empty());
        repo.complete_managed_mutation(&reclaiming).await.unwrap();
    }
}
