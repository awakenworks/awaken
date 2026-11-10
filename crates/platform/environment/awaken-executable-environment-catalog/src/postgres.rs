// @formal entrypoint executable_projection.concurrent_refresh_is_serialized_per_domain
// @formal entrypoint executable_projection.cursor_never_leads_installed_projection
// @formal entrypoint executable_projection.installed_projection_never_leads_authority
//! PostgreSQL durability adapter for the executable Environment projection.
//!
//! Only boundary commands are persisted. Startup and every mutation replay the
//! canonical in-memory catalog state machine, so SQL never becomes a parallel
//! lifecycle implementation.
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation
    )
)]

use std::sync::Arc;

use async_trait::async_trait;
use awaken_durable_projection::{
    ProjectionBatch, ProjectionLoadError, ProjectionLog, ProjectionRefreshDomain, Sequenced,
    load_full, load_refresh,
};
use awaken_executable_environment_contract::{
    ExecutableEnvironmentRegistrar, ExecutableEnvironmentRegistration,
    ExecutableEnvironmentRegistrationError, ExecutableEnvironmentRegistrationOutcome,
    ExecutableEnvironmentWithdrawal, ExecutableEnvironmentWithdrawalOutcome,
};
use sqlx::postgres::PgPool;

use crate::schema::{NS, executable_environment_catalog_bundle};
use crate::{ExecutableEnvironmentCatalog, LocalExecutableEnvironmentRegistrar};

const REGISTRATION_KIND: &str = "registration";
const HISTORICAL_REGISTRATION_KIND: &str = "history_registration";
const WITHDRAWAL_KIND: &str = "withdrawal";

