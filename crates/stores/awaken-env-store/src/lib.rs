//! Durable Control-owned [`EnvRegistry`] backends. Environment definitions and
//! exact revision history survive restart; Coordinator and Worker see only the
//! separately registered executable projection. SQLite and PostgreSQL share one
//! portable migration bundle.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_environment_contract::{
    CreateEnvironmentCommand, CreateEnvironmentError, CreateEnvironmentOutcome, EnvItem,
    EnvRegistry, EnvUpdate, EnvironmentConfig, EnvironmentRegistrationIntent,
    EnvironmentRegistrationIntentFilter, EnvironmentRegistrationOperation, EnvironmentRevision,
    EnvironmentSandboxPolicyRef,
};
use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sqlx::Row;
use sqlx::postgres::{PgPool, PgRow};

// The test-support reference backend lives here beside the durable siblings; the
// port and value objects stay inward in `awaken-environment-contract`.
#[cfg(any(test, feature = "test-support"))]
mod inmem;
#[cfg(any(test, feature = "test-support"))]
pub use inmem::InMemoryEnvRegistry;

/// The frozen presence timestamp the managed wire uses (parity with the registry).
const OBJECT_AT: &str = "2026-01-01T00:00:00Z";
/// The store's table namespace / bundle prefix (`env_registry_env`).
const NS: &str = "env_registry";

fn env_bundle() -> Result<MigrationBundle, MigrationError> {
    MigrationBundle::new(
        "awaken.env_registry",
        vec![
            Migration::new(
                1,
                "self-hosted environment registry: one row per environment",
                "CREATE TABLE {prefix}_env (\
             env_id        TEXT PRIMARY KEY, \
             seq           BIGINT NOT NULL, \
             name          TEXT NOT NULL, \
             description   TEXT NOT NULL, \
             metadata_json TEXT NOT NULL, \
             config_json   TEXT NOT NULL, \
             archived_at   TEXT)",
            )?,
            Migration::new(
                2,
                "monotonic environment revision",
                "ALTER TABLE {prefix}_env ADD COLUMN revision BIGINT NOT NULL DEFAULT 1",
            )?,
            Migration::new(
                3,
                "Anthropic Environment visibility scope",
                "ALTER TABLE {prefix}_env ADD COLUMN scope TEXT",
            )?,
            Migration::new(
                4,
                "idempotent Environment create commands",
                "CREATE TABLE {prefix}_create_command (\
                 command_id  TEXT PRIMARY KEY, \
                 fingerprint TEXT NOT NULL, \
                 env_id      TEXT NOT NULL UNIQUE REFERENCES {prefix}_env(env_id) ON DELETE CASCADE)",
            )?,
            Migration::new(
                5,
                "immutable authored Environment revision history",
                "CREATE TABLE {prefix}_revision (\
                 env_id        TEXT NOT NULL, \
                 revision      BIGINT NOT NULL, \
                 name          TEXT NOT NULL, \
                 description   TEXT NOT NULL, \
                 metadata_json TEXT NOT NULL, \
                 config_json   TEXT NOT NULL, \
                 archived_at   TEXT, \
                 scope         TEXT, \
                 PRIMARY KEY (env_id, revision))",
            )?,
            Migration::new(
                6,
                "seed Environment revision history from the current projection",
                "INSERT INTO {prefix}_revision \
                 (env_id, revision, name, description, metadata_json, config_json, archived_at, scope) \
                 SELECT env_id, revision, name, description, metadata_json, config_json, archived_at, scope \
                 FROM {prefix}_env",
            )?,
            Migration::new(
                7,
                "exact sandbox policy binding on the current Environment projection",
                "ALTER TABLE {prefix}_env ADD COLUMN sandbox_policy_json TEXT",
            )?,
            Migration::new(
                8,
                "exact sandbox policy binding in immutable Environment history",
                "ALTER TABLE {prefix}_revision ADD COLUMN sandbox_policy_json TEXT",
            )?,
            Migration::new(
                9,
                "transactional executable Environment registration outbox",
                "CREATE TABLE {prefix}_registration_intent (\
                 env_id    TEXT NOT NULL, \
                 revision  BIGINT NOT NULL, \
                 operation TEXT NOT NULL, \
                 delivered BIGINT NOT NULL DEFAULT 0, \
                 PRIMARY KEY (env_id, revision), \
                 FOREIGN KEY (env_id, revision) REFERENCES {prefix}_revision(env_id, revision))",
            )?,
            Migration::new(
                10,
                "seed registration intent log from immutable Environment history",
                "INSERT INTO {prefix}_registration_intent (env_id, revision, operation, delivered) \
                 SELECT env_id, revision, \
                 CASE WHEN archived_at IS NULL THEN 'register' ELSE 'withdraw' END, 0 \
                 FROM {prefix}_revision",
            )?,
        ],
    )
}

/// The columns an env row projects to an [`EnvItem`], in `SELECT` order.
const COLS: &str = "env_id, name, description, metadata_json, config_json, archived_at, revision, scope, sandbox_policy_json";

fn metadata_str(m: &BTreeMap<String, String>) -> String {
    serde_json::to_string(m).expect("env metadata serializes")
}

fn config_str(c: &EnvironmentConfig) -> String {
    serde_json::to_string(c).expect("env config serializes")
}

fn sandbox_policy_str(reference: &Option<EnvironmentSandboxPolicyRef>) -> Option<String> {
    reference
        .as_ref()
        .map(|reference| serde_json::to_string(reference).expect("sandbox policy ref serializes"))
}

