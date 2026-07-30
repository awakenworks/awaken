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

pub(crate) struct CatalogDeclaredHandSource(pub(crate) Arc<ExecutableAgentCatalog>);

impl awaken_server::placement::DeclaredHandSource for CatalogDeclaredHandSource {
    fn declared_hand(&self, agent_id: &str) -> Result<Option<String>, String> {
        self.0.declared_hand_for_agent(agent_id)
    }
}

pub(crate) async fn for_runtime_role(
    role: Role,
    deployment: &ResolvedDeployment,
    schema: PostgresSchemaMode,
) -> Result<ExecutableAgentWiring, String> {
    match role {
        Role::AllInOne => Ok(ExecutableAgentWiring::local()),
        Role::Coordinator => ExecutableAgentWiring::coordinator(deployment, schema).await,
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
    pub(crate) private_router: Router,
}

impl ExecutableAgentWiring {
    pub(crate) fn local() -> Self {
        let catalog = Arc::new(ExecutableAgentCatalog::new());
        Self {
            registrar: Arc::new(LocalExecutableAgentRegistrar::new(catalog.clone())),
            catalog,
            private_router: Router::new(),
        }
    }

    pub(crate) fn control(deployment: &ResolvedDeployment) -> Result<Self, String> {
        let (coordinator_url, token) = deployment
            .executable_agent_registration
            .control_credentials()?;
        let registrar = HttpExecutableAgentRegistrar::new(coordinator_url, token)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            catalog: Arc::new(ExecutableAgentCatalog::new()),
            registrar: Arc::new(registrar),
            private_router: Router::new(),
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
            private_router,
        }
    }

    pub(crate) async fn coordinator(
        deployment: &ResolvedDeployment,
        schema: PostgresSchemaMode,
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
        let private_router = executable_agent_registration_router(registrar.clone(), token)
            .map_err(|error| format!("construct executable Agent registration router: {error}"))?;
        Ok(Self {
            catalog,
            registrar,
            private_router,
        })
    }
}
