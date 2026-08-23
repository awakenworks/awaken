//! PostgreSQL durability adapter for the executable Environment projection.
//!
//! Only boundary commands are persisted. Startup and every mutation replay the
//! canonical in-memory catalog state machine, so SQL never becomes a parallel
//! lifecycle implementation.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_durable_projection::{
    ProjectionBatch, ProjectionCursor, ProjectionLoadError, ProjectionLog, Sequenced, load_full,
    load_refresh,
};
use awaken_executable_environment_contract::{
    ExecutableEnvironmentRegistrar, ExecutableEnvironmentRegistration,
    ExecutableEnvironmentRegistrationError, ExecutableEnvironmentRegistrationOutcome,
    ExecutableEnvironmentWithdrawal, ExecutableEnvironmentWithdrawalOutcome,
};
use sqlx::postgres::PgPool;
use tokio::sync::Mutex;

use crate::schema::{NS, executable_environment_catalog_bundle};
use crate::{ExecutableEnvironmentCatalog, LocalExecutableEnvironmentRegistrar};

const REGISTRATION_KIND: &str = "registration";
const WITHDRAWAL_KIND: &str = "withdrawal";

#[derive(Clone, Debug, PartialEq)]
enum CatalogCommand {
    Registration(Box<ExecutableEnvironmentRegistration>),
    Withdrawal(ExecutableEnvironmentWithdrawal),
}

impl CatalogCommand {
    fn identity(&self) -> (&str, u64, &'static str) {
        match self {
            Self::Registration(command) => (
                &command.definition.id,
                command.definition.revision.0,
                REGISTRATION_KIND,
            ),
            Self::Withdrawal(command) => (
                &command.environment_id,
                command.lifecycle_revision.0,
                WITHDRAWAL_KIND,
            ),
        }
    }

    fn encode(&self) -> Result<String, ExecutableEnvironmentRegistrationError> {
        match self {
            Self::Registration(command) => serde_json::to_string(command),
            Self::Withdrawal(command) => serde_json::to_string(command),
        }
        .map_err(storage)
    }

    fn decode(
        environment_id: &str,
        lifecycle_revision: i64,
        kind: &str,
        encoded: &str,
    ) -> Result<Self, ExecutableEnvironmentRegistrationError> {
        let revision = u64::try_from(lifecycle_revision)
            .map_err(|_| storage("negative executable Environment revision"))?;
        let command = match kind {
            REGISTRATION_KIND => {
                Self::Registration(Box::new(serde_json::from_str(encoded).map_err(storage)?))
            }
            WITHDRAWAL_KIND => Self::Withdrawal(serde_json::from_str(encoded).map_err(storage)?),
            other => return Err(storage(format!("unknown catalog command kind `{other}`"))),
        };
        let (stored_id, stored_revision, stored_kind) = command.identity();
        if stored_id != environment_id || stored_revision != revision || stored_kind != kind {
            return Err(storage(
                "catalog command columns do not match the serialized command",
            ));
        }
        Ok(command)
    }
}

fn storage(error: impl std::fmt::Display) -> ExecutableEnvironmentRegistrationError {
    ExecutableEnvironmentRegistrationError::Storage(error.to_string())
}

#[async_trait]
trait CatalogCommandLog:
    ProjectionLog<CatalogCommand, Error = ExecutableEnvironmentRegistrationError> + Send + Sync
{
    async fn append(
        &self,
        command: &CatalogCommand,
    ) -> Result<u64, ExecutableEnvironmentRegistrationError>;
}

struct PostgresCatalogCommandLog {
    pool: PgPool,
}

#[async_trait]
impl CatalogCommandLog for PostgresCatalogCommandLog {
    async fn append(
        &self,
        command: &CatalogCommand,
    ) -> Result<u64, ExecutableEnvironmentRegistrationError> {
        let (environment_id, revision, kind) = command.identity();
        let revision = i64::try_from(revision)
            .map_err(|_| storage("executable Environment revision exceeds i64 storage"))?;
        let encoded = command.encode()?;
        let mut transaction = self.pool.begin().await.map_err(storage)?;
        let inserted: Option<i64> = sqlx::query_scalar(
            "INSERT INTO executable_environment_command \
             (environment_id, lifecycle_revision, command_kind, command_json) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (environment_id, lifecycle_revision, command_kind) DO NOTHING \
             RETURNING command_sequence",
        )
        .bind(environment_id)
        .bind(revision)
        .bind(kind)
        .bind(&encoded)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(storage)?;
        let sequence = if let Some(sequence) = inserted {
            sequence
        } else {
            let (existing, sequence): (String, i64) = sqlx::query_as(
                "SELECT command_json, command_sequence FROM executable_environment_command \
                 WHERE environment_id = $1 AND lifecycle_revision = $2 AND command_kind = $3",
            )
            .bind(environment_id)
            .bind(revision)
            .bind(kind)
            .fetch_one(&mut *transaction)
            .await
            .map_err(storage)?;
            if existing != encoded {
                return Err(ExecutableEnvironmentRegistrationError::Conflict(format!(
                    "Environment `{environment_id}` revision {revision} already has a different {kind} command"
                )));
            }
            sequence
        };
        transaction.commit().await.map_err(storage)?;
        u64::try_from(sequence)
            .map_err(|_| storage("negative executable Environment command sequence"))
    }
}

