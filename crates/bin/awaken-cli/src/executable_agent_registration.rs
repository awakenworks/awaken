//! Role-specific startup of the executable Agent registration boundary.

use std::sync::Arc;

use awaken_executable_agent_catalog::{
    ExecutableAgentCatalog, HttpExecutableAgentRegistrar, LocalExecutableAgentRegistrar,
    PostgresExecutableAgentRegistrar, ReferenceIndexedExecutableAgentRegistrar,
    executable_agent_registration_router_with_authenticator,
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
    references: Arc<dyn awaken_resource_contract::ResourceReferenceIndex>,
) -> Result<ExecutableAgentWiring, String> {
    match role {
        Role::AllInOne => Ok(ExecutableAgentWiring::local_with_references(references)),
        Role::Coordinator => {
            ExecutableAgentWiring::coordinator(deployment, schema, captured_content, references)
                .await
        }
        _ => unreachable!("runtime process accepts only AllInOne or Coordinator"),
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
    #[cfg(any(test, feature = "test-support"))]
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

    pub(crate) fn local_with_references(
        references: Arc<dyn awaken_resource_contract::ResourceReferenceIndex>,
    ) -> Self {
        let catalog = Arc::new(ExecutableAgentCatalog::new());
        let delegate = Arc::new(LocalExecutableAgentRegistrar::new(catalog.clone()));
        Self {
            registrar: Arc::new(ReferenceIndexedExecutableAgentRegistrar::new(
                catalog.clone(),
                delegate,
                references,
            )),
            catalog,
            projection_refresher: None,
            private_router: Router::new(),
            coordinator_content_eraser: None,
        }
    }

    pub(crate) fn control(deployment: &ResolvedDeployment) -> Result<Self, String> {
        let (coordinator_url, token_source) = deployment
            .executable_agent_registration
            .control_credentials()?;
        let registrar =
            HttpExecutableAgentRegistrar::with_token_source(coordinator_url, token_source.clone())
                .map_err(|error| error.to_string())?;
        let coordinator_content_eraser = Arc::new(
            awaken_coordinator::data_subject_boundary::HttpCoordinatorContentEraser::with_token_source(
                coordinator_url,
                token_source,
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
        let private_router = awaken_executable_agent_catalog::executable_agent_registration_router(
            registrar.clone(),
            token,
        )
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
        references: Arc<dyn awaken_resource_contract::ResourceReferenceIndex>,
    ) -> Result<Self, String> {
        let authenticator = deployment
            .executable_agent_registration
            .coordinator_authenticator()?;
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
        let projection_refresher = Arc::new(registrar);
        let indexed = Arc::new(ReferenceIndexedExecutableAgentRegistrar::new(
            catalog.clone(),
            projection_refresher.clone(),
            references,
        ));
        indexed
            .synchronize_current_references()
            .await
            .map_err(|error| error.to_string())?;
        let registrar: Arc<dyn ExecutableAgentRegistrar> = indexed;
        let private_router = executable_agent_registration_router_with_authenticator(
            registrar.clone(),
            authenticator.clone(),
        );
        let coordinator_content =
            awaken_coordinator::data_subject_boundary::coordinator_content_eraser(
                captured_content,
                deployment.runtime.acp_session_blob_root.clone(),
            );
        let private_router = private_router.merge(
            awaken_coordinator::data_subject_boundary::router_with_authenticator(
                coordinator_content,
                authenticator,
            ),
        );
        Ok(Self {
            catalog,
            registrar: registrar.clone(),
            projection_refresher: Some(projection_refresher),
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

/// Consume the explicitly selected role wiring into executable-Agent services.
/// A missing value is a startup defect; local single-process operation is
/// represented by an explicit [`ExecutableAgentWiring::local`] value.
pub(crate) fn process_parts(wiring: Option<ExecutableAgentWiring>) -> ProcessParts {
    let wiring = wiring.expect("product process requires executable Agent wiring");
    (
        wiring.catalog,
        wiring.registrar,
        wiring.private_router,
        wiring.projection_refresher,
        wiring.coordinator_content_eraser,
    )
}

#[cfg(test)]
mod tests {
    #[test]
    #[should_panic(expected = "product process requires executable Agent wiring")]
    fn missing_wiring_never_falls_back_to_a_volatile_catalog() {
        // Cause/effect decision table: A1 explicit local wiring -> the caller's
        // one local catalog; A2 explicit distributed wiring -> its durable/HTTP
        // adapter; A3 missing wiring -> startup failure. A1/A2 are exercised
        // by product-process tests; this case owns A3 and prevents a healthy-
        // looking empty catalog from replacing durable authority.
        let _ = super::process_parts(None);
    }
}
