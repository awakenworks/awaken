//! Role-specific composition of the executable Environment boundary.

use std::sync::Arc;

use awaken_executable_environment_catalog::{
    ExecutableEnvironmentCatalog, HttpExecutableEnvironmentRegistrar,
    LocalExecutableEnvironmentRegistrar, PostgresExecutableEnvironmentRegistrar,
    executable_environment_registration_router,
};
use awaken_executable_environment_contract::ExecutableEnvironmentRegistrar;
use awaken_session_contract::work_queue::WorkQueue;
use axum::Router;

use crate::PostgresSchemaMode;
use crate::config::{ResolvedDeployment, Role};

pub(crate) struct ExecutableEnvironmentWiring {
    pub(crate) catalog: Arc<ExecutableEnvironmentCatalog>,
    pub(crate) registrar: Arc<dyn ExecutableEnvironmentRegistrar>,
    pub(crate) projection_refresher: Option<Arc<PostgresExecutableEnvironmentRegistrar>>,
    pub(crate) private_router: Router,
    pub(crate) image_builds:
        Option<Arc<awaken_environment_image_build::EnvironmentImageBuildCoordinator>>,
}

impl ExecutableEnvironmentWiring {
    pub(crate) fn local(work: Arc<dyn WorkQueue>) -> Result<Self, String> {
        let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let registrar: Arc<dyn ExecutableEnvironmentRegistrar> = Arc::new(
            awaken_protocol_managed::CoordinatorEnvironmentRegistrar::new(
                Arc::new(LocalExecutableEnvironmentRegistrar::new(catalog.clone())),
                work,
            ),
        );
        Ok(Self {
            catalog,
            registrar,
            projection_refresher: None,
            private_router: Router::new(),
            image_builds: None,
        })
    }

    pub(crate) fn control(deployment: &ResolvedDeployment) -> Result<Self, String> {
        Ok(Self {
            catalog: Arc::new(ExecutableEnvironmentCatalog::new()),
            registrar: control_registrar(deployment)?,
            projection_refresher: None,
            private_router: Router::new(),
            image_builds: None,
        })
    }

    async fn coordinator(
        deployment: &ResolvedDeployment,
        schema: PostgresSchemaMode,
        work: Arc<dyn WorkQueue>,
    ) -> Result<Self, String> {
        let token = deployment
            .executable_agent_registration
            .coordinator_token()?;
        let database_url = deployment.runtime.database_url.as_deref().ok_or_else(|| {
            "Coordinator requires runtime_database_url for executable Environment registration"
                .to_owned()
        })?;
        let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let durable = Arc::new(
            match schema {
                PostgresSchemaMode::Migrate => {
                    PostgresExecutableEnvironmentRegistrar::connect(database_url, catalog.clone())
                        .await
                }
                PostgresSchemaMode::Verify => {
                    PostgresExecutableEnvironmentRegistrar::connect_existing(
                        database_url,
                        catalog.clone(),
                    )
                    .await
                }
            }
            .map_err(|error| error.to_string())?,
        );
        let image_builds = open_image_builds(deployment, schema).await?;
        let build_aware: Arc<dyn ExecutableEnvironmentRegistrar> = match &image_builds {
            Some(builds) => Arc::new(
                awaken_environment_image_build::BuildAwareExecutableEnvironmentRegistrar::new(
                    durable.clone(),
                    builds.clone(),
                ),
            ),
            None => durable.clone(),
        };
        let registrar: Arc<dyn ExecutableEnvironmentRegistrar> = Arc::new(
            awaken_protocol_managed::CoordinatorEnvironmentRegistrar::new(build_aware, work),
        );
        let private_router =
            executable_environment_registration_router(registrar.clone(), token)
                .map_err(|error| format!("construct executable Environment router: {error}"))?;
        Ok(Self {
            catalog,
            registrar,
            projection_refresher: Some(durable),
            private_router,
            image_builds,
        })
    }
}

pub(crate) async fn for_runtime_role(
    role: Role,
    deployment: &ResolvedDeployment,
    schema: PostgresSchemaMode,
    work: Arc<dyn WorkQueue>,
) -> Result<ExecutableEnvironmentWiring, String> {
    match role {
        Role::AllInOne => {
            let mut wiring = ExecutableEnvironmentWiring::local(work)?;
            wiring.image_builds = open_image_builds(deployment, schema).await?;
            if let Some(builds) = &wiring.image_builds {
                wiring.registrar = Arc::new(
                    awaken_environment_image_build::BuildAwareExecutableEnvironmentRegistrar::new(
                        wiring.registrar,
                        builds.clone(),
                    ),
                );
            }
            Ok(wiring)
        }
        Role::Coordinator => {
            ExecutableEnvironmentWiring::coordinator(deployment, schema, work).await
        }
        Role::Control | Role::Worker => {
            unreachable!("runtime assembly accepts only AllInOne or Coordinator")
        }
    }
}