#[async_trait]
impl ProjectionLog<CatalogCommand> for PostgresCatalogCommandLog {
    type Error = ExecutableEnvironmentRegistrationError;

    async fn high_water(&self) -> Result<u64, Self::Error> {
        let high: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(command_sequence), 0) FROM executable_environment_command",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(storage)?;
        u64::try_from(high).map_err(|_| storage("negative executable Environment high-water mark"))
    }

    async fn load_range(
        &self,
        after: u64,
        through: u64,
    ) -> Result<Vec<Sequenced<CatalogCommand>>, ExecutableEnvironmentRegistrationError> {
        let after = i64::try_from(after)
            .map_err(|_| storage("executable Environment command sequence exceeds i64 storage"))?;
        let through = i64::try_from(through)
            .map_err(|_| storage("executable Environment high-water mark exceeds i64 storage"))?;
        let rows: Vec<(i64, String, i64, String, String)> = sqlx::query_as(
            "SELECT command_sequence, environment_id, lifecycle_revision, command_kind, command_json \
             FROM executable_environment_command \
             WHERE command_sequence > $1 AND command_sequence <= $2 \
             ORDER BY command_sequence",
        )
        .bind(after)
        .bind(through)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        rows.into_iter()
            .map(|(sequence, id, revision, kind, encoded)| {
                Ok(Sequenced {
                    sequence: u64::try_from(sequence)
                        .map_err(|_| storage("negative executable Environment command sequence"))?,
                    command: CatalogCommand::decode(&id, revision, &kind, &encoded)?,
                })
            })
            .collect()
    }
}

struct DurableRegistrar {
    catalog: Arc<ExecutableEnvironmentCatalog>,
    local: LocalExecutableEnvironmentRegistrar,
    log: Arc<dyn CatalogCommandLog>,
    mutation: Mutex<()>,
    cursor: ProjectionCursor,
}

impl DurableRegistrar {
    async fn hydrate(
        catalog: Arc<ExecutableEnvironmentCatalog>,
        log: Arc<dyn CatalogCommandLog>,
    ) -> Result<Self, ExecutableEnvironmentRegistrationError> {
        let (high_water, commands) = load_full(log.as_ref()).await.map_err(projection_load)?;
        let local = LocalExecutableEnvironmentRegistrar::new(catalog.clone());
        replay_commands(&local, commands).await?;
        Ok(Self {
            catalog,
            local,
            log,
            mutation: Mutex::new(()),
            cursor: ProjectionCursor::at(high_water),
        })
    }

    async fn register(
        &self,
        registration: ExecutableEnvironmentRegistration,
    ) -> Result<ExecutableEnvironmentRegistrationOutcome, ExecutableEnvironmentRegistrationError>
    {
        let _guard = self.mutation.lock().await;
        let expected = self.catalog.preview_registration(registration.clone())?;
        let _sequence = self
            .log
            .append(&CatalogCommand::Registration(Box::new(
                registration.clone(),
            )))
            .await?;
        let actual = self.local.register(registration).await?;
        if actual != expected {
            return Err(storage("catalog changed while persisting registration"));
        }
        Ok(actual)
    }

    async fn refresh_projection(&self) -> Result<(), ExecutableEnvironmentRegistrationError> {
        let _guard = self.mutation.lock().await;
        match load_refresh(&self.cursor, self.log.as_ref())
            .await
            .map_err(projection_load)?
        {
            ProjectionBatch::Current => Ok(()),
            ProjectionBatch::Incremental { through, commands } => {
                let refreshed = clone_catalog(&self.catalog)?;
                replay_commands(
                    &LocalExecutableEnvironmentRegistrar::new(refreshed.clone()),
                    commands,
                )
                .await?;
                install_catalog(&self.catalog, &refreshed)?;
                self.cursor.advance(through);
                Ok(())
            }
            ProjectionBatch::FullReplay { through, commands } => {
                let refreshed = Arc::new(ExecutableEnvironmentCatalog::new());
                replay_commands(
                    &LocalExecutableEnvironmentRegistrar::new(refreshed.clone()),
                    commands,
                )
                .await?;
                install_catalog(&self.catalog, &refreshed)?;
                self.cursor.advance(through);
                Ok(())
            }
        }
    }

