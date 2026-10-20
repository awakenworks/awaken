// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: This source implements the repository-controlled store product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Coordinator-owned PostgreSQL persistence selection.
//!
//! The migration command and both standalone/AllInOne startup reuse this one
//! component boundary. Operational migration applies bundles without publishing
//! process globals; Local startup may migrate and connect; Server startup only
//! verifies ledgers before connecting.

use super::runtime_authority::SchemaAccess;
use super::worker_registry::WorkerDirectoryHandle;
use awaken_session_executor::{DeploymentConfig, DispatchBackend, StoreKind};

#[derive(Clone)]
pub struct CoordinatorPersistence {
    pub worker_directory: WorkerDirectoryHandle,
    pub runtime_authority: std::sync::Arc<dyn awaken_dispatch_erasure::RuntimeAuthority>,
    /// Process-owned PostgreSQL pool shared by every hosted adapter that reads
    /// the Coordinator database. `None` is the explicit non-PostgreSQL case.
    pub postgres_pool: Option<sqlx::PgPool>,
    /// Authoritative committed-run recovery view. Hosted compositions reuse
    /// this handle instead of opening a second commit projection.
    pub run_recovery: Option<
        std::sync::Arc<dyn awaken_agent_contract::thread::read::recovery::RunRecoverySource>,
    >,
    /// Authoritative committed-run lifecycle view over the same projection.
    pub run_lifecycle: Option<
        std::sync::Arc<dyn awaken_agent_contract::thread::read::lifecycle::RunLifecycleFeed>,
    >,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PostgresComponents {
    dispatch: bool,
    commit: bool,
}

impl PostgresComponents {
    fn requires_process_pool(self) -> bool {
        self.dispatch || self.commit
    }

