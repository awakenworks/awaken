//! PostgreSQL durability adapter for executable Agent registration.
//!
//! The database stores the existing boundary commands. It does not reproduce
//! catalog transition rules: construction and every mutation replay commands
//! through [`ExecutableAgentCatalog`], the sole state machine.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_executable_agent_contract::{
    ExecutableAgentRegistrar, ExecutableAgentRegistration, ExecutableAgentRegistrationError,
    ExecutableAgentRegistrationOutcome, ExecutableAgentWithdrawal,
    ExecutableAgentWithdrawalOutcome,
};
use sqlx::postgres::PgPool;
use tokio::sync::Mutex;

use crate::schema::{NS, executable_agent_catalog_bundle};
use crate::{ExecutableAgentCatalog, LocalExecutableAgentRegistrar};

const REGISTRATION_KIND: &str = "registration";
const WITHDRAWAL_KIND: &str = "withdrawal";

#[derive(Clone, Debug, PartialEq)]
enum CatalogCommand {
    Registration(Box<ExecutableAgentRegistration>),
    Withdrawal(ExecutableAgentWithdrawal),
}

impl CatalogCommand {
    fn identity(&self) -> (&str, &str, u64, &'static str) {
        match self {
            Self::Registration(command) => (
                &command.workspace_id,
                &command.agent_id,
                command.source_revision,
                REGISTRATION_KIND,
            ),
            Self::Withdrawal(command) => (
                &command.workspace_id,
                &command.agent_id,
                command.lifecycle_revision,
                WITHDRAWAL_KIND,
            ),
        }
    }

    fn encode(&self) -> Result<String, ExecutableAgentRegistrationError> {
        match self {
            Self::Registration(command) => serde_json::to_string(command),
            Self::Withdrawal(command) => serde_json::to_string(command),
        }
        .map_err(storage)
    }

    fn decode(
        workspace_id: &str,
        agent_id: &str,
        lifecycle_revision: i64,
        kind: &str,
        encoded: &str,
    ) -> Result<Self, ExecutableAgentRegistrationError> {
        let revision = u64::try_from(lifecycle_revision)
            .map_err(|_| storage("negative executable Agent lifecycle revision"))?;
        let command = match kind {
            REGISTRATION_KIND => Self::Registration(Box::new(
                serde_json::from_str(encoded).map_err(|error| corrupt(kind, error))?,
            )),
            WITHDRAWAL_KIND => Self::Withdrawal(
                serde_json::from_str(encoded).map_err(|error| corrupt(kind, error))?,
            ),
            other => return Err(storage(format!("unknown catalog command kind `{other}`"))),
        };
        let (stored_workspace, stored_agent, stored_revision, stored_kind) = command.identity();
        if stored_workspace != workspace_id
            || stored_agent != agent_id
            || stored_revision != revision
            || stored_kind != kind
        {
            return Err(storage(
                "catalog command columns do not match the serialized command",
            ));
        }
        Ok(command)
    }
}

fn storage(error: impl std::fmt::Display) -> ExecutableAgentRegistrationError {
    ExecutableAgentRegistrationError::Storage(error.to_string())
}

fn corrupt(kind: &str, error: impl std::fmt::Display) -> ExecutableAgentRegistrationError {
    storage(format!("decode persisted {kind} command: {error}"))
}

#[async_trait]
trait CatalogCommandLog: Send + Sync {
    async fn append(
        &self,
        command: &CatalogCommand,
    ) -> Result<(), ExecutableAgentRegistrationError>;

    async fn load(&self) -> Result<Vec<CatalogCommand>, ExecutableAgentRegistrationError>;
}

struct PostgresCatalogCommandLog {
    pool: PgPool,
}

