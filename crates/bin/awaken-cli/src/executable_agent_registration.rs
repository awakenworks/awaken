//! Role-specific composition of the executable Agent registration boundary.

use std::sync::Arc;

use awaken_executable_agent_catalog::{
    ExecutableAgentCatalog, HttpExecutableAgentRegistrar, LocalExecutableAgentRegistrar,
    PostgresExecutableAgentRegistrar, executable_agent_registration_router,
};
use awaken_executable_agent_contract::ExecutableAgentRegistrar;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

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
    environments: Arc<awaken_protocol_managed::EnvironmentApplication>,
    captured_content: Arc<dyn awaken_runtime_contract::ContentEraser>,
) -> Result<ExecutableAgentWiring, String> {
    match role {
        Role::AllInOne => Ok(ExecutableAgentWiring::local()),
        Role::Coordinator => {
            ExecutableAgentWiring::coordinator(deployment, schema, environments, captured_content)
                .await
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
    pub(crate) environment_author: Option<Arc<dyn awaken_admin_assistant::EnvironmentAuthor>>,
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
            environment_author: None,
            coordinator_content_eraser: None,
        }
    }

    pub(crate) fn control(deployment: &ResolvedDeployment) -> Result<Self, String> {
        let (coordinator_url, token) = deployment
            .executable_agent_registration
            .control_credentials()?;
        let registrar = HttpExecutableAgentRegistrar::new(coordinator_url, token.clone())
            .map_err(|error| error.to_string())?;
        let environment_author = Arc::new(
            awaken_server::environment_boundary::HttpEnvironmentAuthor::new(
                coordinator_url,
                token.clone(),
            )?,
        );
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
            environment_author: Some(environment_author),
            coordinator_content_eraser: Some(coordinator_content_eraser),
        })
    }

    #[cfg(test)]
    pub(crate) fn local_server(token: &str) -> Self {
        let catalog = Arc::new(ExecutableAgentCatalog::new());
        let registrar = Arc::new(LocalExecutableAgentRegistrar::new(catalog.clone()));
        let private_router = executable_agent_registration_router(registrar.clone(), token)
            .expect("test registration router");
        let environment_author = Arc::new(
            awaken_server::environment_boundary::LocalEnvironmentAuthor::new(
                awaken_protocol_managed::EnvironmentState::new().application(),
            ),
        );
        Self {
            catalog,
            registrar,
            projection_refresher: None,
            private_router,
            environment_author: Some(environment_author),
            coordinator_content_eraser: None,
        }
    }

    pub(crate) async fn coordinator(
        deployment: &ResolvedDeployment,
        schema: PostgresSchemaMode,
        environments: Arc<awaken_protocol_managed::EnvironmentApplication>,
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
        let private_router = private_router.merge(
            awaken_server::environment_boundary::router(environments, token.clone())
                .map_err(|error| format!("construct Environment command router: {error}"))?,
        );
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
            environment_author: None,
            coordinator_content_eraser: None,
        })
    }
}

type ProcessParts = (
    Arc<ExecutableAgentCatalog>,
    Arc<dyn ExecutableAgentRegistrar>,
    Router,
    Option<Arc<PostgresExecutableAgentRegistrar>>,
    Option<Arc<dyn awaken_admin_assistant::EnvironmentAuthor>>,
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
        wiring.environment_author,
        wiring.coordinator_content_eraser,
    )
}

/// Install the active-active reconciliation middleware only for the durable
/// Coordinator composition. Keeping this assembly with the registration
/// boundary prevents the CLI composition root from owning its transport rules.
pub(crate) fn layer_refresh(
    router: Router,
    refresher: Option<Arc<PostgresExecutableAgentRegistrar>>,
) -> Router {
    match refresher {
        Some(refresher) => router.layer(axum::middleware::from_fn_with_state(
            refresher,
            refresh_before_session_runtime_use,
        )),
        None => router,
    }
}

fn requires_agent_projection_refresh(method: &Method, path: &str) -> bool {
    method == Method::POST
        && (path.ends_with("/v1/sessions")
            || path.contains("/v1/sessions/")
            || path.ends_with("/v1/deployments")
            || path.contains("/v1/deployments/"))
}

/// Reconcile before a request can admit or resume Session Runtime work. In an
/// active-active deployment, Session creation and its first event can reach
/// different Coordinator replicas; both must resolve the same durable Agent
/// publication before the Host opens the frozen Session projection. Worker
/// transport and read-only Session operations stay off this database read path.
pub(crate) async fn refresh_before_session_runtime_use(
    State(registrar): State<Arc<PostgresExecutableAgentRegistrar>>,
    request: Request,
    next: Next,
) -> Response {
    if requires_agent_projection_refresh(request.method(), request.uri().path())
        && let Err(error) = registrar.refresh_projection().await
    {
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_refresh_scope_covers_cross_replica_runtime_writes() {
        // Causes: C1 Session create, C2 existing-Session POST that can realize or
        // resume Runtime work, C3 Deployment create/run, C4 read-only Session
        // request, C5 unrelated POST. Effects: E1 refresh the one durable Agent
        // projection before continuing; E2 do not add a database read.
        //
        // Decision table:
        // | rule | method | path                              | effect |
        // | R1   | POST   | /v1/sessions                      | E1     |
        // | R2   | POST   | /v1/sessions/{id}/events          | E1     |
        // | R3   | POST   | /v1/deployments/{id}/run           | E1     |
        // | R4   | GET    | /v1/sessions/{id}                 | E2     |
        // | R5   | POST   | /v1/agents                        | E2     |
        assert!(requires_agent_projection_refresh(
            &Method::POST,
            "/v1/sessions"
        ));
        assert!(requires_agent_projection_refresh(
            &Method::POST,
            "/v1/sessions/sesn_1/events"
        ));
        assert!(requires_agent_projection_refresh(
            &Method::POST,
            "/v1/deployments/depl_1/run"
        ));
        assert!(!requires_agent_projection_refresh(
            &Method::GET,
            "/v1/sessions/sesn_1"
        ));
        assert!(!requires_agent_projection_refresh(
            &Method::POST,
            "/v1/agents"
        ));
    }
}