fn operation_str(operation: EnvironmentRegistrationOperation) -> &'static str {
    match operation {
        EnvironmentRegistrationOperation::Register => "register",
        EnvironmentRegistrationOperation::Withdraw => "withdraw",
    }
}

fn parse_operation(value: &str) -> Result<EnvironmentRegistrationOperation, String> {
    match value {
        "register" => Ok(EnvironmentRegistrationOperation::Register),
        "withdraw" => Ok(EnvironmentRegistrationOperation::Withdraw),
        other => Err(format!(
            "invalid Environment registration operation: {other}"
        )),
    }
}

/// Storage-shaped row shared by the SQLite and PostgreSQL adapters. Keeping the
/// row-to-domain translation here prevents either backend from becoming a second
/// interpretation of persisted Environment facts.
struct PersistedEnvRow {
    id: String,
    name: String,
    description: String,
    metadata_json: String,
    config_json: String,
    archived_at: Option<String>,
    revision: i64,
    scope: Option<String>,
    sandbox_policy_json: Option<String>,
}

impl PersistedEnvRow {
    fn into_item(self) -> EnvItem {
        EnvItem {
            id: self.id,
            revision: EnvironmentRevision(
                u64::try_from(self.revision).expect("valid Environment revision"),
            ),
            name: self.name,
            description: self.description,
            metadata: serde_json::from_str(&self.metadata_json).unwrap_or_default(),
            scope: self.scope,
            config: serde_json::from_str(&self.config_json)
                .expect("valid typed Environment config"),
            sandbox_policy: self
                .sandbox_policy_json
                .map(|json| serde_json::from_str(&json).expect("valid sandbox policy ref")),
            archived_at: self.archived_at,
        }
    }
}

fn sqlite_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EnvItem> {
    Ok(PersistedEnvRow {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        metadata_json: row.get(3)?,
        config_json: row.get(4)?,
        archived_at: row.get(5)?,
        revision: row.get(6)?,
        scope: row.get(7)?,
        sandbox_policy_json: row.get(8)?,
    }
    .into_item())
}

fn pg_row(row: &PgRow) -> EnvItem {
    PersistedEnvRow {
        id: row.get("env_id"),
        name: row.get("name"),
        description: row.get("description"),
        metadata_json: row.get("metadata_json"),
        config_json: row.get("config_json"),
        archived_at: row.get("archived_at"),
        revision: row.get("revision"),
        scope: row.get("scope"),
        sandbox_policy_json: row.get("sandbox_policy_json"),
    }
    .into_item()
}

/// SQLite persistence for the environment registry.
pub struct SqliteEnvRegistry {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteEnvRegistry {
    pub fn open(path: &str) -> Result<Self, String> {
        Self::from_connection(Connection::open(path).map_err(|e| e.to_string())?)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn open_in_memory() -> Result<Self, String> {
        Self::from_connection(Connection::open_in_memory().map_err(|e| e.to_string())?)
    }

    fn from_connection(conn: Connection) -> Result<Self, String> {
        let bundle = env_bundle().map_err(|e| e.to_string())?;
        awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
            .map_err(|e| e.to_string())?
            .run_bundle(&conn, &bundle)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn read(tx: &Transaction<'_>, id: &str) -> Option<EnvItem> {
        tx.query_row(
            &format!("SELECT {COLS} FROM env_registry_env WHERE env_id = ?1"),
            params![id],
            sqlite_row,
        )
        .optional()
        .expect("read env row")
    }

    fn insert_revision(tx: &Transaction<'_>, item: &EnvItem) -> Result<(), rusqlite::Error> {
        tx.execute(
            "INSERT INTO env_registry_revision \
             (env_id, revision, name, description, metadata_json, config_json, archived_at, scope, sandbox_policy_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                item.id,
                item.revision.0,
                item.name,
                item.description,
                metadata_str(&item.metadata),
                config_str(&item.config),
                item.archived_at,
                item.scope,
                sandbox_policy_str(&item.sandbox_policy),
            ],
        )?;
        Ok(())
    }

    fn insert_registration_intent(
        tx: &Transaction<'_>,
        item: &EnvItem,
    ) -> Result<(), rusqlite::Error> {
        let intent = EnvironmentRegistrationIntent::for_item(item);
        tx.execute(
            "INSERT INTO env_registry_registration_intent \
             (env_id, revision, operation, delivered) VALUES (?1, ?2, ?3, 0)",
            params![
                intent.environment_id,
                intent.revision.0,
                operation_str(intent.operation),
            ],
        )?;
        Ok(())
    }
}

#[async_trait]
impl EnvRegistry for SqliteEnvRegistry {
    async fn create_once(
        &self,
        command: CreateEnvironmentCommand,
    ) -> Result<CreateEnvironmentOutcome, CreateEnvironmentError> {
        let mut guard = self.conn.lock().expect("env registry mutex poisoned");
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?;
        let fingerprint = command.fingerprint();
        let replay: Option<(String, String)> = tx
            .query_row(
                "SELECT fingerprint, env_id FROM env_registry_create_command WHERE command_id = ?1",
                params![command.command_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?;
        if let Some((existing_fingerprint, environment_id)) = replay {
            if existing_fingerprint != fingerprint {
                return Err(CreateEnvironmentError::IdempotencyConflict);
            }
            let item = Self::read(&tx, &environment_id)
                .ok_or_else(|| CreateEnvironmentError::Store("command target is missing".into()))?;
            return Ok(CreateEnvironmentOutcome::Replayed(item));
        }
        let next: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq), -1) + 1 FROM env_registry_env",
                [],
                |r| r.get(0),
            )
            .expect("next seq");
        let id = format!("env_{next:016}");
        tx.execute(
            "INSERT INTO env_registry_env \
                (env_id, seq, name, description, metadata_json, config_json, scope) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                next,
                command.name,
                command.description,
                metadata_str(&command.metadata),
                config_str(&command.config),
                command.scope
            ],
        )
        .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?;
        tx.execute(
            "INSERT INTO env_registry_create_command (command_id, fingerprint, env_id) VALUES (?1, ?2, ?3)",
            params![command.command_id, fingerprint, id],
        )
        .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?;
        let item = EnvItem {
            id: id.clone(),
            revision: EnvironmentRevision(1),
            name: command.name,
            description: command.description,
            metadata: command.metadata,
            scope: command.scope,
            config: command.config,
            sandbox_policy: None,
            archived_at: None,
        };
        Self::insert_revision(&tx, &item)
            .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?;
        Self::insert_registration_intent(&tx, &item)
            .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?;
        tx.commit()
            .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?;
        Ok(CreateEnvironmentOutcome::Created(item))
    }