    async fn withdraw(
        &self,
        withdrawal: ExecutableEnvironmentWithdrawal,
    ) -> Result<ExecutableEnvironmentWithdrawalOutcome, ExecutableEnvironmentRegistrationError>
    {
        let _guard = self.mutation.lock().await;
        let expected = self.catalog.preview_withdrawal(withdrawal.clone())?;
        let _sequence = self
            .log
            .append(&CatalogCommand::Withdrawal(withdrawal.clone()))
            .await?;
        let actual = self.local.withdraw(withdrawal).await?;
        if actual != expected {
            return Err(storage("catalog changed while persisting withdrawal"));
        }
        Ok(actual)
    }
}

async fn replay_commands(
    local: &LocalExecutableEnvironmentRegistrar,
    commands: Vec<Sequenced<CatalogCommand>>,
) -> Result<(), ExecutableEnvironmentRegistrationError> {
    for command in commands {
        match command.command {
            CatalogCommand::Registration(command) => {
                local
                    .register(*command)
                    .await
                    .map_err(|error| storage(format!("replay persisted registration: {error}")))?;
            }
            CatalogCommand::Withdrawal(command) => {
                local
                    .withdraw(command)
                    .await
                    .map_err(|error| storage(format!("replay persisted withdrawal: {error}")))?;
            }
        }
    }
    Ok(())
}

fn projection_load(
    error: ProjectionLoadError<ExecutableEnvironmentRegistrationError>,
) -> ExecutableEnvironmentRegistrationError {
    match error {
        ProjectionLoadError::Source(error) => error,
        ProjectionLoadError::Incomplete { through } => storage(format!(
            "executable Environment command replay did not reach durable high-water {through}"
        )),
    }
}

fn clone_catalog(
    source: &Arc<ExecutableEnvironmentCatalog>,
) -> Result<Arc<ExecutableEnvironmentCatalog>, ExecutableEnvironmentRegistrationError> {
    let cloned = Arc::new(ExecutableEnvironmentCatalog::new());
    *cloned
        .state
        .write()
        .map_err(|_| storage("cloned executable Environment catalog lock poisoned"))? = source
        .state
        .read()
        .map_err(|_| storage("executable Environment catalog lock poisoned"))?
        .clone();
    Ok(cloned)
}

fn install_catalog(
    target: &Arc<ExecutableEnvironmentCatalog>,
    refreshed: &Arc<ExecutableEnvironmentCatalog>,
) -> Result<(), ExecutableEnvironmentRegistrationError> {
    let state = refreshed
        .state
        .read()
        .map_err(|_| storage("refreshed executable Environment catalog lock poisoned"))?
        .clone();
    *target
        .state
        .write()
        .map_err(|_| storage("executable Environment catalog lock poisoned"))? = state;
    Ok(())
}

#[derive(Clone)]
pub struct PostgresExecutableEnvironmentRegistrar {
    inner: Arc<DurableRegistrar>,
}

impl PostgresExecutableEnvironmentRegistrar {
    /// Advance this Coordinator replica from the durable high-water mark before
    /// Runtime work, falling back to full replay on cursor/tail inconsistency.
    pub async fn refresh_projection(&self) -> Result<(), ExecutableEnvironmentRegistrationError> {
        self.inner.refresh_projection().await
    }

    pub async fn connect(
        url: &str,
        catalog: Arc<ExecutableEnvironmentCatalog>,
    ) -> Result<Self, ExecutableEnvironmentRegistrationError> {
        let pool = PgPool::connect(url).await.map_err(storage)?;
        Self::with_pool(pool, catalog).await
    }

    pub async fn with_pool(
        pool: PgPool,
        catalog: Arc<ExecutableEnvironmentCatalog>,
    ) -> Result<Self, ExecutableEnvironmentRegistrationError> {
        let bundle = executable_environment_catalog_bundle().map_err(storage)?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(storage)?
            .run_bundle(&bundle)
            .await
            .map_err(storage)?;
        Self::from_pool(pool, catalog).await
    }

