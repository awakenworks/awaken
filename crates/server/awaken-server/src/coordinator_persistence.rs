//! Coordinator-owned Postgres persistence composition.
//!
//! The migration command and both standalone/AllInOne startup reuse this one
//! component boundary. Operational migration applies bundles without publishing
//! process globals; Local startup may migrate and connect; Server startup only
//! verifies ledgers before connecting.

use awaken_runtime_host::{DeploymentConfig, DispatchBackend, StoreKind};

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
        super::worker_registry::migrate_postgres(url).await?;
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

/// Connect configured Coordinator-owned Postgres stores, applying their schema
/// first. This is the Local-mode path.
pub async fn init_postgres(deployment: &DeploymentConfig) -> Result<(), String> {
    init_postgres_with(deployment, SchemaAccess::Migrate).await
}

/// Connect configured Coordinator-owned Postgres stores after verifying their
/// externally-applied ledgers. This is the Server-mode path and executes no DDL.
pub async fn init_existing_postgres(deployment: &DeploymentConfig) -> Result<(), String> {
    init_postgres_with(deployment, SchemaAccess::Verify).await
}

async fn init_postgres_with(
    deployment: &DeploymentConfig,
    schema: SchemaAccess,
) -> Result<(), String> {
    let components = postgres_components(deployment);
    let Some(url) = database_url(deployment)? else {
        return Ok(());
    };
    if components.dispatch {
        match schema {
            SchemaAccess::Migrate => {
                awaken_runtime_host::init_shared_postgres_dispatch_with_config(url, deployment)
                    .await?;
                super::worker_registry::init_postgres(url).await?;
            }
            SchemaAccess::Verify => {
                awaken_runtime_host::init_shared_postgres_dispatch_existing_with_config(
                    url, deployment,
                )
                .await?;
                super::worker_registry::init_existing_postgres(url).await?;
            }
        }
    }
    if components.commit {
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
}