#[async_trait]
impl CatalogCommandLog for PostgresCatalogCommandLog {
    async fn append(
        &self,
        command: &CatalogCommand,
    ) -> Result<(), ExecutableAgentRegistrationError> {
        let (workspace_id, agent_id, revision, kind) = command.identity();
        let revision = i64::try_from(revision)
            .map_err(|_| storage("executable Agent lifecycle revision exceeds i64 storage"))?;
        let encoded = command.encode()?;
        let mut transaction = self.pool.begin().await.map_err(storage)?;
        let inserted = sqlx::query(
            "INSERT INTO executable_agent_command \
                (workspace_id, agent_id, lifecycle_revision, command_kind, command_json) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (workspace_id, agent_id, lifecycle_revision, command_kind) \
             DO NOTHING",
        )
        .bind(workspace_id)
        .bind(agent_id)
        .bind(revision)
        .bind(kind)
        .bind(&encoded)
        .execute(&mut *transaction)
        .await
        .map_err(storage)?
        .rows_affected();
        if inserted == 0 {
            let existing: String = sqlx::query_scalar(
                "SELECT command_json FROM executable_agent_command \
                 WHERE workspace_id = $1 AND agent_id = $2 \
                   AND lifecycle_revision = $3 AND command_kind = $4",
            )
            .bind(workspace_id)
            .bind(agent_id)
            .bind(revision)
            .bind(kind)
            .fetch_one(&mut *transaction)
            .await
            .map_err(storage)?;
            if existing != encoded {
                return Err(ExecutableAgentRegistrationError::Conflict(format!(
                    "Workspace `{workspace_id}` Agent `{agent_id}` revision {revision} \
                     already has a different {kind} command"
                )));
            }
        }
        transaction.commit().await.map_err(storage)
    }

