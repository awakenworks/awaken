// @formal entrypoint environment.registration_intent_is_atomic_recoverable_and_idempotent
//! Durable Control-owned [`EnvRegistry`] backends. Environment definitions and
//! exact revision history survive restart; Coordinator and Worker see only the
//! separately registered executable projection. SQLite and PostgreSQL share one
//! portable migration bundle.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_environment_contract::{
    CreateEnvironmentCommand, CreateEnvironmentError, CreateEnvironmentOutcome, EnvItem,
    EnvRegistry, EnvUpdate, EnvironmentConfig, EnvironmentRegistrationIntent,
    EnvironmentRegistrationIntentFilter, EnvironmentRegistrationOperation, EnvironmentRevision,
    EnvironmentSandboxPolicyRef, EnvironmentStoreError,
};
use awaken_lease_contract::WallClock;
use awaken_lease_contract::WallClockExt;
#[cfg(test)]
use awaken_lease_contract::system_clock;
use awaken_scoped_migration::{Migration, MigrationBundle, MigrationError};
use awaken_sqlite_pool::{SharedSqliteConnection, SqliteConnectionFactory};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sqlx::Row;
use sqlx::postgres::{PgPool, PgRow};

// The test-support reference backend lives here beside the durable siblings; the
// port and value objects stay inward in `awaken-environment-contract`.
#[cfg(any(test, feature = "test-support"))]
mod inmem;
mod postgres;
#[cfg(any(test, feature = "test-support"))]
pub use inmem::InMemoryEnvRegistry;
pub use postgres::PostgresEnvRegistry;

/// The store's table namespace / bundle prefix (`env_registry_env`).
const NS: &str = "env_registry";

fn environment_store(error: impl std::fmt::Display) -> EnvironmentStoreError {
    EnvironmentStoreError::Backend(error.to_string())
}

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
            observation_columns(11, "env")?,
            observation_columns(12, "revision")?,
        ],
    )
}

/// The durable acceptance and last-commit instants of one Environment.
///
/// Rows written before these columns existed never recorded either fact, so
/// there is nothing earlier to recover. The migration writes the only real
/// instant it has — when the columns were added on this install — rather than a
/// constant that would claim every install registered its Environments on the
/// same day. Afterwards the registry's injected clock stamps both inside the
/// transaction that commits the revision, and the database-side default is
/// dropped where the dialect allows it so an insert cannot acquire a time by
/// omission.
fn observation_columns(version: i64, table: &str) -> Result<Migration, MigrationError> {
    const PG_NOW: &str = "to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')";
    const SQLITE_NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ', 'now')";
    Migration::per_dialect(
        version,
        format!("record when each Environment {table} row was committed"),
        format!(
            "ALTER TABLE {{prefix}}_{table} ADD COLUMN created_at TEXT NOT NULL DEFAULT {PG_NOW}, \
             ADD COLUMN updated_at TEXT NOT NULL DEFAULT {PG_NOW}; \
             ALTER TABLE {{prefix}}_{table} ALTER COLUMN created_at DROP DEFAULT, \
             ALTER COLUMN updated_at DROP DEFAULT"
        ),
        format!(
            "ALTER TABLE {{prefix}}_{table} ADD COLUMN created_at TEXT NOT NULL DEFAULT ''; \
             ALTER TABLE {{prefix}}_{table} ADD COLUMN updated_at TEXT NOT NULL DEFAULT ''; \
             UPDATE {{prefix}}_{table} SET created_at = {SQLITE_NOW}, updated_at = {SQLITE_NOW} \
             WHERE created_at = ''"
        ),
    )
}

/// The columns an env row projects to an [`EnvItem`], in `SELECT` order.
const COLS: &str = "env_id, name, description, metadata_json, config_json, archived_at, revision, scope, sandbox_policy_json, created_at, updated_at";

fn metadata_str(m: &BTreeMap<String, String>) -> Result<String, EnvironmentStoreError> {
    serde_json::to_string(m).map_err(environment_store)
}

fn config_str(c: &EnvironmentConfig) -> Result<String, EnvironmentStoreError> {
    serde_json::to_string(c).map_err(environment_store)
}

