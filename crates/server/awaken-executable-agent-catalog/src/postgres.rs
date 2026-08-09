//! PostgreSQL durability adapter for executable Agent registration.
//!
//! The database stores the existing boundary commands. It does not reproduce
//! catalog transition rules: construction and every mutation replay commands
//! through [`ExecutableAgentCatalog`], the sole state machine.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_durable_projection::{
    ProjectionBatch, ProjectionCursor, ProjectionLoadError, ProjectionLog, Sequenced, load_full,
    load_refresh,
};
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
trait CatalogCommandLog:
    ProjectionLog<CatalogCommand, Error = ExecutableAgentRegistrationError> + Send + Sync
{
    async fn append(
        &self,
        command: &CatalogCommand,
    ) -> Result<u64, ExecutableAgentRegistrationError>;
}

struct PostgresCatalogCommandLog {
    pool: PgPool,
}

#[async_trait]
impl CatalogCommandLog for PostgresCatalogCommandLog {
    async fn append(
        &self,
        command: &CatalogCommand,
    ) -> Result<u64, ExecutableAgentRegistrationError> {
        let (workspace_id, agent_id, revision, kind) = command.identity();
        let revision = i64::try_from(revision)
            .map_err(|_| storage("executable Agent lifecycle revision exceeds i64 storage"))?;
        let encoded = command.encode()?;
        let mut transaction = self.pool.begin().await.map_err(storage)?;
        let inserted: Option<i64> = sqlx::query_scalar(
            "INSERT INTO executable_agent_command \
                (workspace_id, agent_id, lifecycle_revision, command_kind, command_json) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (workspace_id, agent_id, lifecycle_revision, command_kind) \
             DO NOTHING \
             RETURNING command_sequence",
        )
        .bind(workspace_id)
        .bind(agent_id)
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
                "SELECT command_json, command_sequence FROM executable_agent_command \
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
            sequence
        };
        transaction.commit().await.map_err(storage)?;
        u64::try_from(sequence).map_err(|_| storage("negative executable Agent command sequence"))
    }
}

#[async_trait]
impl ProjectionLog<CatalogCommand> for PostgresCatalogCommandLog {
    type Error = ExecutableAgentRegistrationError;

    async fn high_water(&self) -> Result<u64, Self::Error> {
        let high: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(command_sequence), 0) FROM executable_agent_command",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(storage)?;
        u64::try_from(high).map_err(|_| storage("negative executable Agent high-water mark"))
    }

    async fn load_range(
        &self,
        after: u64,
        through: u64,
    ) -> Result<Vec<Sequenced<CatalogCommand>>, ExecutableAgentRegistrationError> {
        let after = i64::try_from(after)
            .map_err(|_| storage("executable Agent command sequence exceeds i64 storage"))?;
        let through = i64::try_from(through)
            .map_err(|_| storage("executable Agent high-water mark exceeds i64 storage"))?;
        let rows: Vec<(i64, String, String, i64, String, String)> = sqlx::query_as(
            "SELECT command_sequence, workspace_id, agent_id, lifecycle_revision, command_kind, command_json \
             FROM executable_agent_command \
             WHERE command_sequence > $1 AND command_sequence <= $2 \
             ORDER BY command_sequence",
        )
        .bind(after)
        .bind(through)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        rows.into_iter()
            .map(|(sequence, workspace, agent, revision, kind, encoded)| {
                Ok(Sequenced {
                    sequence: u64::try_from(sequence)
                        .map_err(|_| storage("negative executable Agent command sequence"))?,
                    command: CatalogCommand::decode(&workspace, &agent, revision, &kind, &encoded)?,
                })
            })
            .collect()
    }
}

struct DurableRegistrar {
    catalog: Arc<ExecutableAgentCatalog>,
    local: LocalExecutableAgentRegistrar,
    log: Arc<dyn CatalogCommandLog>,
    mutation: Mutex<()>,
    cursor: ProjectionCursor,
}