    async fn load(&self) -> Result<Vec<CatalogCommand>, ExecutableAgentRegistrationError> {
        let rows: Vec<(String, String, i64, String, String)> = sqlx::query_as(
            "SELECT workspace_id, agent_id, lifecycle_revision, command_kind, command_json \
             FROM executable_agent_command \
             ORDER BY workspace_id, agent_id, lifecycle_revision, command_kind",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        rows.into_iter()
            .map(|(workspace, agent, revision, kind, encoded)| {
                CatalogCommand::decode(&workspace, &agent, revision, &kind, &encoded)
            })
            .collect()
    }
}

struct DurableRegistrar {
    catalog: Arc<ExecutableAgentCatalog>,
    local: LocalExecutableAgentRegistrar,
    log: Arc<dyn CatalogCommandLog>,
    mutation: Mutex<()>,
}

impl DurableRegistrar {
    async fn hydrate(
        catalog: Arc<ExecutableAgentCatalog>,
        log: Arc<dyn CatalogCommandLog>,
    ) -> Result<Self, ExecutableAgentRegistrationError> {
        let local = LocalExecutableAgentRegistrar::new(catalog.clone());
        for command in log.load().await? {
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
        Ok(Self {
            catalog,
            local,
            log,
            mutation: Mutex::new(()),
        })
    }

    async fn register(
        &self,
        registration: ExecutableAgentRegistration,
    ) -> Result<ExecutableAgentRegistrationOutcome, ExecutableAgentRegistrationError> {
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

    async fn withdraw(
        &self,
        withdrawal: ExecutableAgentWithdrawal,
    ) -> Result<ExecutableAgentWithdrawalOutcome, ExecutableAgentRegistrationError> {
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

/// Coordinator PostgreSQL adapter. It acknowledges a command only after the
/// exact command is durable, then applies it through the canonical catalog state
/// machine. A fresh catalog is rehydrated before this constructor returns.
#[derive(Clone)]
pub struct PostgresExecutableAgentRegistrar {
    inner: Arc<DurableRegistrar>,
}

impl PostgresExecutableAgentRegistrar {
    /// Connect and apply the scoped catalog migration.
    pub async fn connect(
        url: &str,
        catalog: Arc<ExecutableAgentCatalog>,
    ) -> Result<Self, ExecutableAgentRegistrationError> {
        let pool = PgPool::connect(url).await.map_err(storage)?;
        Self::with_pool(pool, catalog).await
    }

    /// Reuse a pool and apply the scoped catalog migration.
    pub async fn with_pool(
        pool: PgPool,
        catalog: Arc<ExecutableAgentCatalog>,
    ) -> Result<Self, ExecutableAgentRegistrationError> {
        let bundle = executable_agent_catalog_bundle().map_err(storage)?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(storage)?
            .run_bundle(&bundle)
            .await
            .map_err(storage)?;
        Self::from_pool(pool, catalog).await
    }

    /// Connect to a schema prepared by the deployment migration command without
    /// issuing DDL at application startup.
    pub async fn connect_existing(
        url: &str,
        catalog: Arc<ExecutableAgentCatalog>,
    ) -> Result<Self, ExecutableAgentRegistrationError> {
        let pool = PgPool::connect(url).await.map_err(storage)?;
        let bundle = executable_agent_catalog_bundle().map_err(storage)?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(storage)?
            .verify_bundle(&bundle)
            .await
            .map_err(storage)?;
        Self::from_pool(pool, catalog).await
    }

    async fn from_pool(
        pool: PgPool,
        catalog: Arc<ExecutableAgentCatalog>,
    ) -> Result<Self, ExecutableAgentRegistrationError> {
        let log: Arc<dyn CatalogCommandLog> = Arc::new(PostgresCatalogCommandLog { pool });
        Ok(Self {
            inner: Arc::new(DurableRegistrar::hydrate(catalog, log).await?),
        })
    }
}

#[async_trait]
impl ExecutableAgentRegistrar for PostgresExecutableAgentRegistrar {
    async fn register(
        &self,
        registration: ExecutableAgentRegistration,
    ) -> Result<ExecutableAgentRegistrationOutcome, ExecutableAgentRegistrationError> {
        self.inner.register(registration).await
    }

    async fn withdraw(
        &self,
        withdrawal: ExecutableAgentWithdrawal,
    ) -> Result<ExecutableAgentWithdrawalOutcome, ExecutableAgentRegistrationError> {
        self.inner.withdraw(withdrawal).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::test_support::registration as test_registration;
    use sqlx::Executor;
    use sqlx::postgres::PgPoolOptions;

    fn registration() -> ExecutableAgentRegistration {
        test_registration(7, "fp-a")
    }

    #[derive(Default)]
    struct MemoryCommandLog {
        commands: StdMutex<Vec<CatalogCommand>>,
        fail_next_append: AtomicBool,
    }

    #[async_trait]
    impl CatalogCommandLog for MemoryCommandLog {
        async fn append(
            &self,
            command: &CatalogCommand,
        ) -> Result<(), ExecutableAgentRegistrationError> {
            if self.fail_next_append.swap(false, Ordering::SeqCst) {
                return Err(storage("injected append failure"));
            }
            let mut commands = self.commands.lock().unwrap();
            if let Some(existing) = commands
                .iter()
                .find(|existing| existing.identity() == command.identity())
            {
                return if existing == command {
                    Ok(())
                } else {
                    Err(ExecutableAgentRegistrationError::Conflict(
                        "incompatible persisted command".into(),
                    ))
                };
            }
            commands.push(command.clone());
            commands.sort_by_key(|command| {
                let (workspace, agent, revision, kind) = command.identity();
                (workspace.to_owned(), agent.to_owned(), revision, kind)
            });
            Ok(())
        }

        async fn load(&self) -> Result<Vec<CatalogCommand>, ExecutableAgentRegistrationError> {
            Ok(self.commands.lock().unwrap().clone())
        }
    }

    async fn registrar(
        log: Arc<MemoryCommandLog>,
        catalog: Arc<ExecutableAgentCatalog>,
    ) -> DurableRegistrar {
        DurableRegistrar::hydrate(catalog, log).await.unwrap()
    }

    #[tokio::test]
    async fn persistence_precedes_projection_and_failure_leaves_catalog_unchanged() {
        // Cause/effect table: P1 append succeeds -> durable command and current
        // projection; P2 append fails -> Storage and no projection mutation; P3
        // exact retry -> one command and AlreadyRegistered; P4 semantic conflict
        // -> no append and the prior current value remains authoritative.
        let log = Arc::new(MemoryCommandLog::default());
        let catalog = Arc::new(ExecutableAgentCatalog::new());
        let registrar = registrar(log.clone(), catalog.clone()).await;
        log.fail_next_append.store(true, Ordering::SeqCst);
        assert!(
            matches!(
                registrar.register(registration()).await,
                Err(ExecutableAgentRegistrationError::Storage(_))
            ),
            "P2"
        );
        assert!(catalog.current("workspace-a", "agent-a").is_none(), "P2");

        assert_eq!(
            registrar.register(registration()).await.unwrap(),
            ExecutableAgentRegistrationOutcome::RegisteredCurrent,
            "P1"
        );
        assert_eq!(log.commands.lock().unwrap().len(), 1, "P1");
        assert_eq!(
            registrar.register(registration()).await.unwrap(),
            ExecutableAgentRegistrationOutcome::AlreadyRegistered,
            "P3"
        );
        assert_eq!(log.commands.lock().unwrap().len(), 1, "P3");

        let mut conflict = registration();
        conflict.declared_hand = Some("other-hand".into());
        assert!(
            matches!(
                registrar.register(conflict).await,
                Err(ExecutableAgentRegistrationError::Conflict(_))
            ),
            "P4"
        );
        assert_eq!(log.commands.lock().unwrap().len(), 1, "P4");
        assert_eq!(
            catalog
                .current("workspace-a", "agent-a")
                .unwrap()
                .declared_hand,
            Some("hand-a".into()),
            "P4"
        );
    }

    #[tokio::test]
    async fn restart_replays_registration_and_withdrawal_through_canonical_state_machine() {
        // Causes: registration followed by a higher lifecycle withdrawal, then
        // process restart. Effects: the rebuilt current pointer is unavailable,
        // exact immutable history remains readable, and another withdrawal is
        // idempotent without a duplicate durable command.
        let log = Arc::new(MemoryCommandLog::default());
        let first_catalog = Arc::new(ExecutableAgentCatalog::new());
        let first = registrar(log.clone(), first_catalog).await;
        first.register(registration()).await.unwrap();
        let withdrawal = ExecutableAgentWithdrawal {
            workspace_id: "workspace-a".into(),
            agent_id: "agent-a".into(),
            lifecycle_revision: 8,
        };
        first.withdraw(withdrawal.clone()).await.unwrap();
        assert_eq!(log.commands.lock().unwrap().len(), 2);

        let rebuilt = Arc::new(ExecutableAgentCatalog::new());
        let restarted = registrar(log.clone(), rebuilt.clone()).await;
        assert!(rebuilt.is_unavailable("workspace-a", "agent-a"));
        assert!(rebuilt.exact("workspace-a", "fp-a").is_some());
        assert_eq!(
            restarted.withdraw(withdrawal).await.unwrap(),
            ExecutableAgentWithdrawalOutcome::AlreadyWithdrawn
        );
        assert_eq!(log.commands.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn postgres_adapter_migrates_persists_verifies_and_rehydrates() {
        // Cause/effect integration rule: a reachable PostgreSQL authority starts
        // with no catalog schema; migration + registration persists one command;
        // connect_existing performs no alternate migration path and a fresh
        // process projection rehydrates the exact current publication.
        const SCHEMA: &str = "t_executable_agent_catalog";
        let database_url = std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".into()
        });
        let Ok(admin) = PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_secs(1))
            .connect(&database_url)
            .await
        else {
            eprintln!("[skip] no PostgreSQL reachable for executable Agent catalog test");
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
        let first_catalog = Arc::new(ExecutableAgentCatalog::new());
        let first = PostgresExecutableAgentRegistrar::with_pool(pool.clone(), first_catalog)
            .await
            .unwrap();
        first.register(registration()).await.unwrap();
        drop(first);

        let separator = if database_url.contains('?') { '&' } else { '?' };
        let scoped_url = format!("{database_url}{separator}options=-c%20search_path%3D{SCHEMA}");
        let rebuilt = Arc::new(ExecutableAgentCatalog::new());
        let _restarted =
            PostgresExecutableAgentRegistrar::connect_existing(&scoped_url, rebuilt.clone())
                .await
                .unwrap();
        assert_eq!(
            rebuilt
                .current("workspace-a", "agent-a")
                .unwrap()
                .snapshot
                .fingerprint
                .0,
            "fp-a"
        );

        pool.close().await;
        admin
            .execute(format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE").as_str())
            .await
            .unwrap();
        admin.close().await;
    }
}