    async fn list_active(&self) -> Vec<EnvItem> {
        let conn = self.conn.lock().expect("env registry mutex poisoned");
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {COLS} FROM env_registry_env WHERE archived_at IS NULL ORDER BY seq ASC"
            ))
            .expect("prepare list");
        let rows = stmt.query_map([], sqlite_row).expect("query list");
        rows.map(|r| r.expect("row")).collect()
    }

    async fn list_all(&self) -> Vec<EnvItem> {
        let conn = self.conn.lock().expect("env registry mutex poisoned");
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {COLS} FROM env_registry_env ORDER BY seq ASC"
            ))
            .expect("prepare list all");
        stmt.query_map([], sqlite_row)
            .expect("query list all")
            .map(|row| row.expect("row"))
            .collect()
    }

    async fn get(&self, id: &str) -> Option<EnvItem> {
        let mut guard = self.conn.lock().expect("env registry mutex poisoned");
        let tx = guard.transaction().expect("begin");
        Self::read(&tx, id)
    }

    async fn get_revision(&self, id: &str, revision: EnvironmentRevision) -> Option<EnvItem> {
        let conn = self.conn.lock().expect("env registry mutex poisoned");
        conn.query_row(
            &format!(
                "SELECT {COLS} FROM env_registry_revision WHERE env_id = ?1 AND revision = ?2"
            ),
            params![id, revision.0],
            sqlite_row,
        )
        .optional()
        .expect("read Environment revision")
    }

    async fn exists(&self, id: &str) -> bool {
        self.get(id).await.is_some()
    }

    async fn update(&self, id: &str, patch: EnvUpdate) -> Option<EnvItem> {
        let mut guard = self.conn.lock().expect("env registry mutex poisoned");
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin immediate");
        let mut item = Self::read(&tx, id)?;
        if item.archived_at.is_some() {
            return None;
        }
        if !item.apply(patch) {
            return Some(item);
        }
        tx.execute(
            "UPDATE env_registry_env SET name = ?1, description = ?2, metadata_json = ?3, \
             config_json = ?4, revision = ?5, scope = ?6, sandbox_policy_json = ?7 WHERE env_id = ?8",
            params![
                item.name,
                item.description,
                metadata_str(&item.metadata),
                config_str(&item.config),
                item.revision.0,
                item.scope,
                sandbox_policy_str(&item.sandbox_policy),
                id
            ],
        )
        .expect("update env");
        Self::insert_revision(&tx, &item).expect("insert Environment revision");
        Self::insert_registration_intent(&tx, &item)
            .expect("insert Environment registration intent");
        tx.commit().expect("commit update");
        Some(item)
    }

    async fn archive(&self, id: &str) -> Option<EnvItem> {
        let mut guard = self.conn.lock().expect("env registry mutex poisoned");
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin immediate");
        let mut item = Self::read(&tx, id)?;
        if item.archived_at.is_some() {
            return Some(item);
        }
        item.archived_at = Some(OBJECT_AT.to_string());
        item.revision = EnvironmentRevision(item.revision.0.checked_add(1).expect("revision"));
        tx.execute(
            "UPDATE env_registry_env SET archived_at = ?1, revision = ?2 WHERE env_id = ?3",
            params![OBJECT_AT, item.revision.0, id],
        )
        .expect("archive env");
        Self::insert_revision(&tx, &item).expect("insert archived Environment revision");
        Self::insert_registration_intent(&tx, &item).expect("insert Environment withdrawal intent");
        tx.commit().expect("commit archive");
        Some(item)
    }

    async fn registration_intent(
        &self,
        id: &str,
        revision: EnvironmentRevision,
    ) -> Result<Option<EnvironmentRegistrationIntent>, String> {
        let conn = self.conn.lock().expect("env registry mutex poisoned");
        let row = conn
            .query_row(
                "SELECT operation, delivered FROM env_registry_registration_intent \
                 WHERE env_id = ?1 AND revision = ?2",
                params![id, revision.0],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        row.map(|(operation, delivered)| {
            Ok(EnvironmentRegistrationIntent {
                environment_id: id.to_string(),
                revision,
                operation: parse_operation(&operation)?,
                delivered: delivered != 0,
            })
        })
        .transpose()
    }

    async fn registration_intents(
        &self,
        filter: EnvironmentRegistrationIntentFilter,
    ) -> Result<Vec<EnvironmentRegistrationIntent>, String> {
        let conn = self.conn.lock().expect("env registry mutex poisoned");
        let where_clause = match filter {
            EnvironmentRegistrationIntentFilter::Pending => " WHERE delivered = 0",
            EnvironmentRegistrationIntentFilter::All => "",
        };
        let mut statement = conn
            .prepare(&format!(
                "SELECT env_id, revision, operation, delivered \
                 FROM env_registry_registration_intent{where_clause} ORDER BY env_id, revision"
            ))
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(|error| error.to_string())?;
        rows.map(|row| {
            let (environment_id, revision, operation, delivered) =
                row.map_err(|error| error.to_string())?;
            Ok(EnvironmentRegistrationIntent {
                environment_id,
                revision: EnvironmentRevision(
                    u64::try_from(revision)
                        .map_err(|_| "invalid Environment registration revision".to_string())?,
                ),
                operation: parse_operation(&operation)?,
                delivered: delivered != 0,
            })
        })
        .collect()
    }

    async fn mark_registration_intent_delivered(
        &self,
        intent: &EnvironmentRegistrationIntent,
    ) -> Result<bool, String> {
        let conn = self.conn.lock().expect("env registry mutex poisoned");
        let changed = conn
            .execute(
                "UPDATE env_registry_registration_intent SET delivered = 1 \
                 WHERE env_id = ?1 AND revision = ?2 AND operation = ?3",
                params![
                    intent.environment_id,
                    intent.revision.0,
                    operation_str(intent.operation),
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(changed == 1)
    }
}

/// Postgres persistence for the environment registry (distributed deployments).
pub struct PostgresEnvRegistry {
    pool: PgPool,
}

impl PostgresEnvRegistry {
    pub async fn connect(url: &str) -> Result<Self, String> {
        let pool = PgPool::connect(url).await.map_err(|e| e.to_string())?;
        Self::with_pool(pool).await
    }

    pub async fn with_pool(pool: PgPool) -> Result<Self, String> {
        let bundle = env_bundle().map_err(|e| e.to_string())?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|e| e.to_string())?
            .run_bundle(&bundle)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Self { pool })
    }

    pub async fn connect_existing(url: &str) -> Result<Self, String> {
        let pool = PgPool::connect(url).await.map_err(|e| e.to_string())?;
        let bundle = env_bundle().map_err(|e| e.to_string())?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|e| e.to_string())?
            .verify_bundle(&bundle)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Self { pool })
    }

    async fn read(&self, id: &str) -> Option<EnvItem> {
        sqlx::query(&format!(
            "SELECT {COLS} FROM env_registry_env WHERE env_id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .expect("read env row")
        .map(|r| pg_row(&r))
    }
}

#[async_trait]
impl EnvRegistry for PostgresEnvRegistry {
    async fn create_once(
        &self,
        command: CreateEnvironmentCommand,
    ) -> Result<CreateEnvironmentOutcome, CreateEnvironmentError> {
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
            .map(|row| pg_row(&row))
            .ok_or_else(|| CreateEnvironmentError::Store("command target is missing".into()))?;
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
                .expect("next seq");
        let id = format!("env_{next:016}");
        sqlx::query(
            "INSERT INTO env_registry_env \
                (env_id, seq, name, description, metadata_json, config_json, scope) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&id)
        .bind(next)
        .bind(&command.name)
        .bind(&command.description)
        .bind(metadata_str(&command.metadata))
        .bind(config_str(&command.config))
        .bind(&command.scope)
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
            sandbox_policy: None,
            archived_at: None,
        };
        sqlx::query(
            "INSERT INTO env_registry_revision \
             (env_id, revision, name, description, metadata_json, config_json, archived_at, scope, sandbox_policy_json) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&item.id)
        .bind(i64::try_from(item.revision.0).expect("Environment revision fits i64"))
        .bind(&item.name)
        .bind(&item.description)
        .bind(metadata_str(&item.metadata))
        .bind(config_str(&item.config))
        .bind(&item.archived_at)
        .bind(&item.scope)
        .bind(sandbox_policy_str(&item.sandbox_policy))
        .execute(&mut *tx)
        .await
        .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?;
        sqlx::query(
            "INSERT INTO env_registry_registration_intent \
             (env_id, revision, operation, delivered) VALUES ($1, $2, $3, 0)",
        )
        .bind(&item.id)
        .bind(i64::try_from(item.revision.0).expect("Environment revision fits i64"))
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

    async fn list_active(&self) -> Vec<EnvItem> {
        sqlx::query(&format!(
            "SELECT {COLS} FROM env_registry_env WHERE archived_at IS NULL ORDER BY seq ASC"
        ))
        .fetch_all(&self.pool)
        .await
        .expect("query list")
        .iter()
        .map(pg_row)
        .collect()
    }

    async fn list_all(&self) -> Vec<EnvItem> {
        sqlx::query(&format!(
            "SELECT {COLS} FROM env_registry_env ORDER BY seq ASC"
        ))
        .fetch_all(&self.pool)
        .await
        .expect("query all Environments")
        .iter()
        .map(pg_row)
        .collect()
    }

    async fn get(&self, id: &str) -> Option<EnvItem> {
        self.read(id).await
    }

    async fn get_revision(&self, id: &str, revision: EnvironmentRevision) -> Option<EnvItem> {
        sqlx::query(&format!(
            "SELECT {COLS} FROM env_registry_revision WHERE env_id = $1 AND revision = $2"
        ))
        .bind(id)
        .bind(i64::try_from(revision.0).expect("Environment revision fits i64"))
        .fetch_optional(&self.pool)
        .await
        .expect("read Environment revision")
        .map(|row| pg_row(&row))
    }

    async fn exists(&self, id: &str) -> bool {
        self.read(id).await.is_some()
    }

    async fn update(&self, id: &str, patch: EnvUpdate) -> Option<EnvItem> {
        let mut tx = self.pool.begin().await.expect("begin Environment update");
        let row = sqlx::query(&format!(
            "SELECT {COLS} FROM env_registry_env WHERE env_id = $1 FOR UPDATE"
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .expect("lock env row")?;
        let mut item = pg_row(&row);
        if item.archived_at.is_some() {
            return None;
        }
        if !item.apply(patch) {
            return Some(item);
        }
        sqlx::query(
            "UPDATE env_registry_env SET name = $1, description = $2, metadata_json = $3, \
             config_json = $4, revision = $5, scope = $6, sandbox_policy_json = $7 WHERE env_id = $8",
        )
        .bind(&item.name)
        .bind(&item.description)
        .bind(metadata_str(&item.metadata))
        .bind(config_str(&item.config))
        .bind(i64::try_from(item.revision.0).expect("Environment revision fits i64"))
        .bind(&item.scope)
        .bind(sandbox_policy_str(&item.sandbox_policy))
        .bind(id)
        .execute(&mut *tx)
        .await
        .expect("update env");
        sqlx::query(
            "INSERT INTO env_registry_revision \
             (env_id, revision, name, description, metadata_json, config_json, archived_at, scope, sandbox_policy_json) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&item.id)
        .bind(i64::try_from(item.revision.0).expect("Environment revision fits i64"))
        .bind(&item.name)
        .bind(&item.description)
        .bind(metadata_str(&item.metadata))
        .bind(config_str(&item.config))
        .bind(&item.archived_at)
        .bind(&item.scope)
        .bind(sandbox_policy_str(&item.sandbox_policy))
        .execute(&mut *tx)
        .await
        .expect("insert Environment revision");
        sqlx::query(
            "INSERT INTO env_registry_registration_intent \
             (env_id, revision, operation, delivered) VALUES ($1, $2, $3, 0)",
        )
        .bind(&item.id)
        .bind(i64::try_from(item.revision.0).expect("Environment revision fits i64"))
        .bind(operation_str(
            EnvironmentRegistrationIntent::for_item(&item).operation,
        ))
        .execute(&mut *tx)
        .await
        .expect("insert Environment registration intent");
        tx.commit().await.expect("commit Environment update");
        Some(item)
    }

    async fn archive(&self, id: &str) -> Option<EnvItem> {
        let mut tx = self.pool.begin().await.expect("begin Environment archive");
        let row = sqlx::query(&format!(
            "SELECT {COLS} FROM env_registry_env WHERE env_id = $1 FOR UPDATE"
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .expect("lock env row")?;
        let mut item = pg_row(&row);
        if item.archived_at.is_some() {
            tx.commit()
                .await
                .expect("commit idempotent Environment archive");
            return Some(item);
        }
        item.archived_at = Some(OBJECT_AT.to_string());
        item.revision = EnvironmentRevision(item.revision.0.checked_add(1).expect("revision"));
        sqlx::query(
            "UPDATE env_registry_env SET archived_at = $1, revision = $2 WHERE env_id = $3",
        )
        .bind(OBJECT_AT)
        .bind(i64::try_from(item.revision.0).expect("Environment revision fits i64"))
        .bind(id)
        .execute(&mut *tx)
        .await
        .expect("archive env");
        sqlx::query(
            "INSERT INTO env_registry_revision \
             (env_id, revision, name, description, metadata_json, config_json, archived_at, scope, sandbox_policy_json) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&item.id)
        .bind(i64::try_from(item.revision.0).expect("Environment revision fits i64"))
        .bind(&item.name)
        .bind(&item.description)
        .bind(metadata_str(&item.metadata))
        .bind(config_str(&item.config))
        .bind(&item.archived_at)
        .bind(&item.scope)
        .bind(sandbox_policy_str(&item.sandbox_policy))
        .execute(&mut *tx)
        .await
        .expect("insert archived Environment revision");
        sqlx::query(
            "INSERT INTO env_registry_registration_intent \
             (env_id, revision, operation, delivered) VALUES ($1, $2, $3, 0)",
        )
        .bind(&item.id)
        .bind(i64::try_from(item.revision.0).expect("Environment revision fits i64"))
        .bind(operation_str(
            EnvironmentRegistrationIntent::for_item(&item).operation,
        ))
        .execute(&mut *tx)
        .await
        .expect("insert Environment withdrawal intent");
        tx.commit().await.expect("commit Environment archive");
        Some(item)
    }

    async fn registration_intent(
        &self,
        id: &str,
        revision: EnvironmentRevision,
    ) -> Result<Option<EnvironmentRegistrationIntent>, String> {
        let row = sqlx::query(
            "SELECT operation, delivered FROM env_registry_registration_intent \
             WHERE env_id = $1 AND revision = $2",
        )
        .bind(id)
        .bind(i64::try_from(revision.0).expect("Environment revision fits i64"))
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| error.to_string())?;
        row.map(|row| {
            Ok(EnvironmentRegistrationIntent {
                environment_id: id.to_string(),
                revision,
                operation: parse_operation(row.get::<String, _>("operation").as_str())?,
                delivered: row.get::<i64, _>("delivered") != 0,
            })
        })
        .transpose()
    }

    async fn registration_intents(
        &self,
        filter: EnvironmentRegistrationIntentFilter,
    ) -> Result<Vec<EnvironmentRegistrationIntent>, String> {
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
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|row| {
                Ok(EnvironmentRegistrationIntent {
                    environment_id: row.get("env_id"),
                    revision: EnvironmentRevision(
                        u64::try_from(row.get::<i64, _>("revision"))
                            .map_err(|_| "invalid Environment registration revision".to_string())?,
                    ),
                    operation: parse_operation(row.get::<String, _>("operation").as_str())?,
                    delivered: row.get::<i64, _>("delivered") != 0,
                })
            })
            .collect()
    }

    async fn mark_registration_intent_delivered(
        &self,
        intent: &EnvironmentRegistrationIntent,
    ) -> Result<bool, String> {
        let result = sqlx::query(
            "UPDATE env_registry_registration_intent SET delivered = 1 \
             WHERE env_id = $1 AND revision = $2 AND operation = $3",
        )
        .bind(&intent.environment_id)
        .bind(i64::try_from(intent.revision.0).expect("Environment revision fits i64"))
        .bind(operation_str(intent.operation))
        .execute(&self.pool)
        .await
        .map_err(|error| error.to_string())?;
        Ok(result.rows_affected() == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_schema_is_versioned_and_unconditional() {
        // Cause/effect rule: every Environment schema change is a positive,
        // checksum-tracked migration; conditional DDL or conflict-ignore SQL is
        // rejected before any adapter can apply it.
        let bundle = env_bundle().expect("Environment bundle");
        awaken_scoped_migration::lint(std::slice::from_ref(&bundle))
            .expect("deterministic Environment migrations");
    }

    #[tokio::test]
    async fn migration_from_v8_seeds_exact_registration_history_once() {
        // FMECA: F1 upgrading an existing authority leaves old revisions without
        // intents (S9/O4/D7, RPN252); F2 the archived current row is expanded into
        // registrations instead of its terminal withdrawal (S10/O2/D5, RPN100);
        // F3 a restart duplicates seeded work (S6/O3/D3, RPN54). Mitigation is the
        // checksum-ledgered V9 table plus V10 history seed.
        // Cause/effect decision table:
        // | Rule | legacy history | terminal | migration run | effect |
        // | M1 | v1,v2 | no  | first  | two pending Register intents |
        // | M2 | v1..v3 | yes | first  | v3 is pending Withdraw |
        // | M3 | v1..v3 | yes | repeat | exactly the same three intents |
        let connection = Connection::open_in_memory().unwrap();
        let full = env_bundle().unwrap();
        let legacy =
            MigrationBundle::new(full.bundle_id(), full.migrations()[..8].to_vec()).unwrap();
        let runner =
            awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS).unwrap();
        runner.run_bundle(&connection, &legacy).unwrap();
        connection
            .execute_batch(
                r#"
                INSERT INTO env_registry_env
                    (env_id, seq, name, description, metadata_json, config_json,
                     archived_at, revision, scope, sandbox_policy_json)
                VALUES ('env_legacy', 0, 'legacy', '', '{}', '{"type":"self_hosted"}',
                        '2026-01-01T00:00:00Z', 3, NULL, NULL);
                INSERT INTO env_registry_revision
                    (env_id, revision, name, description, metadata_json, config_json,
                     archived_at, scope, sandbox_policy_json)
                VALUES
                    ('env_legacy', 1, 'v1', '', '{}', '{"type":"self_hosted"}', NULL, NULL, NULL),
                    ('env_legacy', 2, 'v2', '', '{}', '{"type":"self_hosted"}', NULL, NULL, NULL),
                    ('env_legacy', 3, 'v3', '', '{}', '{"type":"self_hosted"}',
                     '2026-01-01T00:00:00Z', NULL, NULL);
                "#,
            )
            .unwrap();
        let applied = runner.run_bundle(&connection, &full).unwrap();
        assert_eq!(
            applied
                .iter()
                .map(|migration| migration.version)
                .collect::<Vec<_>>(),
            [9, 10],
            "M1/M2"
        );
        assert!(
            runner.run_bundle(&connection, &full).unwrap().is_empty(),
            "M3"
        );

        let registry = SqliteEnvRegistry::from_connection(connection).unwrap();
        let intents = registry
            .registration_intents(EnvironmentRegistrationIntentFilter::All)
            .await
            .unwrap();
        assert_eq!(intents.len(), 3, "M3");
        assert!(intents.iter().all(|intent| !intent.delivered), "M1-M3");
        assert_eq!(
            intents[0].operation,
            EnvironmentRegistrationOperation::Register,
            "M1"
        );
        assert_eq!(
            intents[1].operation,
            EnvironmentRegistrationOperation::Register,
            "M1"
        );
        assert_eq!(
            intents[2].operation,
            EnvironmentRegistrationOperation::Withdraw,
            "M2"
        );
    }

    #[tokio::test]
    async fn failed_intent_insert_rolls_back_the_whole_create_transaction() {
        // FMECA: the intent insert fails after current/revision/command writes
        // (S10/O3/D8, RPN240). Cause C1=injected final insert failure must imply
        // E1=typed store error and E2=zero current, revision, command, and intent
        // rows. This is the transaction-rollback rule the success conformance
        // cannot prove merely by observing paired rows.
        let registry = r();
        registry
            .conn
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER env_registry_fail_intent BEFORE INSERT \
                 ON env_registry_registration_intent BEGIN \
                 SELECT RAISE(ABORT, 'injected intent failure'); END;",
            )
            .unwrap();
        let result = registry
            .create_once(CreateEnvironmentCommand {
                command_id: "rollback-create".into(),
                name: "rollback".into(),
                description: String::new(),
                metadata: Default::default(),
                scope: None,
                config: config(),
            })
            .await;
        assert!(
            matches!(result, Err(CreateEnvironmentError::Store(_))),
            "E1"
        );
        let connection = registry.conn.lock().unwrap();
        for (rule, table) in [
            ("current", "env_registry_env"),
            ("revision", "env_registry_revision"),
            ("command", "env_registry_create_command"),
            ("intent", "env_registry_registration_intent"),
        ] {
            let count: i64 = connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "E2/{rule}");
        }
    }

    fn config() -> EnvironmentConfig {
        EnvironmentConfig::SelfHosted
    }

    fn r() -> SqliteEnvRegistry {
        SqliteEnvRegistry::open_in_memory().unwrap()
    }

    #[tokio::test]
    async fn create_get_list_archive_survive_the_store() {
        let r = r();
        let e = r
            .create("prod".into(), "d".into(), BTreeMap::new(), config())
            .await;
        assert!(e.is_self_hosted());
        let got = r.get(&e.id).await.expect("get");
        assert_eq!(got.name, "prod");
        assert_eq!(r.list_active().await.len(), 1);
        r.archive(&e.id).await.expect("archive");
        assert!(
            r.list_active().await.is_empty(),
            "archived drops from active"
        );
        assert!(r.get(&e.id).await.is_some(), "still retrievable");
    }

    #[tokio::test]
    async fn update_patches_and_metadata_null_deletes() {
        let r = r();
        let e = r
            .create(
                "e".into(),
                String::new(),
                BTreeMap::from([("keep".into(), "1".into()), ("drop".into(), "2".into())]),
                config(),
            )
            .await;
        let up = r
            .update(
                &e.id,
                EnvUpdate {
                    name: Some("renamed".into()),
                    metadata: Some(BTreeMap::from([("drop".into(), None)])),
                    ..Default::default()
                },
            )
            .await
            .expect("update");
        assert_eq!(up.name, "renamed");
        assert!(up.metadata.contains_key("keep"));
        assert!(!up.metadata.contains_key("drop"), "null deletes");
        // config preserved through the round-trip
        assert_eq!(up.config, config(), "config round-trips");
    }

    async fn registration_outbox_conformance(registry: &dyn EnvRegistry) {
        let created = registry
            .create("outbox".into(), String::new(), BTreeMap::new(), config())
            .await;
        let v1 = registry
            .registration_intent(&created.id, EnvironmentRevision(1))
            .await
            .unwrap()
            .expect("O1 exact intent");
        assert_eq!(
            v1.operation,
            EnvironmentRegistrationOperation::Register,
            "O1"
        );
        assert!(!v1.delivered, "O1");

        let updated = registry
            .update(
                &created.id,
                EnvUpdate {
                    name: Some("outbox-v2".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("O2 update");
        let v2 = registry
            .registration_intent(&updated.id, updated.revision)
            .await
            .unwrap()
            .expect("O2 exact intent");
        assert!(
            registry
                .mark_registration_intent_delivered(&v2)
                .await
                .unwrap(),
            "O2"
        );
        assert!(
            registry
                .mark_registration_intent_delivered(&v2)
                .await
                .unwrap(),
            "O2 acknowledgement replay"
        );
        let replayed = registry
            .update(
                &created.id,
                EnvUpdate {
                    name: Some("outbox-v2".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("O2b no-op update");
        assert_eq!(replayed.revision, EnvironmentRevision(2), "O2b");
        assert_eq!(
            registry
                .registration_intents(EnvironmentRegistrationIntentFilter::All)
                .await
                .unwrap()
                .len(),
            2,
            "O2b"
        );

        let archived = registry.archive(&created.id).await.expect("O3 archive");
        let archived_replay = registry.archive(&created.id).await.expect("O3 replay");
        assert_eq!(archived_replay.revision, archived.revision, "O3 replay");
        let pending = registry
            .registration_intents(EnvironmentRegistrationIntentFilter::Pending)
            .await
            .unwrap();
        assert_eq!(pending.len(), 2, "O3");
        assert_eq!(pending[0].revision, EnvironmentRevision(1), "O3");
        assert_eq!(pending[1].revision, archived.revision, "O3");
        assert_eq!(
            pending[1].operation,
            EnvironmentRegistrationOperation::Withdraw,
            "O3"
        );

        let all = registry
            .registration_intents(EnvironmentRegistrationIntentFilter::All)
            .await
            .unwrap();
        assert_eq!(
            all.iter()
                .map(|intent| intent.revision.0)
                .collect::<Vec<_>>(),
            [1, 2, 3],
            "O4"
        );
        assert!(all[1].delivered, "O4 acknowledgement is retained");
    }

    #[tokio::test]
    async fn every_registry_adapter_obeys_the_registration_outbox_decision_table() {
        // FMECA and mitigations for the Control authority boundary:
        // F1 revision commits without delivery intent (S9/O3/D7, RPN189) -> the
        // same DB transaction inserts both rows and the FK binds the exact pair;
        // F2 delivery succeeds but acknowledgement is lost (S5/O4/D2, RPN40) ->
        // the intent remains pending and the idempotent registrar is retried;
        // F3 archive is reconstructed as registration (S9/O2/D5, RPN90) -> the
        // immutable operation is stored with the terminal revision, never inferred
        // from a later current scan; F4 restart loses an in-memory projection
        // (S7/O3/D2, RPN42) -> `All` replays the same durable intent log; F5 an
        // identical update retry mints another revision (S6/O4/D3, RPN72) -> the
        // canonical patch reports no fact change and appends neither row.
        //
        // Cause/effect graph: C1=create; C2=update; C3=archive; C4=acknowledged;
        // C5=Pending filter; C6=All filter. Effects: E1=exact Register intent;
        // E2=exact Withdraw intent; E3=acknowledged work absent from Pending;
        // E4=all immutable intents remain recoverable.
        // | Rule | mutation | ack | filter  | effect |
        // | O1   | create   | no  | Pending | v1 Register (E1) |
        // | O2   | update   | yes | Pending | v1 only (E1,E3) |
        // | O2b  | same update | yes | All | still v1,v2 (no-op replay) |
        // | O3   | archive  | no  | Pending | v1 + v3 Withdraw (E2,E3) |
        // | O4   | all      | any | All     | v1,v2,v3 retained (E4) |
        registration_outbox_conformance(&r()).await;
        registration_outbox_conformance(&InMemoryEnvRegistry::new()).await;
    }

    #[tokio::test]
    async fn file_open_and_pg_connect_entry_points() {
        let dir = std::env::temp_dir().join(format!("env-cov-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("env.db");
        let r = SqliteEnvRegistry::open(path.to_str().unwrap()).expect("open file db");
        let e = r
            .create("e".into(), String::new(), BTreeMap::new(), config())
            .await;
        assert!(r.exists(&e.id).await);
        std::fs::remove_dir_all(&dir).ok();
        if let Ok(url) = std::env::var("AWAKEN_TEST_DATABASE_URL")
            && let Ok(r) = PostgresEnvRegistry::connect(&url).await
        {
            let e = r
                .create("c".into(), String::new(), BTreeMap::new(), config())
                .await;
            assert!(r.exists(&e.id).await);
            r.archive(&e.id).await;
        }
    }

    /// Live Postgres parity over the same portable bundle. Skips when no Postgres is
    /// reachable (`AWAKEN_TEST_DATABASE_URL`), isolated in its own schema.
    #[tokio::test]
    async fn postgres_parity_over_a_live_db() {
        use sqlx::Executor;
        use sqlx::postgres::{PgPool, PgPoolOptions};

        let url = std::env::var("AWAKEN_TEST_DATABASE_URL")
            .unwrap_or_else(|_| "postgres://postgres:pw@127.0.0.1:5455/cov".to_string());
        let Ok(admin) = PgPool::connect(&url).await else {
            println!("[skip] no Postgres reachable");
            return;
        };
        let _ = admin
            .execute("DROP SCHEMA IF EXISTS t_env_registry CASCADE")
            .await;
        admin
            .execute("CREATE SCHEMA t_env_registry")
            .await
            .expect("schema");
        admin.close().await;
        let pool = PgPoolOptions::new()
            .after_connect(|conn, _| {
                Box::pin(async move {
                    conn.execute("SET search_path = t_env_registry").await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .expect("schema pool");
        let r = PostgresEnvRegistry::with_pool(pool).await.expect("store");

        // The shared cause/effect rules above run unchanged against PostgreSQL;
        // this is adapter parity, not a PostgreSQL-specific reinterpretation.
        registration_outbox_conformance(&r).await;

        // PostgreSQL transaction-failure parity for the SQLite rollback rule:
        // C1 the final intent insert raises -> E1 create fails and E2 every table
        // count remains at its pre-command value.
        let tables = [
            "env_registry_env",
            "env_registry_revision",
            "env_registry_create_command",
            "env_registry_registration_intent",
        ];
        let mut before = Vec::new();
        for table in &tables {
            before.push(
                sqlx::query_scalar::<_, i64>(&format!("SELECT COUNT(*) FROM {table}"))
                    .fetch_one(&r.pool)
                    .await
                    .unwrap(),
            );
        }
        sqlx::query(
            "CREATE FUNCTION env_registry_reject_intent() RETURNS trigger \
             LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected intent failure'; END $$",
        )
        .execute(&r.pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER env_registry_fail_intent BEFORE INSERT \
             ON env_registry_registration_intent FOR EACH ROW \
             EXECUTE FUNCTION env_registry_reject_intent()",
        )
        .execute(&r.pool)
        .await
        .unwrap();
        let failed = r
            .create_once(CreateEnvironmentCommand {
                command_id: "postgres-rollback".into(),
                name: "rollback".into(),
                description: String::new(),
                metadata: Default::default(),
                scope: None,
                config: config(),
            })
            .await;
        assert!(
            matches!(failed, Err(CreateEnvironmentError::Store(_))),
            "E1"
        );
        for (index, table) in tables.iter().enumerate() {
            let after = sqlx::query_scalar::<_, i64>(&format!("SELECT COUNT(*) FROM {table}"))
                .fetch_one(&r.pool)
                .await
                .unwrap();
            assert_eq!(after, before[index], "E2/{table}");
        }
        sqlx::query("DROP TRIGGER env_registry_fail_intent ON env_registry_registration_intent")
            .execute(&r.pool)
            .await
            .unwrap();
        sqlx::query("DROP FUNCTION env_registry_reject_intent()")
            .execute(&r.pool)
            .await
            .unwrap();

        let e = r
            .create("prod".into(), "d".into(), BTreeMap::new(), config())
            .await;
        assert!(r.exists(&e.id).await);
        assert_eq!(r.get(&e.id).await.expect("get").name, "prod");
        assert_eq!(r.list_active().await.len(), 1);
        let up = r
            .update(
                &e.id,
                EnvUpdate {
                    name: Some("renamed".into()),
                    metadata: Some(BTreeMap::from([("t".into(), Some("x".into()))])),
                    ..Default::default()
                },
            )
            .await
            .expect("update");
        assert_eq!(up.name, "renamed");
        assert_eq!(up.config, config(), "config round-trips");
        assert_eq!(up.metadata.get("t").map(String::as_str), Some("x"));
        r.archive(&e.id).await.expect("archive");
        assert!(
            r.list_active().await.is_empty(),
            "archived drops from active"
        );
        assert!(r.get(&e.id).await.is_some(), "still retrievable");
        assert!(
            r.get_revision(&e.id, EnvironmentRevision(1))
                .await
                .is_some(),
            "terminal archive preserves exact history"
        );
    }
}
