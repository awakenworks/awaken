//! Role-specific composition of the executable Agent registration boundary.

use std::sync::Arc;

use awaken_executable_agent_catalog::{
    ExecutableAgentCatalog, HttpExecutableAgentRegistrar, LocalExecutableAgentRegistrar,
    PostgresExecutableAgentRegistrar, executable_agent_registration_router,
};
use awaken_executable_agent_contract::ExecutableAgentRegistrar;
use axum::Router;

use crate::PostgresSchemaMode;
use crate::config::{ResolvedDeployment, Role};

pub(crate) async fn for_runtime_role(
    role: Role,
    deployment: &ResolvedDeployment,
    schema: PostgresSchemaMode,
    captured_content: Arc<dyn awaken_runtime_contract::ContentEraser>,
) -> Result<ExecutableAgentWiring, String> {
    match role {
        Role::AllInOne => Ok(ExecutableAgentWiring::local()),
        Role::Coordinator => {
            ExecutableAgentWiring::coordinator(deployment, schema, captured_content).await
        }
        _ => unreachable!("runtime assembly accepts only AllInOne or Coordinator"),
    }
}

pub(crate) async fn migrate(deployment: &ResolvedDeployment) -> Result<(), String> {
    let Some(database_url) = deployment.runtime.database_url.as_deref() else {
        return Ok(());
    };
    PostgresExecutableAgentRegistrar::connect(database_url, Arc::new(ExecutableAgentCatalog::new()))
        .await
        .map(drop)
        .map_err(|error| format!("migrate executable Agent catalog: {error}"))
}

pub(crate) struct ExecutableAgentWiring {
    pub(crate) catalog: Arc<ExecutableAgentCatalog>,
    pub(crate) registrar: Arc<dyn ExecutableAgentRegistrar>,
    pub(crate) projection_refresher: Option<Arc<PostgresExecutableAgentRegistrar>>,
    pub(crate) private_router: Router,
    pub(crate) coordinator_content_eraser: Option<Arc<dyn awaken_runtime_contract::ContentEraser>>,
}

impl ExecutableAgentWiring {
    pub(crate) fn local() -> Self {
        let catalog = Arc::new(ExecutableAgentCatalog::new());
        Self {
            registrar: Arc::new(LocalExecutableAgentRegistrar::new(catalog.clone())),
            catalog,
            projection_refresher: None,
            private_router: Router::new(),
            coordinator_content_eraser: None,
        }
    }

    pub(crate) fn control(deployment: &ResolvedDeployment) -> Result<Self, String> {
        let (coordinator_url, token) = deployment
            .executable_agent_registration
            .control_credentials()?;
        let registrar = HttpExecutableAgentRegistrar::new(coordinator_url, token.clone())
            .map_err(|error| error.to_string())?;
        let coordinator_content_eraser = Arc::new(
            awaken_server::data_subject_boundary::HttpCoordinatorContentEraser::new(
                coordinator_url,
                token,
            )?,
        );
        Ok(Self {
            catalog: Arc::new(ExecutableAgentCatalog::new()),
            registrar: Arc::new(registrar),
            projection_refresher: None,
            private_router: Router::new(),
            coordinator_content_eraser: Some(coordinator_content_eraser),
        })
    }

    #[cfg(test)]
    pub(crate) fn local_server(token: &str) -> Self {
        let catalog = Arc::new(ExecutableAgentCatalog::new());
        let registrar = Arc::new(LocalExecutableAgentRegistrar::new(catalog.clone()));
        let private_router = executable_agent_registration_router(registrar.clone(), token)
            .expect("test registration router");
        Self {
            catalog,
            registrar,
            projection_refresher: None,
            private_router,
            coordinator_content_eraser: None,
        }
    }

    pub(crate) async fn coordinator(
        deployment: &ResolvedDeployment,
        schema: PostgresSchemaMode,
        captured_content: Arc<dyn awaken_runtime_contract::ContentEraser>,
    ) -> Result<Self, String> {
        let token = deployment
            .executable_agent_registration
            .coordinator_token()?;
        let database_url = deployment.runtime.database_url.as_deref().ok_or_else(|| {
            "Coordinator requires runtime_database_url for executable Agent registration".to_owned()
        })?;
        let catalog = Arc::new(ExecutableAgentCatalog::new());
        let registrar = match schema {
            PostgresSchemaMode::Migrate => {
                PostgresExecutableAgentRegistrar::connect(database_url, catalog.clone()).await
            }
            PostgresSchemaMode::Verify => {
                PostgresExecutableAgentRegistrar::connect_existing(database_url, catalog.clone())
                    .await
            }
        }
        .map_err(|error| error.to_string())?;
        let registrar = Arc::new(registrar);
        let private_router = executable_agent_registration_router(registrar.clone(), token.clone())
            .map_err(|error| format!("construct executable Agent registration router: {error}"))?;
        let coordinator_content = awaken_server::data_subject_boundary::coordinator_content_eraser(
            captured_content,
            deployment.runtime.acp_session_blob_root.clone(),
        );
        let private_router = private_router.merge(
            awaken_server::data_subject_boundary::router(coordinator_content, token).map_err(
                |error| format!("construct Coordinator content-erasure router: {error}"),
            )?,
        );
        Ok(Self {
            catalog,
            registrar: registrar.clone(),
            projection_refresher: Some(registrar),
            private_router,
            coordinator_content_eraser: None,
        })
    }
}

type ProcessParts = (
    Arc<ExecutableAgentCatalog>,
    Arc<dyn ExecutableAgentRegistrar>,
    Router,
    Option<Arc<PostgresExecutableAgentRegistrar>>,
    Option<Arc<dyn awaken_runtime_contract::ContentEraser>>,
);

/// Consume the role wiring into its process-assembly values. The boundary module
/// owns the local default as well as the distributed selection.
pub(crate) fn process_parts(wiring: Option<ExecutableAgentWiring>) -> ProcessParts {
    let wiring = wiring.unwrap_or_else(ExecutableAgentWiring::local);
    (
        wiring.catalog,
        wiring.registrar,
        wiring.private_router,
        wiring.projection_refresher,
        wiring.coordinator_content_eraser,
    )
}