impl DurableRegistrar {
    async fn hydrate(
        catalog: Arc<ExecutableAgentCatalog>,
        log: Arc<dyn CatalogCommandLog>,
    ) -> Result<Self, ExecutableAgentRegistrationError> {
        let (high_water, commands) = load_full(log.as_ref()).await.map_err(projection_load)?;
        let local = LocalExecutableAgentRegistrar::new(catalog.clone());
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
        registration: ExecutableAgentRegistration,
    ) -> Result<ExecutableAgentRegistrationOutcome, ExecutableAgentRegistrationError> {
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

    async fn refresh_projection(&self) -> Result<(), ExecutableAgentRegistrationError> {
        let _guard = self.mutation.lock().await;
        match load_refresh(&self.cursor, self.log.as_ref())
            .await
            .map_err(projection_load)?
        {
            ProjectionBatch::Current => Ok(()),
            ProjectionBatch::Incremental { through, commands } => {
                let refreshed = clone_catalog(&self.catalog)?;
                replay_commands(
                    &LocalExecutableAgentRegistrar::new(refreshed.clone()),
                    commands,
                )
                .await?;
                install_catalog(&self.catalog, &refreshed)?;
                self.cursor.advance(through);
                Ok(())
            }
            ProjectionBatch::FullReplay { through, commands } => {
                let refreshed = Arc::new(ExecutableAgentCatalog::new());
                replay_commands(
                    &LocalExecutableAgentRegistrar::new(refreshed.clone()),
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
        withdrawal: ExecutableAgentWithdrawal,
    ) -> Result<ExecutableAgentWithdrawalOutcome, ExecutableAgentRegistrationError> {
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
    local: &LocalExecutableAgentRegistrar,
    commands: Vec<Sequenced<CatalogCommand>>,
) -> Result<(), ExecutableAgentRegistrationError> {
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
    error: ProjectionLoadError<ExecutableAgentRegistrationError>,
) -> ExecutableAgentRegistrationError {
    match error {
        ProjectionLoadError::Source(error) => error,
        ProjectionLoadError::Incomplete { through } => storage(format!(
            "executable Agent command replay did not reach durable high-water {through}"
        )),
    }
}

fn clone_catalog(
    source: &Arc<ExecutableAgentCatalog>,
) -> Result<Arc<ExecutableAgentCatalog>, ExecutableAgentRegistrationError> {
    let cloned = Arc::new(ExecutableAgentCatalog::new());
    *cloned
        .state
        .write()
        .map_err(|_| storage("cloned executable Agent catalog lock poisoned"))? = source
        .state
        .read()
        .map_err(|_| storage("executable Agent catalog lock poisoned"))?
        .clone();
    Ok(cloned)
}

fn install_catalog(
    target: &Arc<ExecutableAgentCatalog>,
    refreshed: &Arc<ExecutableAgentCatalog>,
) -> Result<(), ExecutableAgentRegistrationError> {
    let state = refreshed
        .state
        .read()
        .map_err(|_| storage("refreshed executable Agent catalog lock poisoned"))?
        .clone();
    *target
        .state
        .write()
        .map_err(|_| storage("executable Agent catalog lock poisoned"))? = state;
    Ok(())
}

/// Coordinator PostgreSQL adapter. It acknowledges a command only after the
/// exact command is durable, then applies it through the canonical catalog state
/// machine. A fresh catalog is rehydrated before this constructor returns.
#[derive(Clone)]
pub struct PostgresExecutableAgentRegistrar {
    inner: Arc<DurableRegistrar>,
}

impl PostgresExecutableAgentRegistrar {
    /// Advance this replica from the durable high-water mark, falling back to a
    /// complete replay if the cursor or incremental tail is inconsistent.
    /// Registration transition rules remain solely in `ExecutableAgentCatalog`.
    pub async fn refresh_projection(&self) -> Result<(), ExecutableAgentRegistrationError> {
        self.inner.refresh_projection().await
    }

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
        commands: StdMutex<Vec<Sequenced<CatalogCommand>>>,
        fail_next_append: AtomicBool,
        drop_incremental_tail_once: AtomicBool,
        load_after_calls: StdMutex<Vec<u64>>,
    }

    #[async_trait]
    impl CatalogCommandLog for MemoryCommandLog {
        async fn append(
            &self,
            command: &CatalogCommand,
        ) -> Result<u64, ExecutableAgentRegistrationError> {
            if self.fail_next_append.swap(false, Ordering::SeqCst) {
                return Err(storage("injected append failure"));
            }
            let mut commands = self.commands.lock().unwrap();
            if let Some(existing) = commands
                .iter()
                .find(|existing| existing.command.identity() == command.identity())
            {
                return if existing.command == *command {
                    Ok(existing.sequence)
                } else {
                    Err(ExecutableAgentRegistrationError::Conflict(
                        "incompatible persisted command".into(),
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
        type Error = ExecutableAgentRegistrationError;

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
        ) -> Result<Vec<Sequenced<CatalogCommand>>, ExecutableAgentRegistrationError> {
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
        conflict.session_profile.system = Some("conflicting projection".into());
        assert!(
            matches!(
                registrar.register(conflict).await,
                Err(ExecutableAgentRegistrationError::Conflict(_))
            ),
            "P4"
        );
        assert_eq!(log.commands.lock().unwrap().len(), 1, "P4");
        assert_eq!(
            catalog.current("workspace-a", "agent-a").unwrap(),
            registration(),
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
    async fn active_active_peer_refreshes_the_durable_command_projection() {
        // Cause/effect decision table: A1 peers start empty; A2 authority advances
        // -> stale peer incrementally loads only commands above its durable cursor;
        // A3 unchanged high-water -> no command read; A4 an incomplete incremental
        // batch (the same effect as a missed/invalid change signal) -> full replay
        // from durable truth, never partial admission. The memory sequence uses
        // deliberate gaps to prove identity values need not be contiguous.
        let log = Arc::new(MemoryCommandLog::default());
        let left_catalog = Arc::new(ExecutableAgentCatalog::new());
        let right_catalog = Arc::new(ExecutableAgentCatalog::new());
        let left = registrar(log.clone(), left_catalog.clone()).await;
        let right = registrar(log.clone(), right_catalog.clone()).await;
        assert!(
            left_catalog.current("workspace-a", "agent-a").is_none(),
            "A1"
        );
        assert!(
            right_catalog.current("workspace-a", "agent-a").is_none(),
            "A1"
        );

        left.register(registration()).await.unwrap();
        assert!(
            left_catalog.current("workspace-a", "agent-a").is_some(),
            "A2"
        );
        assert!(
            right_catalog.current("workspace-a", "agent-a").is_none(),
            "A2"
        );

        right.refresh_projection().await.unwrap();
        assert_eq!(
            right_catalog
                .current("workspace-a", "agent-a")
                .unwrap()
                .snapshot
                .fingerprint
                .0,
            "fp-a",
            "A3"
        );
        let reads_after_first_refresh = log.load_after_calls.lock().unwrap().len();
        right.refresh_projection().await.unwrap();
        assert_eq!(
            log.load_after_calls.lock().unwrap().len(),
            reads_after_first_refresh,
            "A3"
        );

        left.register(test_registration(8, "fp-b")).await.unwrap();
        right.refresh_projection().await.unwrap();
        assert_eq!(
            *log.load_after_calls.lock().unwrap().last().unwrap(),
            2,
            "A2"
        );
        assert_eq!(
            right_catalog
                .current("workspace-a", "agent-a")
                .unwrap()
                .snapshot
                .fingerprint
                .0,
            "fp-b",
            "A2"
        );

        left.register(test_registration(9, "fp-c")).await.unwrap();
        log.drop_incremental_tail_once.store(true, Ordering::SeqCst);
        right.refresh_projection().await.unwrap();
        let calls = log.load_after_calls.lock().unwrap();
        assert_eq!(&calls[calls.len() - 2..], &[4, 0], "A4");
        assert_eq!(
            right_catalog
                .current("workspace-a", "agent-a")
                .unwrap()
                .snapshot
                .fingerprint
                .0,
            "fp-c",
            "A4"
        );
        assert_eq!(log.commands.lock().unwrap().len(), 3, "A4");
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
