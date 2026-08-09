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
        })
    }

    pub(crate) fn control(deployment: &ResolvedDeployment) -> Result<Self, String> {
        Ok(Self {
            catalog: Arc::new(ExecutableEnvironmentCatalog::new()),
            registrar: control_registrar(deployment)?,
            projection_refresher: None,
            private_router: Router::new(),
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
        let registrar: Arc<dyn ExecutableEnvironmentRegistrar> = Arc::new(
            awaken_protocol_managed::CoordinatorEnvironmentRegistrar::new(durable.clone(), work),
        );
        let private_router =
            executable_environment_registration_router(registrar.clone(), token)
                .map_err(|error| format!("construct executable Environment router: {error}"))?;
        Ok(Self {
            catalog,
            registrar,
            projection_refresher: Some(durable),
            private_router,
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
        Role::AllInOne => ExecutableEnvironmentWiring::local(work),
        Role::Coordinator => {
            ExecutableEnvironmentWiring::coordinator(deployment, schema, work).await
        }
        Role::Control | Role::Worker => {
            unreachable!("runtime assembly accepts only AllInOne or Coordinator")
        }
    }
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
    .map_err(|error| format!("migrate executable Environment catalog: {error}"))
}