async fn open_image_builds(
    deployment: &ResolvedDeployment,
    schema: PostgresSchemaMode,
) -> Result<Option<Arc<awaken_environment_image_build::EnvironmentImageBuildCoordinator>>, String> {
    let Some(provisioner) =
        awaken_runtime_host::package_image_provisioner(&deployment.runtime).await?
    else {
        return Ok(None);
    };
    let builder =
        awaken_environment_package_image_builder::package_environment_image_builder(provisioner);
    let base_image = deployment
        .runtime
        .container_image
        .clone()
        .ok_or_else(|| "Environment image builder requires container_image".to_owned())?;
    let store = match &deployment.coordinator.sessions {
        awaken_control::StoreBackend::Sqlite(_) => {
            awaken_environment_image_build::open_sqlite_environment_image_build_store(
                deployment.data_dir.join("environment_images.db"),
            )
            .map_err(|error| error.to_string())?
        }
        awaken_control::StoreBackend::Postgres(database_url) => match schema {
            PostgresSchemaMode::Migrate => {
                awaken_environment_image_build::connect_postgres_environment_image_build_store(
                    database_url,
                )
                .await
            }
            PostgresSchemaMode::Verify => {
                awaken_environment_image_build::connect_existing_postgres_environment_image_build_store(
                    database_url,
                )
                .await
            }
        }
        .map_err(|error| error.to_string())?,
    };
    let coordinator = Arc::new(
        awaken_environment_image_build::EnvironmentImageBuildCoordinator::new(
            store,
            builder,
            base_image,
            awaken_environment_image_build::EnvironmentImageBuildPolicy::default(),
        )
        .map_err(|error| error.to_string())?,
    );
    coordinator.spawn_worker(deployment.runtime.dispatch_owner.clone());
    Ok(Some(coordinator))
}

pub(crate) fn control_registrar(
    deployment: &ResolvedDeployment,
) -> Result<Arc<dyn ExecutableEnvironmentRegistrar>, String> {
    let (coordinator_url, token) = deployment
        .executable_agent_registration
        .control_credentials()?;
    Ok(Arc::new(
        HttpExecutableEnvironmentRegistrar::new(coordinator_url, token)
            .map_err(|error| error.to_string())?,
    ))
}

pub(crate) async fn migrate(deployment: &ResolvedDeployment) -> Result<(), String> {
    let Some(database_url) = deployment.runtime.database_url.as_deref() else {
        return Ok(());
    };
    PostgresExecutableEnvironmentRegistrar::connect(
        database_url,
        Arc::new(ExecutableEnvironmentCatalog::new()),
    )
    .await
    .map(drop)
    .map_err(|error| format!("migrate executable Environment catalog: {error}"))?;
    awaken_environment_image_build::connect_postgres_environment_image_build_store(database_url)
        .await
        .map(drop)
        .map_err(|error| format!("migrate Environment image-build jobs: {error}"))
}

#[cfg(test)]
pub(crate) fn local_test_wiring() -> ExecutableEnvironmentWiring {
    ExecutableEnvironmentWiring::local(Arc::new(awaken_work_store::InMemoryWorkQueue::new()))
        .expect("compose test executable Environment wiring")
}

/// Require the process-selected Environment boundary. Local AllInOne remains
/// supported through an explicit [`ExecutableEnvironmentWiring::local`] value.
pub(crate) fn require_process_wiring(
    wiring: Option<ExecutableEnvironmentWiring>,
) -> ExecutableEnvironmentWiring {
    wiring.expect("runtime process requires executable Environment wiring")
}

#[cfg(test)]
mod composition_tests {
    #[test]
    #[should_panic(expected = "runtime process requires executable Environment wiring")]
    fn missing_wiring_never_allocates_a_parallel_catalog_or_queue() {
        // Cause/effect decision table: E1 explicit local wiring -> catalog shares
        // the caller's WorkQueue; E2 explicit distributed wiring -> durable
        // catalog/queue; E3 missing wiring -> composition failure. Positive E1/E2
        // are covered by role assembly; this test owns E3.
        let _ = super::require_process_wiring(None);
    }
}
