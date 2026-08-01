//! PostgreSQL durability adapter for the executable Environment projection.
//!
//! Only boundary commands are persisted. Startup and every mutation replay the
//! canonical in-memory catalog state machine, so SQL never becomes a parallel
//! lifecycle implementation.

use std::sync::Arc;

use async_trait::async_trait;
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
trait CatalogCommandLog: Send + Sync {
    async fn append(
        &self,
        command: &CatalogCommand,
    ) -> Result<(), ExecutableEnvironmentRegistrationError>;
    async fn load(&self) -> Result<Vec<CatalogCommand>, ExecutableEnvironmentRegistrationError>;
}

struct PostgresCatalogCommandLog {
    pool: PgPool,
}

#[async_trait]
impl CatalogCommandLog for PostgresCatalogCommandLog {
    async fn append(
        &self,
        command: &CatalogCommand,
    ) -> Result<(), ExecutableEnvironmentRegistrationError> {
        let (environment_id, revision, kind) = command.identity();
        let revision = i64::try_from(revision)
            .map_err(|_| storage("executable Environment revision exceeds i64 storage"))?;
        let encoded = command.encode()?;
        let mut transaction = self.pool.begin().await.map_err(storage)?;
        let inserted = sqlx::query(
            "INSERT INTO executable_environment_command \
             (environment_id, lifecycle_revision, command_kind, command_json) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (environment_id, lifecycle_revision, command_kind) DO NOTHING",
        )
        .bind(environment_id)
        .bind(revision)
        .bind(kind)
        .bind(&encoded)
        .execute(&mut *transaction)
        .await
        .map_err(storage)?
        .rows_affected();
        if inserted == 0 {
            let existing: String = sqlx::query_scalar(
                "SELECT command_json FROM executable_environment_command \
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
        }
        transaction.commit().await.map_err(storage)
    }

    async fn load(&self) -> Result<Vec<CatalogCommand>, ExecutableEnvironmentRegistrationError> {
        let rows: Vec<(String, i64, String, String)> = sqlx::query_as(
            "SELECT environment_id, lifecycle_revision, command_kind, command_json \
             FROM executable_environment_command \
             ORDER BY environment_id, lifecycle_revision, command_kind",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        rows.into_iter()
            .map(|(id, revision, kind, encoded)| {
                CatalogCommand::decode(&id, revision, &kind, &encoded)
            })
            .collect()
    }
}

struct DurableRegistrar {
    catalog: Arc<ExecutableEnvironmentCatalog>,
    local: LocalExecutableEnvironmentRegistrar,
    log: Arc<dyn CatalogCommandLog>,
    mutation: Mutex<()>,
}

impl DurableRegistrar {
    async fn hydrate(
        catalog: Arc<ExecutableEnvironmentCatalog>,
        log: Arc<dyn CatalogCommandLog>,
    ) -> Result<Self, ExecutableEnvironmentRegistrationError> {
        let local = LocalExecutableEnvironmentRegistrar::new(catalog.clone());
        for command in log.load().await? {
            match command {
                CatalogCommand::Registration(command) => {
                    local.register(*command).await?;
                }
                CatalogCommand::Withdrawal(command) => {
                    local.withdraw(command).await?;
                }
            }
        }
        Ok(Self {
            catalog,
            local,
            log,
            mutation: Mutex::new(()),
        })
    }

    async fn register(
        &self,
        registration: ExecutableEnvironmentRegistration,
    ) -> Result<ExecutableEnvironmentRegistrationOutcome, ExecutableEnvironmentRegistrationError>
    {
        let _guard = self.mutation.lock().await;
        let expected = self.catalog.preview_registration(registration.clone())?;
        self.log
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
        let refreshed = Arc::new(ExecutableEnvironmentCatalog::new());
        let local = LocalExecutableEnvironmentRegistrar::new(refreshed.clone());
        for command in self.log.load().await? {
            match command {
                CatalogCommand::Registration(command) => {
                    local.register(*command).await.map_err(|error| {
                        storage(format!("replay persisted registration: {error}"))
                    })?;
                }
                CatalogCommand::Withdrawal(command) => {
                    local.withdraw(command).await.map_err(|error| {
                        storage(format!("replay persisted withdrawal: {error}"))
                    })?;
                }
            }
        }
        let state = refreshed
            .state
            .read()
            .map_err(|_| storage("refreshed executable Environment catalog lock poisoned"))?
            .clone();
        *self
            .catalog
            .state
            .write()
            .map_err(|_| storage("executable Environment catalog lock poisoned"))? = state;
        Ok(())
    }

    async fn withdraw(
        &self,
        withdrawal: ExecutableEnvironmentWithdrawal,
    ) -> Result<ExecutableEnvironmentWithdrawalOutcome, ExecutableEnvironmentRegistrationError>
    {
        let _guard = self.mutation.lock().await;
        let expected = self.catalog.preview_withdrawal(withdrawal.clone())?;
        self.log
            .append(&CatalogCommand::Withdrawal(withdrawal.clone()))
            .await?;
        let actual = self.local.withdraw(withdrawal).await?;
        if actual != expected {
            return Err(storage("catalog changed while persisting withdrawal"));
        }
        Ok(actual)
    }
}

#[derive(Clone)]
pub struct PostgresExecutableEnvironmentRegistrar {
    inner: Arc<DurableRegistrar>,
}

impl PostgresExecutableEnvironmentRegistrar {
    /// Rebuild this Coordinator replica's read projection from the one durable
    /// command log before admitting or resuming Runtime work.
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

    use awaken_environment_contract::{EnvItem, EnvironmentConfig, EnvironmentRevision};

    use super::*;

    #[derive(Default)]
    struct MemoryCommandLog {
        commands: StdMutex<Vec<CatalogCommand>>,
    }

    #[async_trait]
    impl CatalogCommandLog for MemoryCommandLog {
        async fn append(
            &self,
            command: &CatalogCommand,
        ) -> Result<(), ExecutableEnvironmentRegistrationError> {
            let mut commands = self.commands.lock().unwrap();
            if let Some(existing) = commands
                .iter()
                .find(|existing| existing.identity() == command.identity())
            {
                return if existing == command {
                    Ok(())
                } else {
                    Err(ExecutableEnvironmentRegistrationError::Conflict(
                        "different command at the same identity".into(),
                    ))
                };
            }
            commands.push(command.clone());
            Ok(())
        }

        async fn load(
            &self,
        ) -> Result<Vec<CatalogCommand>, ExecutableEnvironmentRegistrationError> {
            Ok(self.commands.lock().unwrap().clone())
        }
    }

    fn registration() -> ExecutableEnvironmentRegistration {
        ExecutableEnvironmentRegistration::new(
            EnvItem {
                id: "env-a".into(),
                revision: EnvironmentRevision(1),
                name: "Environment A".into(),
                description: String::new(),
                metadata: Default::default(),
                scope: None,
                config: EnvironmentConfig::SelfHosted,
                sandbox_policy: None,
                archived_at: None,
            },
            None,
        )
    }

    async fn registrar(
        log: Arc<MemoryCommandLog>,
        catalog: Arc<ExecutableEnvironmentCatalog>,
    ) -> DurableRegistrar {
        DurableRegistrar::hydrate(catalog, log).await.unwrap()
    }

    #[tokio::test]
    async fn active_active_peer_refreshes_the_durable_environment_projection() {
        // Cause/effect decision table: R1 two replicas hydrate before a command
        // -> both are empty; R2 replica L registers -> the durable log and L
        // advance while R remains stale; R3 R refreshes before Session admission
        // -> R sees the exact Environment; R4 another refresh -> idempotent and
        // the command log still contains exactly one command.
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
        right.refresh_projection().await.unwrap();
        assert_eq!(log.commands.lock().unwrap().len(), 1, "R4");
    }
}