    fn exposes_committed_run_feeds(self) -> bool {
        self.commit
    }
}

fn postgres_components(deployment: &DeploymentConfig) -> PostgresComponents {
    PostgresComponents {
        dispatch: deployment.dispatch_backend == DispatchBackend::Postgres,
        commit: deployment.store == StoreKind::Postgres,
    }
}

fn database_url(deployment: &DeploymentConfig) -> Result<Option<&str>, String> {
    let components = postgres_components(deployment);
    if !components.dispatch && !components.commit {
        return Ok(None);
    }
    deployment
        .database_url
        .as_deref()
        .map(Some)
        .ok_or_else(|| "a Postgres Coordinator store requires runtime.database_url".to_string())
}

/// Apply every configured Coordinator-owned Postgres bundle without starting
/// runtime services or connecting a wake transport.
pub async fn migrate_postgres_schema(deployment: &DeploymentConfig) -> Result<(), String> {
    let Some(url) = database_url(deployment)? else {
        return Ok(());
    };
    let pool = awaken_sql_storage::postgres_pool_options()
        .max_connections(deployment.postgres_max_connections.get())
        .connect(url)
        .await
        .map_err(|error| error.to_string())?;
    migrate_postgres_schema_with_pool(deployment, pool).await
}

/// Operational migration reuses the composition's one bounded pool. Scope SQL
/// stays in the existing adapters and no wake/runtime global is initialized.
pub async fn migrate_postgres_schema_with_pool(
    deployment: &DeploymentConfig,
    pool: sqlx::PgPool,
) -> Result<(), String> {
    if database_url(deployment)?.is_none() {
        return Ok(());
    }
    let components = postgres_components(deployment);
    if components.dispatch {
        super::worker_registry::open_postgres_pool(pool.clone(), false).await?;
    }
    super::runtime_authority::migrate_with_pool(deployment, pool).await
}

/// Initialize only the runtime backends selected by a scenario deployment.
///
/// Test-support scenario routers inject one explicit reference runtime authority
/// and their own in-memory `WorkerDirectory`; opening and then discarding a
/// production directory here would create a second authority and would
/// incorrectly require SQLite storage for an intentionally ephemeral fixture.
/// Production startup uses [`open`] or [`open_existing`] and therefore
/// still receive the durable authorities returned by this module.
#[cfg(any(test, feature = "test-support"))]
pub async fn init_scenario_runtime(
    deployment: &DeploymentConfig,
    clock: std::sync::Arc<dyn awaken_time::WallClock>,
) -> Result<std::sync::Arc<dyn awaken_dispatch_erasure::RuntimeAuthority>, String> {
    if deployment.storage_dir.is_none()
        && deployment.database_url.is_none()
        && deployment.store == awaken_session_executor::StoreKind::Sqlite
        && deployment.dispatch_backend == awaken_session_executor::DispatchBackend::Sqlite
    {
        let dispatch = awaken_dispatch::AnyCoordinatorDispatch::from_durable_store(
            awaken_dispatch::MemoryDispatchStore::new(clock.clone()),
            None,
        );
        return Ok(std::sync::Arc::new(
            awaken_session_executor::EphemeralRuntimeAuthority::with_dispatch(
                clock,
                std::sync::Arc::new(dispatch),
            ),
        ));
    }
    super::runtime_authority::DurableRuntimeAuthority::open(
        deployment,
        super::runtime_authority::SchemaAccess::Migrate,
        clock,
    )
    .await
    .map(|authority| authority as std::sync::Arc<dyn awaken_dispatch_erasure::RuntimeAuthority>)
}

/// Open every Coordinator-owned runtime authority, applying schemas first.
/// This Local-mode path returns the one WorkerDirectory instance that must be
/// injected into every Worker-facing and observation-facing consumer.
pub async fn open(
    deployment: &DeploymentConfig,
    clock: std::sync::Arc<dyn awaken_time::WallClock>,
) -> Result<CoordinatorPersistence, String> {
    open_with(deployment, SchemaAccess::Migrate, clock).await
}

/// Open Coordinator-owned authorities after verifying externally-applied
/// PostgreSQL ledgers. SQLite remains an explicitly durable single-node store.
pub async fn open_existing(
    deployment: &DeploymentConfig,
    clock: std::sync::Arc<dyn awaken_time::WallClock>,
) -> Result<CoordinatorPersistence, String> {
    open_with(deployment, SchemaAccess::Verify, clock).await
}

async fn open_with(
    deployment: &DeploymentConfig,
    schema: SchemaAccess,
    clock: std::sync::Arc<dyn awaken_time::WallClock>,
) -> Result<CoordinatorPersistence, String> {
    let components = postgres_components(deployment);
    let database_url = database_url(deployment)?;
    let postgres_pool = if components.requires_process_pool() {
        let url = database_url.expect("Postgres Coordinator components require database URL");
        Some(
            awaken_sql_storage::postgres_pool_options()
                .max_connections(deployment.postgres_max_connections.get())
                .connect(url)
                .await
                .map_err(|error| {
                    format!("connect process-owned Coordinator Postgres pool: {error}")
                })?,
        )
    } else {
        None
    };
    let runtime_authority =
        super::runtime_authority::DurableRuntimeAuthority::open_with_postgres_pool(
            deployment,
            schema,
            postgres_pool.clone(),
            clock,
        )
        .await?;
    let run_recovery = runtime_authority.postgres_commit().map(|authority| {
        authority
            as std::sync::Arc<dyn awaken_agent_contract::thread::read::recovery::RunRecoverySource>
    });
    let run_lifecycle = runtime_authority.postgres_commit().map(|authority| {
        authority
            as std::sync::Arc<dyn awaken_agent_contract::thread::read::lifecycle::RunLifecycleFeed>
    });
    debug_assert_eq!(
        run_recovery.is_some() && run_lifecycle.is_some(),
        components.exposes_committed_run_feeds()
    );
    let worker_directory = if components.dispatch {
        super::worker_registry::open_postgres_pool(
            postgres_pool
                .clone()
                .expect("Postgres dispatch opened the process pool"),
            matches!(schema, SchemaAccess::Verify),
        )
        .await?
    } else {
        let storage_dir = deployment.storage_dir.as_deref().ok_or_else(|| {
            "Coordinator SQLite Worker registry requires runtime.storage_dir; refusing volatile Worker identity and generation state"
                .to_owned()
        })?;
        super::worker_registry::open_sqlite(storage_dir).await?
    };
    Ok(CoordinatorPersistence {
        worker_directory,
        runtime_authority,
        postgres_pool,
        run_recovery,
        run_lifecycle,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_worker_registry::{WorkerManifest, WorkerRegistration};

    #[test]
    fn backend_selection_maps_to_one_coordinator_schema_manifest() {
        // Cause/effect decision table:
        // Cause/effect graph: C1 dispatch uses Postgres; C2 commit uses
        // Postgres. Either cause requires E1 exactly one process pool; neither
        // cause yields E0 no pool. Dispatch, commit, checkpoint/wake and Worker
        // registry clone that handle, so failover reconnect and backpressure do
        // not multiply by adapter count.
        //
        // E2 is the hosted committed-run read pair, which exists exactly when
        // C2 does; hosted compositions receive E1/E2 from PreparedProcess and
        // therefore never reopen dispatch, commit, or Worker-directory stores.
        //
        // | Rule | C1 dispatch | C2 commit | Pool effect | Feed effect |
        // |---|---|---|---|---|
        // | R1 | no | no | E0 none | E0 none |
        // | R2 | yes | no | E1 one | E0 none |
        // | R3 | no | yes | E1 one | E2 pair |
        // | R4 | yes | yes | E1 one | E2 pair |
        let mut deployment = DeploymentConfig::ephemeral();
        assert_eq!(
            postgres_components(&deployment),
            PostgresComponents {
                dispatch: false,
                commit: false,
            },
            "R1"
        );
        assert!(
            !postgres_components(&deployment).requires_process_pool(),
            "R1"
        );
        assert!(
            !postgres_components(&deployment).exposes_committed_run_feeds(),
            "R1"
        );

        deployment.dispatch_backend = DispatchBackend::Postgres;
        assert_eq!(
            postgres_components(&deployment),
            PostgresComponents {
                dispatch: true,
                commit: false,
            },
            "R2"
        );
        assert!(
            postgres_components(&deployment).requires_process_pool(),
            "R2"
        );
        assert!(
            !postgres_components(&deployment).exposes_committed_run_feeds(),
            "R2"
        );

        deployment.dispatch_backend = DispatchBackend::Sqlite;
        deployment.store = StoreKind::Postgres;
        assert_eq!(
            postgres_components(&deployment),
            PostgresComponents {
                dispatch: false,
                commit: true,
            },
            "R3"
        );
        assert!(
            postgres_components(&deployment).requires_process_pool(),
            "R3"
        );
        assert!(
            postgres_components(&deployment).exposes_committed_run_feeds(),
            "R3"
        );

        deployment.dispatch_backend = DispatchBackend::Postgres;
        assert_eq!(
            postgres_components(&deployment),
            PostgresComponents {
                dispatch: true,
                commit: true,
            },
            "R4"
        );
        assert!(
            postgres_components(&deployment).requires_process_pool(),
            "R4"
        );
        assert!(
            postgres_components(&deployment).exposes_committed_run_feeds(),
            "R4"
        );
    }

    #[tokio::test]
    async fn sqlite_worker_authority_is_durable_and_missing_storage_fails_closed() {
        // Cause/effect graph: dispatch backend + storage coordinate + schema
        // mode -> one WorkerDirectory adapter -> persisted incarnation truth.
        // Decision table: R0 scenario runtime init + SQLite + no storage -> one
        // explicitly injected reference authority with its original atomic
        // Session-continuation and operational-feed roles; R1 production
        // open + SQLite + no storage_dir -> startup error; R2 production open +
        // writable storage_dir -> durable registry; R3 reopen same directory ->
        // exact identity/generation survives. Postgres migrate/verify rules are
        // covered by worker-registry conformance and migration-ledger tests.
        let missing = DeploymentConfig::ephemeral();
        let reference = init_scenario_runtime(&missing, awaken_time::system_clock())
            .await
            .expect("R0 scenario SQLite runtime needs no shared backend");
        fn continuation(_: &dyn awaken_dispatch::SessionContinuationOutbox) {}
        continuation(reference.dispatch_store().as_ref());
        use awaken_dispatch::DispatchOperationalFeed as _;
        assert!(
            reference
                .dispatch_store()
                .events_after(awaken_dispatch::DispatchCursor(0), 1, None)
                .await
                .is_ok(),
            "R0 exact operational feed survives erasure"
        );
        let missing_error = match open(&missing, awaken_time::system_clock()).await {
            Ok(_) => panic!("R1 must reject volatile authority"),
            Err(error) => error,
        };
        assert!(missing_error.contains("runtime.storage_dir"), "R1");

        let root = tempfile::tempdir().unwrap();
        let mut durable = DeploymentConfig::ephemeral();
        durable.storage_dir = Some(root.path().to_path_buf());
        let directory = open(&durable, awaken_time::system_clock())
            .await
            .expect("R2 durable registry");
        let registered = directory
            .worker_directory
            .register(
                WorkerRegistration {
                    worker_id: "worker-a".into(),
                    incarnation_id: "boot-a".into(),
                    manifest: WorkerManifest::default(),
                    create_ledger: None,
                },
                10,
                100,
            )
            .await
            .unwrap();
        drop(directory);

        let reopened = open_existing(&durable, awaken_time::system_clock())
            .await
            .expect("R3 reopen registry");
        assert_eq!(
            reopened.worker_directory.current("worker-a").await.unwrap(),
            Some(registered),
            "R3"
        );
    }
}
