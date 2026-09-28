// @formal surface obligation=environment.registration_intent_is_atomic_recoverable_and_idempotent
// @formal reason: The Postgres half of the Environment registry, split out of the crate root; it appends
// @formal reason: the same registration intent inside the transaction that advances the authored revision.
//! The Postgres sibling of the Environment registry (distributed deployments).
//!
//! Split out so each adapter is read on its own; the schema bundle, the row
//! codec and the shared value helpers stay in the crate root, which is what
//! keeps the two backends one interpretation of a persisted Environment rather
//! than two.

use super::*;

/// Postgres persistence for the environment registry (distributed deployments).
pub struct PostgresEnvRegistry {
    pub(super) pool: PgPool,
    /// The one source of every observation timestamp this registry writes.
    clock: Arc<dyn Clock>,
}

impl PostgresEnvRegistry {
    pub async fn connect(url: &str, clock: Arc<dyn Clock>) -> Result<Self, String> {
        let pool = PgPool::connect(url).await.map_err(|e| e.to_string())?;
        Self::with_pool(pool, clock).await
    }

    pub async fn with_pool(pool: PgPool, clock: Arc<dyn Clock>) -> Result<Self, String> {
        let bundle = env_bundle().map_err(|e| e.to_string())?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|e| e.to_string())?
            .run_bundle(&bundle)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Self { pool, clock })
    }

    pub async fn connect_existing(url: &str, clock: Arc<dyn Clock>) -> Result<Self, String> {
        let pool = PgPool::connect(url).await.map_err(|e| e.to_string())?;
        let bundle = env_bundle().map_err(|e| e.to_string())?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|e| e.to_string())?
            .verify_bundle(&bundle)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Self { pool, clock })
    }

    async fn read(&self, id: &str) -> Result<Option<EnvItem>, EnvironmentStoreError> {
        sqlx::query(&format!(
            "SELECT {COLS} FROM env_registry_env WHERE env_id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(environment_store)?
        .map(|row| pg_row(&row))
        .transpose()
    }
}

#[async_trait]
impl EnvRegistry for PostgresEnvRegistry {
    async fn create_once(
        &self,
        command: CreateEnvironmentCommand,
    ) -> Result<CreateEnvironmentOutcome, CreateEnvironmentError> {
        command.config.validate()?;
        let committed_at = self.clock.observed_at();
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?;
        sqlx::query("LOCK TABLE env_registry_create_command IN SHARE ROW EXCLUSIVE MODE")
            .execute(&mut *tx)
            .await
            .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?;
        let fingerprint = command.fingerprint();
        let replay = sqlx::query(
            "SELECT fingerprint, env_id FROM env_registry_create_command WHERE command_id = $1",
        )
        .bind(&command.command_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?;
        if let Some(row) = replay {
            let existing_fingerprint: String = row.get("fingerprint");
            if existing_fingerprint != fingerprint {
                return Err(CreateEnvironmentError::IdempotencyConflict);
            }
            let environment_id: String = row.get("env_id");
            let item = sqlx::query(&format!(
                "SELECT {COLS} FROM env_registry_env WHERE env_id = $1"
            ))
            .bind(environment_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?
            .ok_or_else(|| CreateEnvironmentError::Store("command target is missing".into()))?;
            let item =
                pg_row(&item).map_err(|error| CreateEnvironmentError::Store(error.to_string()))?;
            return Ok(CreateEnvironmentOutcome::Replayed(item));
        }
        sqlx::query("LOCK TABLE env_registry_env IN SHARE ROW EXCLUSIVE MODE")
            .execute(&mut *tx)
            .await
            .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?;
        let next: i64 =
            sqlx::query_scalar("SELECT COALESCE(MAX(seq), -1) + 1 FROM env_registry_env")
                .fetch_one(&mut *tx)
                .await
                .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?;
        let id = format!("env_{next:016}");
        sqlx::query(
            "INSERT INTO env_registry_env \
                (env_id, seq, name, description, metadata_json, config_json, scope, sandbox_policy_json, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $9)",
        )
        .bind(&id)
        .bind(next)
        .bind(&command.name)
        .bind(encode_description(&command.description))
        .bind(
            metadata_str(&command.metadata)
                .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?,
        )
        .bind(
            config_str(&command.config)
                .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?,
        )
        .bind(&command.scope)
        .bind(
            sandbox_policy_str(&command.sandbox_policy)
                .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?,
        )
        .bind(&committed_at)
        .execute(&mut *tx)
        .await
        .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?;
        sqlx::query("INSERT INTO env_registry_create_command (command_id, fingerprint, env_id) VALUES ($1, $2, $3)")
            .bind(&command.command_id).bind(&fingerprint).bind(&id)
            .execute(&mut *tx).await.map_err(|error| CreateEnvironmentError::Store(error.to_string()))?;
        let item = EnvItem {
            id: id.clone(),
            revision: EnvironmentRevision(1),
            name: command.name,
            description: command.description,
            metadata: command.metadata,
            scope: command.scope,
            config: command.config,
            sandbox_policy: command.sandbox_policy,
            created_at: Some(committed_at.clone()),
            updated_at: Some(committed_at),
            archived_at: None,
        };
        sqlx::query(
            "INSERT INTO env_registry_revision \
             (env_id, revision, name, description, metadata_json, config_json, archived_at, scope, sandbox_policy_json, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(&item.id)
        .bind(
            i64::try_from(item.revision.0)
                .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?,
        )
        .bind(&item.name)
        .bind(encode_description(&item.description))
        .bind(
            metadata_str(&item.metadata)
                .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?,
        )
        .bind(
            config_str(&item.config)
                .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?,
        )
        .bind(&item.archived_at)
        .bind(&item.scope)
        .bind(
            sandbox_policy_str(&item.sandbox_policy)
                .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?,
        )
        .bind(&item.created_at)
        .bind(&item.updated_at)
        .execute(&mut *tx)
        .await
        .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?;
        sqlx::query(
            "INSERT INTO env_registry_registration_intent \
             (env_id, revision, operation, delivered) VALUES ($1, $2, $3, 0)",
        )
        .bind(&item.id)
        .bind(
            i64::try_from(item.revision.0)
                .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?,
        )
        .bind(operation_str(
            EnvironmentRegistrationIntent::for_item(&item).operation,
        ))
        .execute(&mut *tx)
        .await
        .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?;
        tx.commit()
            .await
            .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?;
        Ok(CreateEnvironmentOutcome::Created(item))
    }

    async fn list_active(&self) -> Result<Vec<EnvItem>, EnvironmentStoreError> {
        sqlx::query(&format!(
            "SELECT {COLS} FROM env_registry_env WHERE archived_at IS NULL ORDER BY seq ASC"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(environment_store)?
        .iter()
        .map(pg_row)
        .collect()
    }

    async fn list_all(&self) -> Result<Vec<EnvItem>, EnvironmentStoreError> {
        sqlx::query(&format!(
            "SELECT {COLS} FROM env_registry_env ORDER BY seq ASC"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(environment_store)?
        .iter()
        .map(pg_row)
        .collect()
    }

    async fn get(&self, id: &str) -> Result<Option<EnvItem>, EnvironmentStoreError> {
        self.read(id).await
    }

    async fn get_revision(
        &self,
        id: &str,
        revision: EnvironmentRevision,
    ) -> Result<Option<EnvItem>, EnvironmentStoreError> {
        let revision = i64::try_from(revision.0).map_err(environment_store)?;
        sqlx::query(&format!(
            "SELECT {COLS} FROM env_registry_revision WHERE env_id = $1 AND revision = $2"
        ))
        .bind(id)
        .bind(revision)
        .fetch_optional(&self.pool)
        .await
        .map_err(environment_store)?
        .map(|row| pg_row(&row))
        .transpose()
    }

    async fn exists(&self, id: &str) -> Result<bool, EnvironmentStoreError> {
        Ok(self.read(id).await?.is_some())
    }

    async fn update(
        &self,
        id: &str,
        expected_revision: EnvironmentRevision,
        patch: EnvUpdate,
    ) -> Result<Option<EnvItem>, EnvironmentStoreError> {
        let mut tx = self.pool.begin().await.map_err(environment_store)?;
        let Some(row) = sqlx::query(&format!(
            "SELECT {COLS} FROM env_registry_env WHERE env_id = $1 FOR UPDATE"
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(environment_store)?
        else {
            return Ok(None);
        };
        let mut item = pg_row(&row)?;
        if item.archived_at.is_some() {
            return Ok(None);
        }
        if item.revision != expected_revision {
            return Err(EnvironmentStoreError::RevisionConflict);
        }
        if !item.apply(patch)? {
            return Ok(Some(item));
        }
        item.updated_at = Some(self.clock.observed_at());
        let revision = i64::try_from(item.revision.0).map_err(environment_store)?;
        sqlx::query(
            "UPDATE env_registry_env SET name = $1, description = $2, metadata_json = $3, \
             config_json = $4, revision = $5, scope = $6, sandbox_policy_json = $7, \
             updated_at = $9 WHERE env_id = $8",
        )
        .bind(&item.name)
        .bind(encode_description(&item.description))
        .bind(metadata_str(&item.metadata)?)
        .bind(config_str(&item.config)?)
        .bind(revision)
        .bind(&item.scope)
        .bind(sandbox_policy_str(&item.sandbox_policy)?)
        .bind(id)
        .bind(&item.updated_at)
        .execute(&mut *tx)
        .await
        .map_err(environment_store)?;
        sqlx::query(
            "INSERT INTO env_registry_revision \
             (env_id, revision, name, description, metadata_json, config_json, archived_at, scope, sandbox_policy_json, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(&item.id)
        .bind(revision)
        .bind(&item.name)
        .bind(encode_description(&item.description))
        .bind(metadata_str(&item.metadata)?)
        .bind(config_str(&item.config)?)
        .bind(&item.archived_at)
        .bind(&item.scope)
        .bind(sandbox_policy_str(&item.sandbox_policy)?)
        .bind(&item.created_at)
        .bind(&item.updated_at)
        .execute(&mut *tx)
        .await
        .map_err(environment_store)?;
        sqlx::query(
            "INSERT INTO env_registry_registration_intent \
             (env_id, revision, operation, delivered) VALUES ($1, $2, $3, 0)",
        )
        .bind(&item.id)
        .bind(revision)
        .bind(operation_str(
            EnvironmentRegistrationIntent::for_item(&item).operation,
        ))
        .execute(&mut *tx)
        .await
        .map_err(environment_store)?;
        tx.commit().await.map_err(environment_store)?;
        Ok(Some(item))
    }

    async fn archive(&self, id: &str) -> Result<Option<EnvItem>, EnvironmentStoreError> {
        let mut tx = self.pool.begin().await.map_err(environment_store)?;
        let Some(row) = sqlx::query(&format!(
            "SELECT {COLS} FROM env_registry_env WHERE env_id = $1 FOR UPDATE"
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(environment_store)?
        else {
            return Ok(None);
        };
        let mut item = pg_row(&row)?;
        if item.archived_at.is_some() {
            tx.commit().await.map_err(environment_store)?;
            return Ok(Some(item));
        }
        let committed_at = self.clock.observed_at();
        item.archived_at = Some(committed_at.clone());
        item.updated_at = Some(committed_at);
        item.revision = EnvironmentRevision(item.revision.0.checked_add(1).ok_or_else(|| {
            EnvironmentStoreError::Backend("Environment revision exhausted".into())
        })?);
        let revision = i64::try_from(item.revision.0).map_err(environment_store)?;
        sqlx::query(
            "UPDATE env_registry_env SET archived_at = $1, revision = $2, updated_at = $1 \
             WHERE env_id = $3",
        )
        .bind(&item.archived_at)
        .bind(revision)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(environment_store)?;
        sqlx::query(
            "INSERT INTO env_registry_revision \
             (env_id, revision, name, description, metadata_json, config_json, archived_at, scope, sandbox_policy_json, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(&item.id)
        .bind(revision)
        .bind(&item.name)
        .bind(encode_description(&item.description))
        .bind(metadata_str(&item.metadata)?)
        .bind(config_str(&item.config)?)
        .bind(&item.archived_at)
        .bind(&item.scope)
        .bind(sandbox_policy_str(&item.sandbox_policy)?)
        .bind(&item.created_at)
        .bind(&item.updated_at)
        .execute(&mut *tx)
        .await
        .map_err(environment_store)?;
        sqlx::query(
            "INSERT INTO env_registry_registration_intent \
             (env_id, revision, operation, delivered) VALUES ($1, $2, $3, 0)",
        )
        .bind(&item.id)
        .bind(revision)
        .bind(operation_str(
            EnvironmentRegistrationIntent::for_item(&item).operation,
        ))
        .execute(&mut *tx)
        .await
        .map_err(environment_store)?;
        tx.commit().await.map_err(environment_store)?;
        Ok(Some(item))
    }

    async fn registration_intent(
        &self,
        id: &str,
        revision: EnvironmentRevision,
    ) -> Result<Option<EnvironmentRegistrationIntent>, EnvironmentStoreError> {
        let revision_value = i64::try_from(revision.0).map_err(environment_store)?;
        let row = sqlx::query(
            "SELECT operation, delivered FROM env_registry_registration_intent \
             WHERE env_id = $1 AND revision = $2",
        )
        .bind(id)
        .bind(revision_value)
        .fetch_optional(&self.pool)
        .await
        .map_err(environment_store)?;
        row.map(|row| {
            Ok(EnvironmentRegistrationIntent {
                environment_id: id.to_string(),
                revision,
                operation: parse_operation(row.get::<String, _>("operation").as_str())
                    .map_err(EnvironmentStoreError::Backend)?,
                delivered: row.get::<i64, _>("delivered") != 0,
            })
        })
        .transpose()
    }

    async fn registration_intents(
        &self,
        filter: EnvironmentRegistrationIntentFilter,
    ) -> Result<Vec<EnvironmentRegistrationIntent>, EnvironmentStoreError> {
        let query = match filter {
            EnvironmentRegistrationIntentFilter::Pending => {
                "SELECT env_id, revision, operation, delivered \
                 FROM env_registry_registration_intent WHERE delivered = 0 ORDER BY env_id, revision"
            }
            EnvironmentRegistrationIntentFilter::All => {
                "SELECT env_id, revision, operation, delivered \
                 FROM env_registry_registration_intent ORDER BY env_id, revision"
            }
        };
        sqlx::query(query)
            .fetch_all(&self.pool)
            .await
            .map_err(environment_store)?
            .into_iter()
            .map(|row| {
                Ok(EnvironmentRegistrationIntent {
                    environment_id: row.get("env_id"),
                    revision: EnvironmentRevision(
                        u64::try_from(row.get::<i64, _>("revision")).map_err(|_| {
                            EnvironmentStoreError::Backend(
                                "invalid Environment registration revision".into(),
                            )
                        })?,
                    ),
                    operation: parse_operation(row.get::<String, _>("operation").as_str())
                        .map_err(EnvironmentStoreError::Backend)?,
                    delivered: row.get::<i64, _>("delivered") != 0,
                })
            })
            .collect()
    }

    async fn mark_registration_intent_delivered(
        &self,
        intent: &EnvironmentRegistrationIntent,
    ) -> Result<bool, EnvironmentStoreError> {
        let revision = i64::try_from(intent.revision.0).map_err(environment_store)?;
        let result = sqlx::query(
            "UPDATE env_registry_registration_intent SET delivered = 1 \
             WHERE env_id = $1 AND revision = $2 AND operation = $3",
        )
        .bind(&intent.environment_id)
        .bind(revision)
        .bind(operation_str(intent.operation))
        .execute(&self.pool)
        .await
        .map_err(environment_store)?;
        Ok(result.rows_affected() == 1)
    }
}