#[derive(Clone, Debug, PartialEq)]
enum CatalogCommand {
    Registration(Box<ExecutableEnvironmentRegistration>),
    HistoricalRegistration(Box<ExecutableEnvironmentRegistration>),
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
            Self::HistoricalRegistration(command) => (
                &command.definition.id,
                command.definition.revision.0,
                HISTORICAL_REGISTRATION_KIND,
            ),
            Self::Withdrawal(command) => (
                &command.environment_id,
                command.lifecycle_revision.0,
                WITHDRAWAL_KIND,
            ),
        }
    }

    fn registration_facts(&self) -> Option<&ExecutableEnvironmentRegistration> {
        match self {
            Self::Registration(command) | Self::HistoricalRegistration(command) => Some(command),
            Self::Withdrawal(_) => None,
        }
    }

    fn encode(&self) -> Result<String, ExecutableEnvironmentRegistrationError> {
        match self {
            Self::Registration(command) | Self::HistoricalRegistration(command) => {
                serde_json::to_string(command)
            }
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
            HISTORICAL_REGISTRATION_KIND => Self::HistoricalRegistration(Box::new(
                serde_json::from_str(encoded).map_err(storage)?,
            )),
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
        if command.registration_facts().is_some() {
            // Both command modes retain one immutable (id, revision) fact.
            // A separate kind must not bypass same-revision conflict checking.
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
                .bind(format!(
                    "environment-registration:{environment_id}:{revision}"
                ))
                .execute(&mut *transaction)
                .await
                .map_err(storage)?;
            let siblings: Vec<String> = sqlx::query_scalar(
                "SELECT command_json FROM executable_environment_command WHERE environment_id=$1 AND lifecycle_revision=$2 AND command_kind IN ('registration','history_registration')"
            ).bind(environment_id).bind(revision).fetch_all(&mut *transaction).await.map_err(storage)?;
            if siblings.iter().any(|existing| existing != &encoded) {
                return Err(ExecutableEnvironmentRegistrationError::Conflict(
                    "Environment revision already has different immutable registration facts"
                        .into(),
                ));
            }
        }
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
    refresh: ProjectionRefreshDomain,
}

impl DurableRegistrar {
    async fn hydrate(
        catalog: Arc<ExecutableEnvironmentCatalog>,
        log: Arc<dyn CatalogCommandLog>,
    ) -> Result<Self, ExecutableEnvironmentRegistrationError> {
        let (target, commands) = load_full(log.as_ref()).await.map_err(projection_load)?;
        let local = LocalExecutableEnvironmentRegistrar::new(catalog.clone());
        replay_commands(&local, commands).await?;
        Ok(Self {
            catalog,
            local,
            log,
            refresh: ProjectionRefreshDomain::at(target.through()),
        })
    }

    async fn restore_historical_registration(
        &self,
        registration: ExecutableEnvironmentRegistration,
    ) -> Result<(), ExecutableEnvironmentRegistrationError> {
        let _guard = self.refresh.lock().await;
        self.catalog
            .preview_historical_registration(&registration)?;
        self.log
            .append(&CatalogCommand::HistoricalRegistration(Box::new(
                registration.clone(),
            )))
            .await?;
        self.local
            .restore_historical_registration(registration)
            .await
    }

    async fn register(
        &self,
        registration: ExecutableEnvironmentRegistration,
    ) -> Result<ExecutableEnvironmentRegistrationOutcome, ExecutableEnvironmentRegistrationError>
    {
        let _guard = self.refresh.lock().await;
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
        let guard = self.refresh.lock().await;
        match load_refresh(guard.cursor(), self.log.as_ref())
            .await
            .map_err(projection_load)?
        {
            ProjectionBatch::Current => Ok(()),
            ProjectionBatch::Incremental { target, commands } => {
                let refreshed = clone_catalog(&self.catalog)?;
                replay_commands(
                    &LocalExecutableEnvironmentRegistrar::new(refreshed.clone()),
                    commands,
                )
                .await?;
                guard
                    .cursor()
                    .install_then_reconcile(target, || install_catalog(&self.catalog, &refreshed))
            }
            ProjectionBatch::FullReplay { target, commands } => {
                let refreshed = Arc::new(ExecutableEnvironmentCatalog::new());
                replay_commands(
                    &LocalExecutableEnvironmentRegistrar::new(refreshed.clone()),
                    commands,
                )
                .await?;
                guard
                    .cursor()
                    .install_then_reconcile(target, || install_catalog(&self.catalog, &refreshed))
            }
        }
    }

    async fn withdraw(
        &self,
        withdrawal: ExecutableEnvironmentWithdrawal,
    ) -> Result<ExecutableEnvironmentWithdrawalOutcome, ExecutableEnvironmentRegistrationError>
    {
        let _guard = self.refresh.lock().await;
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
            CatalogCommand::HistoricalRegistration(command) => {
                local
                    .restore_historical_registration(*command)
                    .await
                    .map_err(|error| {
                        storage(format!("replay persisted historical registration: {error}"))
                    })?;
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
        let pool = awaken_sql_storage::postgres_pool_options()
            .connect(url)
            .await
            .map_err(storage)?;
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
        let pool = awaken_sql_storage::postgres_pool_options()
            .connect(url)
            .await
            .map_err(storage)?;
        Self::with_existing_pool(pool, catalog).await
    }

    /// Verify the existing scoped ledger over the composition-owned pool.
    pub async fn with_existing_pool(
        pool: PgPool,
        catalog: Arc<ExecutableEnvironmentCatalog>,
    ) -> Result<Self, ExecutableEnvironmentRegistrationError> {
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
    async fn restore_historical_registration(
        &self,
        registration: ExecutableEnvironmentRegistration,
    ) -> Result<(), ExecutableEnvironmentRegistrationError> {
        self.inner
            .restore_historical_registration(registration)
            .await
    }

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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;

    use awaken_environment_contract::{EnvItem, EnvironmentConfig, EnvironmentRevision};
    use sqlx::Executor;
    use sqlx::postgres::PgPoolOptions;
    use tokio::sync::{Barrier, Notify};

    use super::*;

    #[derive(Default)]
    struct MemoryCommandLog {
        commands: StdMutex<Vec<Sequenced<CatalogCommand>>>,
        drop_incremental_tail_once: AtomicBool,
        load_after_calls: StdMutex<Vec<u64>>,
        commit_after_next_high_water: StdMutex<Option<CatalogCommand>>,
        gate_next_high_water: AtomicBool,
        high_water_calls: AtomicUsize,
        high_water_entered: Notify,
        release_high_water: Notify,
    }

    #[async_trait]
    impl CatalogCommandLog for MemoryCommandLog {
        async fn append(
            &self,
            command: &CatalogCommand,
        ) -> Result<u64, ExecutableEnvironmentRegistrationError> {
            let mut commands = self.commands.lock().unwrap();
            if let Some(proposed) = command.registration_facts()
                && commands.iter().any(|existing| {
                    existing.command.registration_facts().is_some_and(|stored| {
                        stored.definition.id == proposed.definition.id
                            && stored.definition.revision == proposed.definition.revision
                            && stored != proposed
                    })
                })
            {
                return Err(ExecutableEnvironmentRegistrationError::Conflict(
                    "different registration facts across command modes".into(),
                ));
            }
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
            self.high_water_calls.fetch_add(1, Ordering::SeqCst);
            if self.gate_next_high_water.swap(false, Ordering::SeqCst) {
                self.high_water_entered.notify_one();
                self.release_high_water.notified().await;
            }
            let mut commands = self.commands.lock().unwrap();
            let through = commands.last().map_or(0, |command| command.sequence);
            if let Some(command) = self.commit_after_next_high_water.lock().unwrap().take() {
                commands.push(Sequenced {
                    sequence: through.saturating_add(2),
                    command,
                });
            }
            Ok(through)
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
                created_at: Some("2026-02-03T04:05:06.007Z".into()),
                updated_at: Some("2026-02-03T04:05:06.007Z".into()),
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

    #[tokio::test]
    async fn install_error_precedes_environment_target_publication() {
        // Causes: C1 the target contains revision 1; C2 the replacement contains
        // revision 2; C3 the replacement read lock is poisoned before install.
        // Effects: R1 install returns Storage; R2 the target remains the exact
        // revision-1 projection. This binds install_then_reconcile's Err-means-
        // no-publication precondition to the real Environment catalog installer.
        let target = Arc::new(ExecutableEnvironmentCatalog::new());
        LocalExecutableEnvironmentRegistrar::new(target.clone())
            .register(registration())
            .await
            .unwrap();
        let refreshed = Arc::new(ExecutableEnvironmentCatalog::new());
        LocalExecutableEnvironmentRegistrar::new(refreshed.clone())
            .register(registration_at(2, "Environment B"))
            .await
            .unwrap();
        let poisoned = refreshed.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoned
                .state
                .write()
                .unwrap_or_else(|poisoned| panic!("{poisoned}"));
            panic!("poison refreshed Environment catalog lock");
        })
        .join();

        assert!(
            matches!(
                install_catalog(&target, &refreshed),
                Err(ExecutableEnvironmentRegistrationError::Storage(_))
            ),
            "R1"
        );
        assert_eq!(
            target.current("env-a").unwrap().definition.name,
            "Environment A",
            "R2"
        );
    }

    async fn registrar(
        log: Arc<MemoryCommandLog>,
        catalog: Arc<ExecutableEnvironmentCatalog>,
    ) -> DurableRegistrar {
        DurableRegistrar::hydrate(catalog, log).await.unwrap()
    }

    #[tokio::test]
    async fn concurrent_environment_refreshes_are_serialized_by_the_domain_registrar() {
        // Causes: C1 one registrar has cursor 0 and authority sequence 2; C2 two
        // refresh tasks have both crossed their start barrier; C3 the first
        // high-water read is held pending. Effects: R1 exactly one source read
        // can enter while C3 holds; R2 release installs sequence 2 and advances
        // the cursor; R3 the waiting refresh then observes Current without a
        // second range replay, leaving the exact canonical Environment projection.
        // Rules: C1+C2+C3 => R1; release+C1 => R2; R2+waiting task => R3.
        let log = Arc::new(MemoryCommandLog::default());
        let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let registrar = Arc::new(registrar(log.clone(), catalog.clone()).await);
        log.append(&CatalogCommand::Registration(Box::new(registration())))
            .await
            .unwrap();
        log.high_water_calls.store(0, Ordering::SeqCst);
        log.load_after_calls.lock().unwrap().clear();
        log.gate_next_high_water.store(true, Ordering::SeqCst);

        let start = Arc::new(Barrier::new(3));
        let attempts = Arc::new(AtomicUsize::new(0));
        let mut refreshes = Vec::new();
        for _ in 0..2 {
            let registrar = registrar.clone();
            let start = start.clone();
            let attempts = attempts.clone();
            refreshes.push(tokio::spawn(async move {
                start.wait().await;
                attempts.fetch_add(1, Ordering::SeqCst);
                registrar.refresh_projection().await
            }));
        }
        start.wait().await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while attempts.load(Ordering::SeqCst) != 2 {
                tokio::task::yield_now().await;
            }
            log.high_water_entered.notified().await;
        })
        .await
        .expect("both refresh tasks attempt and one reaches the source");
        tokio::task::yield_now().await;
        assert_eq!(log.high_water_calls.load(Ordering::SeqCst), 1, "R1");
        assert!(catalog.current("env-a").is_none(), "R1");

        log.release_high_water.notify_one();
        for refresh in refreshes {
            refresh.await.unwrap().unwrap();
        }
        assert_eq!(log.high_water_calls.load(Ordering::SeqCst), 2, "R2/R3");
        assert_eq!(*log.load_after_calls.lock().unwrap(), vec![0], "R3");
        assert_eq!(
            catalog.current("env-a").unwrap().definition.name,
            "Environment A",
            "R3"
        );
    }

    #[tokio::test]
    async fn environment_command_above_captured_high_water_waits_for_next_refresh() {
        // Causes: C1 the registrar starts at cursor 0 with revision 1 committed
        // at sequence 2; C2 revision 2 commits at sequence 4 after high_water
        // captures 2 but before load_range runs; C3 a second refresh observes 4.
        // Effects: R1 the first bounded load installs only revision 1 and leaves
        // cursor 2 while durable authority is already 4; R2 the second refresh
        // loads after 2 through 4 and installs revision 2 with cursor 4.
        // Rules: C1+C2 => R1; R1+C3 => R2. This binds the shared stable-range
        // contract to the canonical Environment replay/install path.
        let log = Arc::new(MemoryCommandLog::default());
        let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let registrar = registrar(log.clone(), catalog.clone()).await;
        log.load_after_calls.lock().unwrap().clear();
        log.append(&CatalogCommand::Registration(Box::new(registration())))
            .await
            .unwrap();
        *log.commit_after_next_high_water.lock().unwrap() = Some(CatalogCommand::Registration(
            Box::new(registration_at(2, "Environment B")),
        ));

        registrar.refresh_projection().await.unwrap();
        assert_eq!(registrar.refresh.applied(), 2, "R1");
        assert_eq!(
            log.commands.lock().unwrap().last().unwrap().sequence,
            4,
            "R1"
        );
        assert_eq!(*log.load_after_calls.lock().unwrap(), vec![0], "R1");
        assert_eq!(
            catalog.current("env-a").unwrap().definition.name,
            "Environment A",
            "R1"
        );

        registrar.refresh_projection().await.unwrap();
        assert_eq!(registrar.refresh.applied(), 4, "R2");
        assert_eq!(*log.load_after_calls.lock().unwrap(), vec![0, 2], "R2");
        assert_eq!(
            catalog.current("env-a").unwrap().definition.name,
            "Environment B",
            "R2"
        );
    }

    #[tokio::test]
    async fn active_active_peer_refreshes_the_durable_environment_projection() {
        // Cause/effect decision table: R1 peers start empty; R2 authority ahead
        // -> stale peer loads only commands above its durable cursor; R3 unchanged
        // high-water -> no command read; R4 incomplete incremental tail -> full
        // replay from durable truth; R5 authority restored behind the cursor ->
        // replace from complete truth and lower the cursor to that exact
        // high-water. Deliberate sequence gaps prove no contiguous identity
        // assumption leaks into the projection.
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
        {
            let calls = log.load_after_calls.lock().unwrap();
            assert_eq!(&calls[calls.len() - 2..], &[4, 0], "R4");
        }
        assert_eq!(
            right_catalog.current("env-a").unwrap().definition.name,
            "Environment C",
            "R4"
        );
        assert_eq!(log.commands.lock().unwrap().len(), 3, "R4");
        log.commands.lock().unwrap().truncate(1);
        right.refresh_projection().await.unwrap();
        assert_eq!(right.refresh.applied(), 2, "R5");
        assert_eq!(
            right_catalog.current("env-a").unwrap().definition.name,
            "Environment A",
            "R5"
        );
    }

    #[tokio::test]
    async fn postgres_migration_persists_high_water_and_rehydrates_environment() {
        // Cause/effect decision table: R1 an optional fixture is absent -> the
        // integration case skips; configured failures reject. R2 a fresh authority applies
        // deterministic V1/V2 and accepts revision 1 -> the command has a
        // positive durable sequence; R3 connect_existing runs no DDL and a new
        // replica full-replays the same Environment definition.
        const SCHEMA: &str = "t_executable_environment_catalog";
        let Some(database_url) = awaken_reliability_testkit::postgres_fixture_url() else {
            return;
        };
        let admin = PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_secs(1))
            .connect(&database_url)
            .await
            .expect("configured executable Environment PostgreSQL fixture must connect");
        admin
            .execute("DROP SCHEMA IF EXISTS t_executable_environment_catalog CASCADE")
            .await
            .unwrap();
        admin
            .execute("CREATE SCHEMA t_executable_environment_catalog")
            .await
            .unwrap();
        let pool = PgPoolOptions::new()
            .after_connect(|connection, _| {
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path', $1, false)")
                        .bind(SCHEMA)
                        .execute(connection)
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
            .execute("DROP SCHEMA IF EXISTS t_executable_environment_catalog CASCADE")
            .await
            .unwrap();
        admin.close().await;
    }

    #[tokio::test]
    async fn durable_history_replay_is_distinct_from_admission_and_allows_later_promotion() {
        // D1 history-only append survives full hydration and incremental peer
        // refresh without admission. D2 explicit register promotes the same fact.
        // D3 conflicts across command modes fail before log append. D4 replay
        // after a terminal withdrawal retains exact histories without admission.
        let log = Arc::new(MemoryCommandLog::default());
        let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let writer = registrar(log.clone(), catalog.clone()).await;
        let peer_catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let peer = registrar(log.clone(), peer_catalog.clone()).await;
        let exact = registration();
        writer
            .restore_historical_registration(exact.clone())
            .await
            .unwrap();
        peer.refresh_projection().await.unwrap();
        assert!(peer_catalog.current("env-a").is_none(), "D1 incremental");
        assert_eq!(
            peer_catalog.at_revision("env-a", EnvironmentRevision(1)),
            Some(exact.clone()),
            "D1"
        );
        let reopened_catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let reopened = registrar(log.clone(), reopened_catalog.clone()).await;
        assert!(reopened_catalog.current("env-a").is_none(), "D1 full");
        assert_eq!(
            reopened.register(exact.clone()).await.unwrap(),
            ExecutableEnvironmentRegistrationOutcome::RegisteredCurrent,
            "D2"
        );
        peer.refresh_projection().await.unwrap();
        assert_eq!(peer_catalog.current("env-a"), Some(exact.clone()), "D2");
        let before = log.commands.lock().unwrap().len();
        assert!(
            matches!(
                writer
                    .restore_historical_registration(registration_at(1, "changed"))
                    .await,
                Err(ExecutableEnvironmentRegistrationError::Conflict(_))
            ),
            "D3 history after normal"
        );
        assert!(
            matches!(
                log.append(&CatalogCommand::Registration(Box::new(registration_at(
                    1, "changed"
                ))))
                .await,
                Err(ExecutableEnvironmentRegistrationError::Conflict(_))
            ),
            "D3 cross-mode log fence"
        );
        assert_eq!(log.commands.lock().unwrap().len(), before, "D3 no append");
        reopened
            .withdraw(ExecutableEnvironmentWithdrawal {
                environment_id: "env-a".into(),
                lifecycle_revision: EnvironmentRevision(3),
            })
            .await
            .unwrap();
        reopened
            .restore_historical_registration(registration_at(2, "old-frozen"))
            .await
            .unwrap();
        let final_catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let _final = registrar(log, final_catalog.clone()).await;
        assert!(final_catalog.current("env-a").is_none(), "D4");
        assert_eq!(
            final_catalog.at_revision("env-a", EnvironmentRevision(1)),
            Some(exact),
            "D4"
        );
        assert!(
            final_catalog
                .at_revision("env-a", EnvironmentRevision(2))
                .is_some(),
            "D4"
        );
    }

    #[tokio::test]
    async fn postgres_history_command_reopens_without_admission_and_fences_cross_mode_conflicts() {
        // P1 exact history in the real SQL log reopens without admission.
        // P2 separate replicas race conflicting modes at one identity: exactly
        // one immutable fact can commit. P3 normal exact promotion survives reopen.
        let Some(database_url) = awaken_reliability_testkit::postgres_fixture_url() else {
            return;
        };
        let admin = PgPoolOptions::new()
            .acquire_timeout(Duration::from_secs(3))
            .connect(&database_url)
            .await
            .expect("explicit PostgreSQL fixture must be available");
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let schema = format!("t_env_history_{}_{}", std::process::id(), unique);
        admin
            .execute(
                // The fixture schema is either a literal or a fixed prefix plus integer process/time coordinates.
                sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")),
            )
            .await
            .unwrap();
        let search_schema = schema.clone();
        let pool = PgPoolOptions::new()
            .max_connections(3)
            .after_connect(move |connection, _| {
                let schema = search_schema.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path', $1, false)")
                        .bind(schema)
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(&database_url)
            .await
            .unwrap();
        let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let writer =
            PostgresExecutableEnvironmentRegistrar::with_pool(pool.clone(), catalog.clone())
                .await
                .unwrap();
        let exact = registration();
        writer
            .restore_historical_registration(exact.clone())
            .await
            .unwrap();
        let reopened_catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let reopened = PostgresExecutableEnvironmentRegistrar::with_pool(
            pool.clone(),
            reopened_catalog.clone(),
        )
        .await
        .unwrap();
        assert!(reopened_catalog.current("env-a").is_none(), "P1");
        assert_eq!(
            reopened_catalog.at_revision("env-a", EnvironmentRevision(1)),
            Some(exact.clone()),
            "P1"
        );
        let history = registration_at(2, "history-fact");
        let normal = registration_at(2, "normal-conflicting-fact");
        let (left, right) = tokio::join!(
            writer.restore_historical_registration(history),
            reopened.register(normal)
        );
        assert_ne!(left.is_ok(), right.is_ok(), "P2 exactly one fact wins");
        assert!(
            matches!(
                left.as_ref().err().or(right.as_ref().err()),
                Some(ExecutableEnvironmentRegistrationError::Conflict(_))
            ),
            "P2 explicit conflict"
        );
        assert_eq!(
            writer.register(exact.clone()).await.unwrap(),
            ExecutableEnvironmentRegistrationOutcome::RegisteredCurrent,
            "P3 local history promotion"
        );
        let final_catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let final_registrar =
            PostgresExecutableEnvironmentRegistrar::with_pool(pool.clone(), final_catalog.clone())
                .await
                .unwrap();
        // A winning normal revision 2 may legitimately supersede revision 1.
        assert!(
            final_catalog.current("env-a").is_some(),
            "P3 promotion was persisted"
        );
        assert_eq!(
            final_catalog.at_revision("env-a", EnvironmentRevision(1)),
            Some(exact),
            "P3 exact history"
        );
        drop((writer, reopened, final_registrar));
        pool.close().await;
        admin
            .execute(
                // The fixture schema is either a literal or a fixed prefix plus integer process/time coordinates.
                sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")),
            )
            .await
            .unwrap();
        admin.close().await;
    }
    #[test]
    fn postgres_history_command_reopens_without_admission_and_fences_cross_mode_conflicts_fixture_admission()
     {
        awaken_reliability_testkit::assert_postgres_fixture_child_contract(
            "postgres::tests::postgres_history_command_reopens_without_admission_and_fences_cross_mode_conflicts",
        );
    }
}