fn sandbox_policy_str(
    reference: &Option<EnvironmentSandboxPolicyRef>,
) -> Result<Option<String>, EnvironmentStoreError> {
    reference
        .as_ref()
        .map(|reference| serde_json::to_string(reference).map_err(environment_store))
        .transpose()
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
    created_at: String,
    updated_at: String,
}

impl PersistedEnvRow {
    fn try_into_item(self) -> Result<EnvItem, EnvironmentStoreError> {
        let config: EnvironmentConfig =
            serde_json::from_str(&self.config_json).map_err(environment_store)?;
        config.validate()?;
        Ok(EnvItem {
            id: self.id,
            revision: EnvironmentRevision(u64::try_from(self.revision).map_err(|_| {
                EnvironmentStoreError::Backend("invalid persisted Environment revision".into())
            })?),
            name: self.name,
            description: decode_description(self.description)?,
            metadata: serde_json::from_str(&self.metadata_json).map_err(environment_store)?,
            scope: self.scope,
            config,
            sandbox_policy: self
                .sandbox_policy_json
                .map(|json| serde_json::from_str(&json).map_err(environment_store))
                .transpose()?,
            created_at: Some(self.created_at),
            updated_at: Some(self.updated_at),
            archived_at: self.archived_at,
        })
    }
}

const DESCRIPTION_CODEC_PREFIX: &str = "awaken-option-json:";

fn encode_description(description: &Option<String>) -> String {
    format!(
        "{DESCRIPTION_CODEC_PREFIX}{}",
        serde_json::to_string(description)
            .expect("Option<String> JSON serialization is infallible")
    )
}

fn decode_description(stored: String) -> Result<Option<String>, EnvironmentStoreError> {
    match stored.strip_prefix(DESCRIPTION_CODEC_PREFIX) {
        Some(encoded) => serde_json::from_str(encoded).map_err(environment_store),
        // Rows written before nullable SDK descriptions are literal strings;
        // retain even an empty string as an authored value.
        None => Ok(Some(stored)),
    }
}