    pub async fn connect_existing(
        url: &str,
        catalog: Arc<ExecutableEnvironmentCatalog>,
    ) -> Result<Self, ExecutableEnvironmentRegistrationError> {
        let pool = PgPool::connect(url).await.map_err(storage)?;
        let bundle = executable_environment_catalog_bundle().map_err(storage)?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(storage)?
            .verify_bundle(&bundle)
            .await
            .map_err(storage)?;
        Self::from_pool(pool, catalog).await
    }

    async fn from_pool(
        pool: PgPool,
        catalog: Arc<ExecutableEnvironmentCatalog>,
    ) -> Result<Self, ExecutableEnvironmentRegistrationError> {
        let log: Arc<dyn CatalogCommandLog> = Arc::new(PostgresCatalogCommandLog { pool });
        Ok(Self {
            inner: Arc::new(DurableRegistrar::hydrate(catalog, log).await?),
        })
    }
}

#[async_trait]
impl ExecutableEnvironmentRegistrar for PostgresExecutableEnvironmentRegistrar {
    async fn register(
        &self,
        registration: ExecutableEnvironmentRegistration,
    ) -> Result<ExecutableEnvironmentRegistrationOutcome, ExecutableEnvironmentRegistrationError>
    {
        self.inner.register(registration).await
    }

