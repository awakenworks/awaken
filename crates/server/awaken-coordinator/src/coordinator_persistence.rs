//! Coordinator-owned Postgres persistence composition.
//!
//! The migration command and both standalone/AllInOne startup reuse this one
//! component boundary. Operational migration applies bundles without publishing
//! process globals; Local startup may migrate and connect; Server startup only
//! verifies ledgers before connecting.

use awaken_runtime_host::{DeploymentConfig, DispatchBackend, StoreKind};

use super::worker_registry::WorkerDirectoryHandle;

#[derive(Clone, Copy)]
enum SchemaAccess {
    Migrate,
    Verify,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PostgresComponents {
    dispatch: bool,
    commit: bool,
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
    let components = postgres_components(deployment);
    let Some(url) = database_url(deployment)? else {
        return Ok(());
    };
    if components.dispatch {
        awaken_runtime_host::migrate_postgres_dispatch_schema(
            url,
            deployment.postgres_max_connections.get(),
        )
        .await?;
        super::worker_registry::migrate_postgres(url, deployment.postgres_max_connections.get())
            .await?;
    }
    if components.commit {
        awaken_runtime_host::migrate_postgres_commit_schema(
            url,
            deployment.postgres_max_connections.get(),
        )
        .await?;
    }
    Ok(())
}

/// Initialize only the runtime backends selected by a scenario composition.
///
/// Test-support scenario routers inject their own explicit in-memory
/// `WorkerDirectory`; opening and then discarding a production directory here
/// would create a second authority and would incorrectly require SQLite storage
/// for an intentionally ephemeral fixture. Production composition roots use
/// [`open`] or [`open_existing`] and therefore still receive the durable
/// directory returned by this module.
#[cfg(any(test, feature = "test-support"))]
pub async fn init_scenario_runtime(deployment: &DeploymentConfig) -> Result<(), String> {
    init_runtime_backends(deployment, SchemaAccess::Migrate).await
}

/// Open every Coordinator-owned runtime authority, applying schemas first.
/// This Local-mode path returns the one WorkerDirectory instance that must be
/// injected into every Worker-facing and observation-facing consumer.
pub async fn open(deployment: &DeploymentConfig) -> Result<WorkerDirectoryHandle, String> {
    open_with(deployment, SchemaAccess::Migrate).await
}

/// Open Coordinator-owned authorities after verifying externally-applied
/// PostgreSQL ledgers. SQLite remains an explicitly durable single-node store.
pub async fn open_existing(deployment: &DeploymentConfig) -> Result<WorkerDirectoryHandle, String> {
    open_with(deployment, SchemaAccess::Verify).await
}

async fn open_with(
    deployment: &DeploymentConfig,
    schema: SchemaAccess,
) -> Result<WorkerDirectoryHandle, String> {
    let components = postgres_components(deployment);
    let database_url = database_url(deployment)?;
    init_runtime_backends_with(deployment, schema, components, database_url).await?;
    let worker_directory = if components.dispatch {
        let url = database_url.expect("Postgres dispatch requires database URL");
        match schema {
            SchemaAccess::Migrate => {
                super::worker_registry::open_postgres(
                    url,
                    deployment.postgres_max_connections.get(),
                )
                .await?
            }
            SchemaAccess::Verify => {
                super::worker_registry::open_existing_postgres(
                    url,
                    deployment.postgres_max_connections.get(),
                )
                .await?
            }
        }
    } else {
        let storage_dir = deployment.storage_dir.as_deref().ok_or_else(|| {
            "Coordinator SQLite Worker registry requires runtime.storage_dir; refusing volatile Worker identity and generation state"
                .to_owned()
        })?;
        super::worker_registry::open_sqlite(storage_dir)?
    };
    Ok(worker_directory)
}

#[cfg(any(test, feature = "test-support"))]
async fn init_runtime_backends(
    deployment: &DeploymentConfig,
    schema: SchemaAccess,
) -> Result<(), String> {
    let components = postgres_components(deployment);
    let database_url = database_url(deployment)?;
    init_runtime_backends_with(deployment, schema, components, database_url).await
}

async fn init_runtime_backends_with(
    deployment: &DeploymentConfig,
    schema: SchemaAccess,
    components: PostgresComponents,
    database_url: Option<&str>,
) -> Result<(), String> {
    if components.dispatch {
        let url = database_url.expect("Postgres dispatch requires database URL");
        match schema {
            SchemaAccess::Migrate => {
                awaken_runtime_host::init_shared_postgres_dispatch_with_config(url, deployment)
                    .await?
            }
            SchemaAccess::Verify => {
                awaken_runtime_host::init_shared_postgres_dispatch_existing_with_config(
                    url, deployment,
                )
                .await?
            }
        }
    }
    if components.commit {
        let url = database_url.expect("Postgres commit requires database URL");
        match schema {
            SchemaAccess::Migrate => {
                awaken_runtime_host::init_shared_postgres_commit(
                    url,
                    deployment.postgres_max_connections.get(),
                )
                .await?
            }
            SchemaAccess::Verify => {
                awaken_runtime_host::init_shared_postgres_commit_existing(
                    url,
                    deployment.postgres_max_connections.get(),
                )
                .await?
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_worker_registry::{WorkerManifest, WorkerRegistration};

    #[test]
    fn backend_selection_maps_to_one_coordinator_schema_manifest() {
        // Cause/effect decision table:
        // R1 SQLite dispatch + memory commit -> no Postgres bundle or URL.
        // R2 Postgres dispatch -> dispatch and worker-registry bundles.
        // R3 Postgres commit -> portable commit and PG-sequence bundles.
        // R4 both Postgres -> the union of R2/R3, sharing one Coordinator URL.
        let mut deployment = DeploymentConfig::ephemeral();
        assert_eq!(
            postgres_components(&deployment),
            PostgresComponents {
                dispatch: false,
                commit: false,
            },
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

        deployment.dispatch_backend = DispatchBackend::Postgres;
        assert_eq!(
            postgres_components(&deployment),
            PostgresComponents {
                dispatch: true,
                commit: true,
            },
            "R4"
        );
    }

    #[tokio::test]
    async fn sqlite_worker_authority_is_durable_and_missing_storage_fails_closed() {
        // Cause/effect graph: dispatch backend + storage coordinate + schema
        // mode -> one WorkerDirectory adapter -> persisted incarnation truth.
        // Decision table: R0 scenario runtime init + SQLite + no storage -> no-op
        // because the scenario injects its separate test directory; R1 production
        // open + SQLite + no storage_dir -> startup error; R2 production open +
        // writable storage_dir -> durable registry; R3 reopen same directory ->
        // exact identity/generation survives. Postgres migrate/verify rules are
        // covered by worker-registry conformance and migration-ledger tests.
        let missing = DeploymentConfig::ephemeral();
        init_scenario_runtime(&missing)
            .await
            .expect("R0 scenario SQLite runtime needs no shared backend");
        let missing_error = match open(&missing).await {
            Ok(_) => panic!("R1 must reject volatile authority"),
            Err(error) => error,
        };
        assert!(missing_error.contains("runtime.storage_dir"), "R1");

        let root = tempfile::tempdir().unwrap();
        let mut durable = DeploymentConfig::ephemeral();
        durable.storage_dir = Some(root.path().to_path_buf());
        let directory = open(&durable).await.expect("R2 durable registry");
        let registered = directory
            .register(
                WorkerRegistration {
                    worker_id: "worker-a".into(),
                    incarnation_id: "boot-a".into(),
                    manifest: WorkerManifest::default(),
                },
                10,
                100,
            )
            .await
            .unwrap();
        drop(directory);

        let reopened = open_existing(&durable).await.expect("R3 reopen registry");
        assert_eq!(
            reopened.current("worker-a").await.unwrap(),
            Some(registered),
            "R3"
        );
    }
}