fn sqlite_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PersistedEnvRow> {
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
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

fn pg_row(row: &PgRow) -> Result<EnvItem, EnvironmentStoreError> {
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
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
    .try_into_item()
}

/// SQLite persistence for the environment registry.
pub struct SqliteEnvRegistry {
    conn: SharedSqliteConnection,
    /// The one source of every observation timestamp this registry writes.
    clock: Arc<dyn WallClock>,
}

impl SqliteEnvRegistry {
    pub fn open(path: &str, clock: Arc<dyn WallClock>) -> Result<Self, String> {
        Self::from_connection(
            SqliteConnectionFactory::file(path)
                .open()
                .map_err(|e| e.to_string())?,
            clock,
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn open_in_memory(clock: Arc<dyn WallClock>) -> Result<Self, String> {
        Self::from_connection(
            SqliteConnectionFactory::memory()
                .open()
                .map_err(|e| e.to_string())?,
            clock,
        )
    }

    fn from_connection(conn: Connection, clock: Arc<dyn WallClock>) -> Result<Self, String> {
        let bundle = env_bundle().map_err(|e| e.to_string())?;
        awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
            .map_err(|e| e.to_string())?
            .run_bundle(&conn, &bundle)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            conn: SharedSqliteConnection::new(conn),
            clock,
        })
    }

    async fn with_connection<T, F>(&self, operation: F) -> Result<T, EnvironmentStoreError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, EnvironmentStoreError> + Send + 'static,
    {
        awaken_sqlite_pool::with_connection(self.conn.clone(), operation)
            .await
            .map_err(environment_store)?
    }

    fn read(tx: &Transaction<'_>, id: &str) -> Result<Option<EnvItem>, EnvironmentStoreError> {
        tx.query_row(
            &format!("SELECT {COLS} FROM env_registry_env WHERE env_id = ?1"),
            params![id],
            sqlite_row,
        )
        .optional()
        .map_err(environment_store)?
        .map(PersistedEnvRow::try_into_item)
        .transpose()
    }

    fn insert_revision(tx: &Transaction<'_>, item: &EnvItem) -> Result<(), EnvironmentStoreError> {
        tx.execute(
            "INSERT INTO env_registry_revision \
             (env_id, revision, name, description, metadata_json, config_json, archived_at, scope, sandbox_policy_json, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                item.id,
                item.revision.0,
                item.name,
                encode_description(&item.description),
                metadata_str(&item.metadata)?,
                config_str(&item.config)?,
                item.archived_at,
                item.scope,
                sandbox_policy_str(&item.sandbox_policy)?,
                item.created_at,
                item.updated_at,
            ],
        )
        .map_err(environment_store)?;
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
        command.config.validate()?;
        let committed_at = self.clock.observed_at();
        awaken_sqlite_pool::with_connection(self.conn.clone(), move |conn| {
            let tx = conn
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
                .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?
                .ok_or_else(|| CreateEnvironmentError::Store("command target is missing".into()))?;
            return Ok(CreateEnvironmentOutcome::Replayed(item));
        }
        let next: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq), -1) + 1 FROM env_registry_env",
                [],
                |r| r.get(0),
            )
            .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?;
        let id = format!("env_{next:016}");
        tx.execute(
            "INSERT INTO env_registry_env \
                (env_id, seq, name, description, metadata_json, config_json, scope, sandbox_policy_json, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
            params![
                id,
                next,
                command.name,
                encode_description(&command.description),
                metadata_str(&command.metadata)
                    .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?,
                config_str(&command.config)
                    .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?,
                command.scope,
                sandbox_policy_str(&command.sandbox_policy)
                    .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?,
                committed_at,
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
            sandbox_policy: command.sandbox_policy,
            created_at: Some(committed_at.clone()),
            updated_at: Some(committed_at),
            archived_at: None,
        };
        Self::insert_revision(&tx, &item)
            .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?;
        Self::insert_registration_intent(&tx, &item)
            .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?;
        tx.commit()
            .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?;
            Ok(CreateEnvironmentOutcome::Created(item))
        })
        .await
        .map_err(|error| CreateEnvironmentError::Store(error.to_string()))?
    }

    async fn list_active(&self) -> Result<Vec<EnvItem>, EnvironmentStoreError> {
        self.with_connection(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {COLS} FROM env_registry_env WHERE archived_at IS NULL ORDER BY seq ASC"
                ))
                .map_err(environment_store)?;
            let rows = stmt.query_map([], sqlite_row).map_err(environment_store)?;
            rows.map(|row| row.map_err(environment_store)?.try_into_item())
                .collect()
        })
        .await
    }

    async fn list_all(&self) -> Result<Vec<EnvItem>, EnvironmentStoreError> {
        self.with_connection(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {COLS} FROM env_registry_env ORDER BY seq ASC"
                ))
                .map_err(environment_store)?;
            let rows = stmt.query_map([], sqlite_row).map_err(environment_store)?;
            rows.map(|row| row.map_err(environment_store)?.try_into_item())
                .collect()
        })
        .await
    }

    async fn get(&self, id: &str) -> Result<Option<EnvItem>, EnvironmentStoreError> {
        let id = id.to_string();
        self.with_connection(move |conn| {
            let tx = conn.transaction().map_err(environment_store)?;
            Self::read(&tx, &id)
        })
        .await
    }

    async fn get_revision(
        &self,
        id: &str,
        revision: EnvironmentRevision,
    ) -> Result<Option<EnvItem>, EnvironmentStoreError> {
        let id = id.to_string();
        self.with_connection(move |conn| {
            conn.query_row(
                &format!(
                    "SELECT {COLS} FROM env_registry_revision WHERE env_id = ?1 AND revision = ?2"
                ),
                params![id, revision.0],
                sqlite_row,
            )
            .optional()
            .map_err(environment_store)?
            .map(PersistedEnvRow::try_into_item)
            .transpose()
        })
        .await
    }

    async fn exists(&self, id: &str) -> Result<bool, EnvironmentStoreError> {
        Ok(self.get(id).await?.is_some())
    }

    async fn update(
        &self,
        id: &str,
        expected_revision: EnvironmentRevision,
        patch: EnvUpdate,
    ) -> Result<Option<EnvItem>, EnvironmentStoreError> {
        let id = id.to_string();
        let committed_at = self.clock.observed_at();
        self.with_connection(move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(environment_store)?;
            let Some(mut item) = Self::read(&tx, &id)? else {
                return Ok(None);
            };
            if item.archived_at.is_some() {
                return Ok(None);
            }
            if item.revision != expected_revision {
                return Err(EnvironmentStoreError::RevisionConflict);
            }
            if !item.apply(patch)? {
                return Ok(Some(item));
            }
            item.updated_at = Some(committed_at);
            tx.execute(
                "UPDATE env_registry_env SET name = ?1, description = ?2, metadata_json = ?3, \
                 config_json = ?4, revision = ?5, scope = ?6, sandbox_policy_json = ?7, \
                 updated_at = ?9 WHERE env_id = ?8",
                params![
                    item.name,
                    encode_description(&item.description),
                    metadata_str(&item.metadata)?,
                    config_str(&item.config)?,
                    item.revision.0,
                    item.scope,
                    sandbox_policy_str(&item.sandbox_policy)?,
                    id,
                    item.updated_at,
                ],
            )
            .map_err(environment_store)?;
            Self::insert_revision(&tx, &item).map_err(environment_store)?;
            Self::insert_registration_intent(&tx, &item).map_err(environment_store)?;
            tx.commit().map_err(environment_store)?;
            Ok(Some(item))
        })
        .await
    }

    async fn archive(&self, id: &str) -> Result<Option<EnvItem>, EnvironmentStoreError> {
        let id = id.to_string();
        let committed_at = self.clock.observed_at();
        self.with_connection(move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(environment_store)?;
            let Some(mut item) = Self::read(&tx, &id)? else {
                return Ok(None);
            };
            if item.archived_at.is_some() {
                return Ok(Some(item));
            }
            item.archived_at = Some(committed_at.clone());
            item.updated_at = Some(committed_at);
            item.revision =
                EnvironmentRevision(item.revision.0.checked_add(1).ok_or_else(|| {
                    EnvironmentStoreError::Backend("Environment revision exhausted".into())
                })?);
            tx.execute(
                "UPDATE env_registry_env SET archived_at = ?1, revision = ?2, updated_at = ?1 \
                 WHERE env_id = ?3",
                params![item.archived_at, item.revision.0, id],
            )
            .map_err(environment_store)?;
            Self::insert_revision(&tx, &item).map_err(environment_store)?;
            Self::insert_registration_intent(&tx, &item).map_err(environment_store)?;
            tx.commit().map_err(environment_store)?;
            Ok(Some(item))
        })
        .await
    }

    async fn registration_intent(
        &self,
        id: &str,
        revision: EnvironmentRevision,
    ) -> Result<Option<EnvironmentRegistrationIntent>, EnvironmentStoreError> {
        let id = id.to_string();
        self.with_connection(move |conn| {
            let row = conn
                .query_row(
                    "SELECT operation, delivered FROM env_registry_registration_intent \
                 WHERE env_id = ?1 AND revision = ?2",
                    params![id, revision.0],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()
                .map_err(environment_store)?;
            row.map(|(operation, delivered)| {
                Ok(EnvironmentRegistrationIntent {
                    environment_id: id,
                    revision,
                    operation: parse_operation(&operation)
                        .map_err(EnvironmentStoreError::Backend)?,
                    delivered: delivered != 0,
                })
            })
            .transpose()
        })
        .await
    }

    async fn registration_intents(
        &self,
        filter: EnvironmentRegistrationIntentFilter,
    ) -> Result<Vec<EnvironmentRegistrationIntent>, EnvironmentStoreError> {
        self.with_connection(move |conn| {
            let where_clause = match filter {
                EnvironmentRegistrationIntentFilter::Pending => " WHERE delivered = 0",
                EnvironmentRegistrationIntentFilter::All => "",
            };
            let mut statement = conn
                .prepare(&format!(
                    "SELECT env_id, revision, operation, delivered \
                 FROM env_registry_registration_intent{where_clause} ORDER BY env_id, revision"
                ))
                .map_err(environment_store)?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                })
                .map_err(environment_store)?;
            rows.map(|row| {
                let (environment_id, revision, operation, delivered) =
                    row.map_err(environment_store)?;
                Ok(EnvironmentRegistrationIntent {
                    environment_id,
                    revision: EnvironmentRevision(u64::try_from(revision).map_err(|_| {
                        EnvironmentStoreError::Backend(
                            "invalid Environment registration revision".into(),
                        )
                    })?),
                    operation: parse_operation(&operation)
                        .map_err(EnvironmentStoreError::Backend)?,
                    delivered: delivered != 0,
                })
            })
            .collect()
        })
        .await
    }

    async fn mark_registration_intent_delivered(
        &self,
        intent: &EnvironmentRegistrationIntent,
    ) -> Result<bool, EnvironmentStoreError> {
        let intent = intent.clone();
        self.with_connection(move |conn| {
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
                .map_err(environment_store)?;
            Ok(changed == 1)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_environment_contract::{
        EnvironmentConfigMutation, EnvironmentFieldUpdate, EnvironmentNetworking,
        EnvironmentNetworkingMutation, EnvironmentPackages, EnvironmentStoreError,
        InvalidEnvironmentConfig,
    };

    #[test]
    fn environment_schema_is_versioned_and_unconditional() {
        // Cause/effect rule: every Environment schema change is a positive,
        // checksum-tracked migration; conditional DDL or conflict-ignore SQL is
        // rejected before any adapter can apply it.
        let bundle = env_bundle().expect("Environment bundle");
        awaken_scoped_migration::lint(std::slice::from_ref(&bundle))
            .expect("deterministic Environment migrations");
    }

    #[test]
    fn nullable_description_codec_preserves_all_three_storage_generations() {
        // Cause/effect graph: C1=new null, C2=new empty string, C3=new ordinary
        // or prefix-looking string, C4=legacy literal. Effects: E1=None,
        // E2=Some(""), E3=byte-exact Some(value), E4=legacy Some(value).
        // Decision rules R1=C1->E1, R2=C2->E2, R3=C3->E3, R4=C4->E4.
        // The NOT NULL database columns remain the one durable authority; this
        // codec prevents a second nullable shadow column or destructive rewrite.
        for value in [
            None,
            Some(String::new()),
            Some("ordinary".into()),
            Some("awaken-option-json:null".into()),
        ] {
            assert_eq!(
                decode_description(encode_description(&value)).unwrap(),
                value,
                "R1-R3"
            );
        }
        assert_eq!(
            decode_description(String::new()).unwrap(),
            Some(String::new()),
            "R4"
        );
    }

    #[test]
    fn persisted_invalid_environment_fails_closed_on_rehydration() {
        // C1 legacy/corrupt storage contains an unreachable package plan -> E1
        // rehydration returns the same typed domain error rather than admitting
        // an invalid aggregate through a read-side bypass.
        let row = PersistedEnvRow {
            id: "env_invalid".into(),
            name: "invalid".into(),
            description: encode_description(&None),
            metadata_json: "{}".into(),
            config_json: serde_json::json!({
                "type": "cloud",
                "networking": {
                    "type": "limited",
                    "allow_package_managers": false
                },
                "packages": { "npm": ["tsx"] }
            })
            .to_string(),
            archived_at: None,
            revision: 1,
            scope: None,
            sandbox_policy_json: None,
            created_at: "2026-02-03T04:05:06.007Z".into(),
            updated_at: "2026-02-03T04:05:06.007Z".into(),
        };
        assert_eq!(
            row.try_into_item().unwrap_err(),
            EnvironmentStoreError::InvalidConfig(
                InvalidEnvironmentConfig::PackagesRequirePackageManager
            )
        );
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
            [9, 10, 11, 12],
            "M1/M2"
        );
        assert!(
            runner.run_bundle(&connection, &full).unwrap().is_empty(),
            "M3"
        );
        // M4: rows that predate the observation columns carry the only real
        // instant available — when this install added them — never the empty
        // placeholder the ALTER needed and never a frozen literal.
        for table in ["env", "revision"] {
            let stamps: Vec<(String, String)> = connection
                .prepare(&format!(
                    "SELECT created_at, updated_at FROM env_registry_{table}"
                ))
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            assert!(!stamps.is_empty(), "M4 {table} rows");
            for (created, updated) in stamps {
                assert_eq!(created, updated, "M4 {table} backfill is one reading");
                assert!(
                    awaken_lease_contract::utc::rfc3339_to_epoch_millis(&created)
                        .is_some_and(|millis| millis > 0),
                    "M4 {table} backfilled a real instant, got {created:?}"
                );
                assert_ne!(
                    created, "2026-01-01T00:00:00Z",
                    "M4 {table} not the frozen literal"
                );
            }
        }

        let registry = SqliteEnvRegistry::from_connection(connection, system_clock()).unwrap();
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
                description: None,
                metadata: Default::default(),
                scope: None,
                config: config(),
                sandbox_policy: None,
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
        SqliteEnvRegistry::open_in_memory(system_clock()).unwrap()
    }

    /// Failure-mode rule: losing the SQLite connection lock is an adapter error,
    /// never a process-wide panic and never an invented empty catalog.
    #[tokio::test]
    async fn sqlite_lock_failure_is_reported_by_every_read_boundary() {
        let registry = r();
        let connection = registry.conn.clone();
        let _ = std::thread::spawn(move || {
            let _guard = connection
                .lock()
                .expect("acquire connection before poisoning");
            panic!("inject poisoned SQLite connection lock");
        })
        .join();

        assert!(registry.list_active().await.is_err());
        assert!(registry.list_all().await.is_err());
        assert!(registry.get("env_missing").await.is_err());
        assert!(registry.exists("env_missing").await.is_err());
    }

    #[tokio::test]
    async fn corrupt_persisted_environment_is_never_silently_defaulted_or_panicked() {
        // Corruption decision table:
        // | persisted fact       | old behavior       | required observation |
        // | metadata_json        | invented `{}`      | typed store error    |
        // | config_json          | process panic      | typed store error    |
        // | sandbox_policy_json  | process panic      | typed store error    |
        // A storage adapter may decode domain values, but it must neither repair
        // authority data implicitly nor terminate the process when that data is bad.
        for column in ["metadata_json", "config_json", "sandbox_policy_json"] {
            let registry = r();
            let environment = registry
                .create(
                    format!("corrupt-{column}"),
                    String::new(),
                    BTreeMap::new(),
                    config(),
                )
                .await
                .expect("create Environment fixture");
            registry
                .conn
                .lock()
                .expect("Environment test connection")
                .execute(
                    &format!("UPDATE env_registry_env SET {column} = 'not-json' WHERE env_id = ?1"),
                    params![environment.id],
                )
                .expect("inject corrupt persisted value");

            assert!(registry.get(&environment.id).await.is_err(), "{column}");
            assert!(registry.list_all().await.is_err(), "{column}");
        }
    }

    #[tokio::test]
    async fn create_get_list_archive_survive_the_store() {
        let r = r();
        let e = r
            .create("prod".into(), "d".into(), BTreeMap::new(), config())
            .await
            .expect("create Environment");
        assert!(e.is_self_hosted());
        let got = r
            .get(&e.id)
            .await
            .expect("read Environment store operation")
            .expect("get");
        assert_eq!(got.name, "prod");
        assert_eq!(r.list_active().await.expect("list Environments").len(), 1);
        r.archive(&e.id)
            .await
            .expect("archive Environment store operation")
            .expect("archive");
        assert!(
            r.list_active().await.expect("list Environments").is_empty(),
            "archived drops from active"
        );
        assert!(
            r.get(&e.id).await.expect("read Environment").is_some(),
            "still retrievable"
        );
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
            .await
            .expect("create Environment");
        let up = r
            .update(
                &e.id,
                e.revision,
                EnvUpdate {
                    name: Some("renamed".into()),
                    metadata: Some(BTreeMap::from([("drop".into(), None)])),
                    ..Default::default()
                },
            )
            .await
            .expect("update Environment store operation")
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
            .await
            .expect("O1 create");
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
                created.revision,
                EnvUpdate {
                    name: Some("outbox-v2".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("O2 update store operation")
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
                updated.revision,
                EnvUpdate {
                    name: Some("outbox-v2".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("O2b update store operation")
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

        let archived = registry
            .archive(&created.id)
            .await
            .expect("O3 archive store operation")
            .expect("O3 archive");
        let archived_replay = registry
            .archive(&created.id)
            .await
            .expect("O3 replay store operation")
            .expect("O3 replay");
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
        let r =
            SqliteEnvRegistry::open(path.to_str().unwrap(), system_clock()).expect("open file db");
        let e = r
            .create("e".into(), String::new(), BTreeMap::new(), config())
            .await
            .expect("create SQLite Environment");
        assert!(r.exists(&e.id).await.expect("read SQLite Environment"));
        std::fs::remove_dir_all(&dir).ok();
        if let Ok(url) = std::env::var("AWAKEN_TEST_DATABASE_URL")
            && let Ok(r) = PostgresEnvRegistry::connect(&url, system_clock()).await
        {
            let e = r
                .create("c".into(), String::new(), BTreeMap::new(), config())
                .await
                .expect("create PostgreSQL Environment");
            assert!(r.exists(&e.id).await.expect("read PostgreSQL Environment"));
            r.archive(&e.id)
                .await
                .expect("archive PostgreSQL Environment");
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
        for schema in ["t_env_registry", "t_env_registry_instants"] {
            let _ = admin
                .execute(format!("DROP SCHEMA IF EXISTS {schema} CASCADE").as_str())
                .await;
            admin
                .execute(format!("CREATE SCHEMA {schema}").as_str())
                .await
                .expect("schema");
        }
        admin.close().await;
        let instants_pool = PgPoolOptions::new()
            .after_connect(|conn, _| {
                Box::pin(async move {
                    conn.execute("SET search_path = t_env_registry_instants")
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .expect("instants schema pool");
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
        // Observation parity first, on its own schema and its own hand-driven
        // clock: the Postgres adapter must write the same injected reading the
        // reference and SQLite siblings do (create -> both instants; update ->
        // only `updated_at`; archive -> `archived_at` == `updated_at`), and it
        // must persist them rather than recompute on read.
        {
            use awaken_lease_contract::utc::epoch_millis_to_rfc3339;
            use awaken_lease_testkit::ManualClock;

            const CREATED: u64 = 1_700_000_000_000;
            const UPDATED: u64 = CREATED + 4_000;
            const ARCHIVED: u64 = CREATED + 9_000;

            let clock = Arc::new(ManualClock::new(CREATED));
            let observed = PostgresEnvRegistry::with_pool(instants_pool.clone(), clock.clone())
                .await
                .expect("observation store");
            let item = observed
                .create(
                    "pg-instants".into(),
                    String::new(),
                    BTreeMap::new(),
                    EnvironmentConfig::SelfHosted,
                )
                .await
                .expect("pg create");
            assert_eq!(
                item.created_at,
                Some(epoch_millis_to_rfc3339(CREATED)),
                "pg create stamps the injected reading"
            );
            assert_eq!(item.updated_at, item.created_at, "pg create: one reading");
            clock.set(UPDATED);
            let updated = observed
                .update(
                    &item.id,
                    item.revision,
                    EnvUpdate {
                        name: Some("pg-renamed".into()),
                        ..Default::default()
                    },
                )
                .await
                .expect("pg update")
                .expect("pg updated record");
            assert_eq!(
                updated.created_at, item.created_at,
                "pg acceptance is fixed"
            );
            assert_eq!(
                updated.updated_at,
                Some(epoch_millis_to_rfc3339(UPDATED)),
                "pg update advances only the commit instant"
            );
            clock.set(ARCHIVED);
            let archived = observed
                .archive(&item.id)
                .await
                .expect("pg archive")
                .expect("pg archived record");
            let at_archive = Some(epoch_millis_to_rfc3339(ARCHIVED));
            assert_eq!(archived.archived_at, at_archive, "pg archive instant");
            assert_eq!(archived.updated_at, at_archive, "pg archive: one reading");
            // Round trip: a reader on a different clock sees the written values.
            let reread = PostgresEnvRegistry::with_pool(instants_pool.clone(), system_clock())
                .await
                .expect("reader store")
                .get(&item.id)
                .await
                .expect("pg reread")
                .expect("pg record survives");
            assert_eq!(reread.created_at, item.created_at, "pg round trip");
            assert_eq!(reread.updated_at, at_archive, "pg round trip");
        }

        let r = PostgresEnvRegistry::with_pool(pool, system_clock())
            .await
            .expect("store");

        // The shared cause/effect rules above run unchanged against PostgreSQL;
        // this is adapter parity, not a PostgreSQL-specific reinterpretation.
        registration_outbox_conformance(&r).await;

        // Cross-backend invariant parity: C1 limited networking denies package
        // managers while C2 packages are configured -> E1 create/update reject;
        // E2 the failed update appends neither a revision nor an outbox intent.
        // C3 the invariant fixture is complete -> E3 archive it through the
        // canonical terminal transition so later lifecycle rules own the only
        // active row without discarding this fixture's durable history.
        let invalid_config = EnvironmentConfig::Cloud {
            networking: EnvironmentNetworking::Limited {
                allowed_hosts: Vec::new(),
                allow_mcp_servers: false,
                allow_package_managers: false,
            },
            packages: EnvironmentPackages {
                npm: vec!["tsx".into()],
                ..Default::default()
            },
        };
        assert_eq!(
            r.create(
                "invalid".into(),
                String::new(),
                BTreeMap::new(),
                invalid_config,
            )
            .await
            .unwrap_err(),
            CreateEnvironmentError::InvalidConfig(
                InvalidEnvironmentConfig::PackagesRequirePackageManager
            )
        );
        let valid = r
            .create(
                "valid".into(),
                String::new(),
                BTreeMap::new(),
                EnvironmentConfig::Cloud {
                    networking: EnvironmentNetworking::Limited {
                        allowed_hosts: Vec::new(),
                        allow_mcp_servers: false,
                        allow_package_managers: true,
                    },
                    packages: EnvironmentPackages {
                        npm: vec!["tsx".into()],
                        ..Default::default()
                    },
                },
            )
            .await
            .unwrap();
        let intent_count = r
            .registration_intents(EnvironmentRegistrationIntentFilter::All)
            .await
            .unwrap()
            .len();
        assert_eq!(
            r.update(
                &valid.id,
                valid.revision,
                EnvUpdate {
                    config: Some(EnvironmentConfigMutation::PatchCloud {
                        networking: Some(EnvironmentNetworkingMutation::Limited {
                            allowed_hosts: None,
                            allow_mcp_servers: None,
                            allow_package_managers: Some(EnvironmentFieldUpdate::Replace(false)),
                        }),
                        packages: None,
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err(),
            EnvironmentStoreError::InvalidConfig(
                InvalidEnvironmentConfig::PackagesRequirePackageManager
            )
        );
        assert_eq!(r.get(&valid.id).await.unwrap(), Some(valid.clone()));
        assert!(
            r.get_revision(&valid.id, EnvironmentRevision(2))
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            r.registration_intents(EnvironmentRegistrationIntentFilter::All)
                .await
                .unwrap()
                .len(),
            intent_count
        );
        r.archive(&valid.id)
            .await
            .expect("archive invariant fixture")
            .expect("invariant fixture exists");

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
                description: None,
                metadata: Default::default(),
                scope: None,
                config: config(),
                sandbox_policy: None,
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

        // Fixture-isolation rule P3: C1 the invalid-update fixture remains active
        // after proving rollback; C2 the following create/list parity rule assumes
        // no pre-existing active Environment. E1 archive the fixture through the
        // public contract (and therefore preserve its revision/outbox history);
        // E2 the next active count observes only the Environment it creates.
        r.archive(&valid.id)
            .await
            .expect("archive parity fixture")
            .expect("parity fixture exists");

        let e = r
            .create("prod".into(), "d".into(), BTreeMap::new(), config())
            .await
            .expect("create Environment");
        assert!(r.exists(&e.id).await.expect("read Environment"));
        assert_eq!(
            r.get(&e.id)
                .await
                .expect("read Environment store operation")
                .expect("get")
                .name,
            "prod"
        );
        assert_eq!(r.list_active().await.expect("list Environments").len(), 1);
        let up = r
            .update(
                &e.id,
                e.revision,
                EnvUpdate {
                    name: Some("renamed".into()),
                    metadata: Some(BTreeMap::from([("t".into(), Some("x".into()))])),
                    ..Default::default()
                },
            )
            .await
            .expect("update Environment store operation")
            .expect("update");
        assert_eq!(up.name, "renamed");
        assert_eq!(up.config, config(), "config round-trips");
        assert_eq!(up.metadata.get("t").map(String::as_str), Some("x"));
        r.archive(&e.id)
            .await
            .expect("archive Environment store operation")
            .expect("archive");
        assert!(
            r.list_active().await.expect("list Environments").is_empty(),
            "archived drops from active"
        );
        assert!(
            r.get(&e.id).await.expect("read Environment").is_some(),
            "still retrievable"
        );
        assert!(
            r.get_revision(&e.id, EnvironmentRevision(1))
                .await
                .expect("read Environment revision")
                .is_some(),
            "terminal archive preserves exact history"
        );

        // An unavailable durable backend is observable as a store failure rather
        // than a false empty result or a panic.
        r.pool.close().await;
        assert!(r.list_active().await.is_err());
        assert!(r.get(&e.id).await.is_err());
    }
}