    async fn withdraw(
        &self,
        withdrawal: ExecutableEnvironmentWithdrawal,
    ) -> Result<ExecutableEnvironmentWithdrawalOutcome, ExecutableEnvironmentRegistrationError>
    {
        self.inner.withdraw(withdrawal).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use awaken_environment_contract::{EnvItem, EnvironmentConfig, EnvironmentRevision};
    use sqlx::Executor;
    use sqlx::postgres::PgPoolOptions;

    use super::*;

    #[derive(Default)]
    struct MemoryCommandLog {
        commands: StdMutex<Vec<Sequenced<CatalogCommand>>>,
        drop_incremental_tail_once: AtomicBool,
        load_after_calls: StdMutex<Vec<u64>>,
    }

    #[async_trait]
    impl CatalogCommandLog for MemoryCommandLog {
        async fn append(
            &self,
            command: &CatalogCommand,
        ) -> Result<u64, ExecutableEnvironmentRegistrationError> {
            let mut commands = self.commands.lock().unwrap();
            if let Some(existing) = commands
                .iter()
                .find(|existing| existing.command.identity() == command.identity())
            {
                return if existing.command == *command {
                    Ok(existing.sequence)
                } else {
                    Err(ExecutableEnvironmentRegistrationError::Conflict(
                        "different command at the same identity".into(),
                    ))
                };
            }
            let sequence = commands
                .last()
                .map_or(2, |command| command.sequence.saturating_add(2));
            commands.push(Sequenced {
                sequence,
                command: command.clone(),
            });
            Ok(sequence)
        }
    }

    #[async_trait]
    impl ProjectionLog<CatalogCommand> for MemoryCommandLog {
        type Error = ExecutableEnvironmentRegistrationError;

        async fn high_water(&self) -> Result<u64, Self::Error> {
            Ok(self
                .commands
                .lock()
                .unwrap()
                .last()
                .map_or(0, |command| command.sequence))
        }

        async fn load_range(
            &self,
            after: u64,
            through: u64,
        ) -> Result<Vec<Sequenced<CatalogCommand>>, ExecutableEnvironmentRegistrationError>
        {
            self.load_after_calls.lock().unwrap().push(after);
            let mut commands = self
                .commands
                .lock()
                .unwrap()
                .iter()
                .filter(|command| command.sequence > after && command.sequence <= through)
                .cloned()
                .collect::<Vec<_>>();
            if after > 0
                && self
                    .drop_incremental_tail_once
                    .swap(false, Ordering::SeqCst)
            {
                commands.pop();
            }
            Ok(commands)
        }
    }

    fn registration() -> ExecutableEnvironmentRegistration {
        ExecutableEnvironmentRegistration::new(
            EnvItem {
                id: "env-a".into(),
                revision: EnvironmentRevision(1),
                name: "Environment A".into(),
                description: None,
                metadata: Default::default(),
                scope: None,
                config: EnvironmentConfig::SelfHosted,
                sandbox_policy: None,
                archived_at: None,
            },
            None,
        )
    }

    fn registration_at(revision: u64, name: &str) -> ExecutableEnvironmentRegistration {
        let mut definition = registration().definition;
        definition.revision = EnvironmentRevision(revision);
        definition.name = name.into();
        ExecutableEnvironmentRegistration::new(definition, None)
    }

    async fn registrar(
        log: Arc<MemoryCommandLog>,
        catalog: Arc<ExecutableEnvironmentCatalog>,
    ) -> DurableRegistrar {
        DurableRegistrar::hydrate(catalog, log).await.unwrap()
    }

    #[tokio::test]
    async fn active_active_peer_refreshes_the_durable_environment_projection() {
        // Cause/effect decision table: R1 peers start empty; R2 authority ahead
        // -> stale peer loads only commands above its durable cursor; R3 unchanged
        // high-water -> no command read; R4 incomplete incremental tail -> full
        // replay from durable truth. Deliberate sequence gaps prove no contiguous
        // identity assumption leaks into the projection.
        let log = Arc::new(MemoryCommandLog::default());
        let left_catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let right_catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let left = registrar(log.clone(), left_catalog.clone()).await;
        let right = registrar(log.clone(), right_catalog.clone()).await;
        assert!(left_catalog.current("env-a").is_none(), "R1");
        assert!(right_catalog.current("env-a").is_none(), "R1");

        left.register(registration()).await.unwrap();
        assert!(left_catalog.current("env-a").is_some(), "R2");
        assert!(right_catalog.current("env-a").is_none(), "R2");

        right.refresh_projection().await.unwrap();
        assert_eq!(
            right_catalog.current("env-a").unwrap().definition.name,
            "Environment A",
            "R3"
        );
        let reads_after_first_refresh = log.load_after_calls.lock().unwrap().len();
        right.refresh_projection().await.unwrap();
        assert_eq!(
            log.load_after_calls.lock().unwrap().len(),
            reads_after_first_refresh,
            "R3"
        );

        left.register(registration_at(2, "Environment B"))
            .await
            .unwrap();
        right.refresh_projection().await.unwrap();
        assert_eq!(
            *log.load_after_calls.lock().unwrap().last().unwrap(),
            2,
            "R2"
        );
        assert_eq!(
            right_catalog.current("env-a").unwrap().definition.name,
            "Environment B",
            "R2"
        );

        left.register(registration_at(3, "Environment C"))
            .await
            .unwrap();
        log.drop_incremental_tail_once.store(true, Ordering::SeqCst);
        right.refresh_projection().await.unwrap();
        let calls = log.load_after_calls.lock().unwrap();
        assert_eq!(&calls[calls.len() - 2..], &[4, 0], "R4");
        assert_eq!(
            right_catalog.current("env-a").unwrap().definition.name,
            "Environment C",
            "R4"
        );
        assert_eq!(log.commands.lock().unwrap().len(), 3, "R4");
    }

    #[tokio::test]
    async fn postgres_migration_persists_high_water_and_rehydrates_environment() {
        // Cause/effect decision table: R1 PostgreSQL is unavailable -> the
        // optional integration case self-skips; R2 a fresh authority applies
        // deterministic V1/V2 and accepts revision 1 -> the command has a
        // positive durable sequence; R3 connect_existing runs no DDL and a new
        // replica full-replays the same Environment definition.
        const SCHEMA: &str = "t_executable_environment_catalog";
        let database_url = std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".into()
        });
        let Ok(admin) = PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_secs(1))
            .connect(&database_url)
            .await
        else {
            eprintln!("[skip] no PostgreSQL reachable for executable Environment catalog test");
            return;
        };
        admin
            .execute(format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE").as_str())
            .await
            .unwrap();
        admin
            .execute(format!("CREATE SCHEMA {SCHEMA}").as_str())
            .await
            .unwrap();
        let pool = PgPoolOptions::new()
            .after_connect(|connection, _| {
                Box::pin(async move {
                    connection
                        .execute(format!("SET search_path = {SCHEMA}").as_str())
                        .await?;
                    Ok(())
                })
            })
            .connect(&database_url)
            .await
            .unwrap();
        let first_catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let first = PostgresExecutableEnvironmentRegistrar::with_pool(pool.clone(), first_catalog)
            .await
            .unwrap();
        first.register(registration()).await.unwrap();
        let high_water: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(command_sequence), 0) FROM executable_environment_command",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(high_water > 0, "R2");
        drop(first);

        let separator = if database_url.contains('?') { '&' } else { '?' };
        let scoped_url = format!("{database_url}{separator}options=-c%20search_path%3D{SCHEMA}");
        let rebuilt = Arc::new(ExecutableEnvironmentCatalog::new());
        let _restarted =
            PostgresExecutableEnvironmentRegistrar::connect_existing(&scoped_url, rebuilt.clone())
                .await
                .unwrap();
        assert_eq!(
            rebuilt.current("env-a").unwrap().definition.name,
            "Environment A",
            "R3"
        );

        pool.close().await;
        admin
            .execute(format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE").as_str())
            .await
            .unwrap();
        admin.close().await;
    }
}
