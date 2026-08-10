//! Authenticated HTTP adapters for the executable Agent registrar port.

use std::sync::Arc;
use std::time::Duration;

use awaken_executable_agent_contract::{
    ExecutableAgentRegistrar, ExecutableAgentRegistration, ExecutableAgentRegistrationError,
    ExecutableAgentRegistrationOutcome, ExecutableAgentWithdrawal,
    ExecutableAgentWithdrawalOutcome,
};
use awaken_service_auth_contract::{
    COORDINATOR_SERVICE_AUDIENCE, ServiceAuthError, ServiceAuthorizationRequirement,
    ServiceBearerTokenSource, ServiceRequestAuthenticator,
};
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::post;
use axum::{Router, response::IntoResponse};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub const EXECUTABLE_AGENT_REGISTER_PATH: &str = "/internal/v1/executable-agents/register";
pub const EXECUTABLE_AGENT_WITHDRAW_PATH: &str = "/internal/v1/executable-agents/withdraw";
pub const EXECUTABLE_AGENT_PUBLISH_PERMISSION: &str = "agent:publish";
pub const EXECUTABLE_AGENT_WITHDRAW_PERMISSION: &str = "agent:withdraw";

const IDEMPOTENT_ATTEMPTS: usize = 3;
const RETRY_DELAY: Duration = Duration::from_millis(25);

#[derive(Clone)]
struct RegistrationHttpState {
    registrar: Arc<dyn ExecutableAgentRegistrar>,
    authenticator: Arc<dyn ServiceRequestAuthenticator>,
}

/// Coordinator's private registration surface. The token is mandatory because
/// this mutation changes executable availability even though the route is not a
/// public management API.
pub fn executable_agent_registration_router(
    registrar: Arc<dyn ExecutableAgentRegistrar>,
    bearer_token: impl Into<String>,
) -> Result<Router, String> {
    Ok(executable_agent_registration_router_with_authenticator(
        registrar,
        awaken_service_auth_contract::static_token_authenticator(bearer_token)?,
    ))
}

pub fn executable_agent_registration_router_with_authenticator(
    registrar: Arc<dyn ExecutableAgentRegistrar>,
    authenticator: Arc<dyn ServiceRequestAuthenticator>,
) -> Router {
    Router::new()
        .route(EXECUTABLE_AGENT_REGISTER_PATH, post(register))
        .route(EXECUTABLE_AGENT_WITHDRAW_PATH, post(withdraw))
        .with_state(RegistrationHttpState {
            registrar,
            authenticator,
        })
}

async fn register(
    State(state): State<RegistrationHttpState>,
    headers: HeaderMap,
    Json(command): Json<ExecutableAgentRegistration>,
) -> impl IntoResponse {
    if let Err(status) = authorize(
        &headers,
        state.authenticator.as_ref(),
        ServiceAuthorizationRequirement::new(
            COORDINATOR_SERVICE_AUDIENCE,
            EXECUTABLE_AGENT_PUBLISH_PERMISSION,
        )
        .in_workspace(&command.workspace_id),
    ) {
        return status.into_response();
    }
    match state.registrar.register(command).await {
        Ok(outcome) => (
            StatusCode::OK,
            Json(Some(Ok::<_, ExecutableAgentRegistrationError>(outcome))),
        )
            .into_response(),
        Err(error) => registration_error_response(error),
    }
}

async fn withdraw(
    State(state): State<RegistrationHttpState>,
    headers: HeaderMap,
    Json(command): Json<ExecutableAgentWithdrawal>,
) -> impl IntoResponse {
    if let Err(status) = authorize(
        &headers,
        state.authenticator.as_ref(),
        ServiceAuthorizationRequirement::new(
            COORDINATOR_SERVICE_AUDIENCE,
            EXECUTABLE_AGENT_WITHDRAW_PERMISSION,
        )
        .in_workspace(&command.workspace_id),
    ) {
        return status.into_response();
    }
    match state.registrar.withdraw(command).await {
        Ok(outcome) => (
            StatusCode::OK,
            Json(Some(Ok::<_, ExecutableAgentRegistrationError>(outcome))),
        )
            .into_response(),
        Err(error) => registration_error_response(error),
    }
}

fn authorize(
    headers: &HeaderMap,
    authenticator: &dyn ServiceRequestAuthenticator,
    requirement: ServiceAuthorizationRequirement<'_>,
) -> Result<(), StatusCode> {
    match authenticator.authenticate(
        headers
            .get(header::AUTHORIZATION)
            .map(|value| value.as_bytes()),
        requirement,
    ) {
        Ok(_) => Ok(()),
        Err(ServiceAuthError::Unauthorized) => Err(StatusCode::UNAUTHORIZED),
        Err(ServiceAuthError::Forbidden) => Err(StatusCode::FORBIDDEN),
        Err(ServiceAuthError::Unavailable(_)) => Err(StatusCode::SERVICE_UNAVAILABLE),
    }
}

fn registration_error_response(
    error: ExecutableAgentRegistrationError,
) -> axum::response::Response {
    let status = match error {
        ExecutableAgentRegistrationError::Invalid(_) => StatusCode::BAD_REQUEST,
        ExecutableAgentRegistrationError::Conflict(_) => StatusCode::CONFLICT,
        ExecutableAgentRegistrationError::Unavailable(_)
        | ExecutableAgentRegistrationError::Storage(_) => StatusCode::SERVICE_UNAVAILABLE,
    };
    (status, Json(Some(Err::<serde_json::Value, _>(error)))).into_response()
}

/// Control-side network adapter for the same registrar port used by AllInOne.
#[derive(Clone)]
pub struct HttpExecutableAgentRegistrar {
    base_url: String,
    token_source: Arc<dyn ServiceBearerTokenSource>,
    client: reqwest::Client,
}

impl HttpExecutableAgentRegistrar {
    pub fn new(
        base_url: impl Into<String>,
        bearer_token: impl Into<String>,
    ) -> Result<Self, ExecutableAgentRegistrationError> {
        let source = awaken_service_auth_contract::static_token_source(bearer_token)
            .map_err(ExecutableAgentRegistrationError::Invalid)?;
        Self::with_token_source(base_url, source)
    }

    pub fn with_token_source(
        base_url: impl Into<String>,
        token_source: Arc<dyn ServiceBearerTokenSource>,
    ) -> Result<Self, ExecutableAgentRegistrationError> {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        if base_url.is_empty() {
            return Err(ExecutableAgentRegistrationError::Invalid(
                "Coordinator URL and registration bearer token are required".into(),
            ));
        }
        awaken_service_auth_contract::resolve_service_bearer_token(token_source.as_ref())
            .map_err(ExecutableAgentRegistrationError::Invalid)?;
        let parsed = reqwest::Url::parse(&base_url).map_err(|error| {
            ExecutableAgentRegistrationError::Invalid(format!(
                "invalid Coordinator registration URL: {error}"
            ))
        })?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(ExecutableAgentRegistrationError::Invalid(
                "Coordinator registration URL must be an http(s) base URL without query or fragment"
                    .into(),
            ));
        }
        Ok(Self {
            base_url,
            token_source,
            client: reqwest::Client::builder()
                .no_proxy()
                .connect_timeout(std::time::Duration::from_secs(5))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .map_err(|error| {
                    ExecutableAgentRegistrationError::Unavailable(error.to_string())
                })?,
        })
    }

    #[must_use]
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    async fn post<T: Serialize + ?Sized, O: DeserializeOwned>(
        &self,
        path: &str,
        command: &T,
    ) -> Result<O, ExecutableAgentRegistrationError> {
        let mut last_unavailable = None;
        for attempt in 1..=IDEMPOTENT_ATTEMPTS {
            let bearer_token = awaken_service_auth_contract::resolve_service_bearer_token(
                self.token_source.as_ref(),
            )
            .map_err(ExecutableAgentRegistrationError::Unavailable)?;
            let response = self
                .client
                .post(format!("{}{}", self.base_url, path))
                .bearer_auth(bearer_token.as_ref())
                .json(command)
                .send()
                .await;
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    last_unavailable = Some(error.to_string());
                    if attempt < IDEMPOTENT_ATTEMPTS {
                        tokio::time::sleep(RETRY_DELAY).await;
                        continue;
                    }
                    break;
                }
            };
            if response.status() == StatusCode::UNAUTHORIZED {
                let rotated = awaken_service_auth_contract::service_bearer_token_rotated(
                    self.token_source.as_ref(),
                    bearer_token.as_ref(),
                )
                .map_err(ExecutableAgentRegistrationError::Unavailable)?;
                if attempt < IDEMPOTENT_ATTEMPTS && rotated {
                    continue;
                }
                return Err(ExecutableAgentRegistrationError::Invalid(
                    "Coordinator rejected registration credentials".into(),
                ));
            }
            let status = response.status();
            let decoded = response
                .json::<Option<Result<O, ExecutableAgentRegistrationError>>>()
                .await;
            if status.is_success() {
                return decoded
                    .map_err(|error| {
                        ExecutableAgentRegistrationError::Unavailable(format!(
                            "Coordinator registration response decode failed: {error}"
                        ))
                    })?
                    .ok_or_else(|| {
                        ExecutableAgentRegistrationError::Unavailable(
                            "Coordinator returned an empty registration response".into(),
                        )
                    })?;
            }
            if let Ok(Some(Err(error))) = decoded {
                if matches!(
                    error,
                    ExecutableAgentRegistrationError::Unavailable(_)
                        | ExecutableAgentRegistrationError::Storage(_)
                ) && attempt < IDEMPOTENT_ATTEMPTS
                {
                    last_unavailable = Some(error.to_string());
                    tokio::time::sleep(RETRY_DELAY).await;
                    continue;
                }
                return Err(error);
            }
            if status.is_server_error() && attempt < IDEMPOTENT_ATTEMPTS {
                last_unavailable = Some(format!("Coordinator returned {status}"));
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
            return Err(ExecutableAgentRegistrationError::Unavailable(format!(
                "Coordinator registration returned {status}"
            )));
        }
        Err(ExecutableAgentRegistrationError::Unavailable(
            last_unavailable.unwrap_or_else(|| "Coordinator registration unavailable".into()),
        ))
    }
}

#[async_trait::async_trait]
impl ExecutableAgentRegistrar for HttpExecutableAgentRegistrar {
    async fn register(
        &self,
        registration: ExecutableAgentRegistration,
    ) -> Result<ExecutableAgentRegistrationOutcome, ExecutableAgentRegistrationError> {
        self.post(EXECUTABLE_AGENT_REGISTER_PATH, &registration)
            .await
    }

    async fn withdraw(
        &self,
        withdrawal: ExecutableAgentWithdrawal,
    ) -> Result<ExecutableAgentWithdrawalOutcome, ExecutableAgentRegistrationError> {
        self.post(EXECUTABLE_AGENT_WITHDRAW_PATH, &withdrawal).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::registration as test_registration;
    use crate::{ExecutableAgentCatalog, LocalExecutableAgentRegistrar};
    use awaken_runtime_contract::{AgentSnapshotFingerprint, CatalogFingerprint};
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    fn registration() -> ExecutableAgentRegistration {
        test_registration(7, "fp-a")
    }

    async fn server_for(
        registrar: Arc<dyn ExecutableAgentRegistrar>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let app = executable_agent_registration_router(registrar, "secret-token").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}"), task)
    }

    async fn server() -> (
        String,
        tokio::task::JoinHandle<()>,
        Arc<ExecutableAgentCatalog>,
    ) {
        let catalog = Arc::new(ExecutableAgentCatalog::new());
        let (url, task) = server_for(Arc::new(LocalExecutableAgentRegistrar::new(
            catalog.clone(),
        )))
        .await;
        (url, task, catalog)
    }

    struct CountingRegistrar {
        delegate: LocalExecutableAgentRegistrar,
        unavailable_before: usize,
        calls: AtomicUsize,
        withdrawal_calls: AtomicUsize,
    }

    impl CountingRegistrar {
        fn new(catalog: Arc<ExecutableAgentCatalog>, unavailable_before: usize) -> Self {
            Self {
                delegate: LocalExecutableAgentRegistrar::new(catalog),
                unavailable_before,
                calls: AtomicUsize::new(0),
                withdrawal_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl ExecutableAgentRegistrar for CountingRegistrar {
        async fn register(
            &self,
            registration: ExecutableAgentRegistration,
        ) -> Result<ExecutableAgentRegistrationOutcome, ExecutableAgentRegistrationError> {
            let prior_calls = self.calls.fetch_add(1, Ordering::SeqCst);
            if prior_calls < self.unavailable_before {
                return Err(ExecutableAgentRegistrationError::Unavailable(
                    "injected transient failure".into(),
                ));
            }
            self.delegate.register(registration).await
        }

        async fn withdraw(
            &self,
            withdrawal: ExecutableAgentWithdrawal,
        ) -> Result<ExecutableAgentWithdrawalOutcome, ExecutableAgentRegistrationError> {
            self.withdrawal_calls.fetch_add(1, Ordering::SeqCst);
            self.delegate.withdraw(withdrawal).await
        }
    }

    struct RecordingForbiddenAuthenticator(Mutex<Vec<(String, String, Option<String>)>>);

    impl ServiceRequestAuthenticator for RecordingForbiddenAuthenticator {
        fn authenticate(
            &self,
            _authorization: Option<&[u8]>,
            requirement: ServiceAuthorizationRequirement<'_>,
        ) -> Result<
            awaken_service_auth_contract::AuthenticatedService,
            awaken_service_auth_contract::ServiceAuthError,
        > {
            self.0.lock().unwrap().push((
                requirement.audience.to_owned(),
                requirement.permission.to_owned(),
                requirement.workspace_id.map(str::to_owned),
            ));
            Err(awaken_service_auth_contract::ServiceAuthError::Forbidden)
        }
    }

    #[tokio::test]
    async fn typed_authorization_fences_catalog_commands_before_mutation() {
        // Cause/effect graph: decoded command identity -> route-specific
        // requirement -> injected IAM decision -> registrar. Decision table:
        // R1 forbidden register carries coordinator/agent:publish/exact
        // Workspace -> 403 and zero registrar calls; R2 forbidden withdrawal
        // carries agent:withdraw/the same Workspace -> 403 and zero withdrawal
        // calls. Static-token 401 and successful mutation are covered by the
        // authenticated adapter table below; dependency-unavailable 503 is
        // covered by the Worker observation boundary table.
        let catalog = Arc::new(ExecutableAgentCatalog::new());
        let registrar = Arc::new(CountingRegistrar::new(catalog, 0));
        let authenticator = Arc::new(RecordingForbiddenAuthenticator(Mutex::new(Vec::new())));
        let app = executable_agent_registration_router_with_authenticator(
            registrar.clone(),
            authenticator.clone(),
        );
        let response = app
            .clone()
            .oneshot(
                Request::post(EXECUTABLE_AGENT_REGISTER_PATH)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&registration()).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "R1");
        assert_eq!(registrar.calls.load(Ordering::SeqCst), 0, "R1");

        let response = app
            .oneshot(
                Request::post(EXECUTABLE_AGENT_WITHDRAW_PATH)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&ExecutableAgentWithdrawal {
                            workspace_id: "workspace-a".into(),
                            agent_id: "agent-a".into(),
                            lifecycle_revision: 8,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "R2");
        assert_eq!(registrar.withdrawal_calls.load(Ordering::SeqCst), 0, "R2");
        assert_eq!(
            *authenticator.0.lock().unwrap(),
            vec![
                (
                    COORDINATOR_SERVICE_AUDIENCE.into(),
                    EXECUTABLE_AGENT_PUBLISH_PERMISSION.into(),
                    Some("workspace-a".into()),
                ),
                (
                    COORDINATOR_SERVICE_AUDIENCE.into(),
                    EXECUTABLE_AGENT_WITHDRAW_PERMISSION.into(),
                    Some("workspace-a".into()),
                ),
            ],
            "R1/R2"
        );
    }

    #[tokio::test]
    async fn network_adapter_retries_only_transient_idempotent_failures() {
        // Cause/effect decision table: R1 two transient Unavailable responses
        // within the three-attempt budget -> retry and publish exactly once;
        // R2 a semantic Conflict -> return immediately, preserve the prior
        // current registration, and do not consume another retry attempt.
        let catalog = Arc::new(ExecutableAgentCatalog::new());
        let transient = Arc::new(CountingRegistrar::new(catalog.clone(), 2));
        let (url, task) = server_for(transient.clone()).await;
        let client = HttpExecutableAgentRegistrar::new(&url, "secret-token").unwrap();
        assert_eq!(
            client.register(registration()).await.unwrap(),
            ExecutableAgentRegistrationOutcome::RegisteredCurrent,
            "R1"
        );
        assert_eq!(transient.calls.load(Ordering::SeqCst), 3, "R1");
        task.abort();

        let conflict_catalog = Arc::new(ExecutableAgentCatalog::new());
        let conflict = Arc::new(CountingRegistrar::new(conflict_catalog.clone(), 0));
        let (url, task) = server_for(conflict.clone()).await;
        let client = HttpExecutableAgentRegistrar::new(&url, "secret-token").unwrap();
        client.register(registration()).await.unwrap();
        let mut incompatible = registration();
        incompatible.snapshot.fingerprint = CatalogFingerprint("fp-b".into());
        incompatible.snapshot.resolved_spec.catalog_fingerprint = CatalogFingerprint("fp-b".into());
        incompatible.snapshot.metadata.fingerprint = AgentSnapshotFingerprint("fp-b".into());
        let conflict_result = client.register(incompatible).await;
        assert!(
            matches!(
                &conflict_result,
                Err(ExecutableAgentRegistrationError::Conflict(_))
            ),
            "R2: {conflict_result:?}"
        );
        assert_eq!(conflict.calls.load(Ordering::SeqCst), 2, "R2");
        assert_eq!(
            conflict_catalog
                .current("workspace-a", "agent-a")
                .unwrap()
                .snapshot
                .fingerprint
                .0,
            "fp-a",
            "R2"
        );
        task.abort();
    }

    #[test]
    fn network_boundary_requires_complete_private_endpoint_configuration() {
        // Causes: blank Coordinator URL or blank bearer token. Effect: reject
        // construction before any request can cross the private boundary.
        assert!(HttpExecutableAgentRegistrar::new("", "secret-token").is_err());
        assert!(HttpExecutableAgentRegistrar::new("http://", "secret-token").is_err());
        assert!(
            HttpExecutableAgentRegistrar::new("http://coordinator?mode=write", "secret-token")
                .is_err()
        );
        assert!(HttpExecutableAgentRegistrar::new("http://coordinator", " ").is_err());
        let catalog = Arc::new(ExecutableAgentCatalog::new());
        assert!(
            executable_agent_registration_router(
                Arc::new(LocalExecutableAgentRegistrar::new(catalog.clone())),
                "",
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn authenticated_network_adapter_preserves_registration_semantics() {
        // Decision table: R1 valid token + new command -> current; R2 identical
        // retry -> AlreadyRegistered; R3 bad token -> Invalid and no mutation;
        // R4 withdrawal -> current unavailable while exact history remains.
        let (url, task, catalog) = server().await;
        let client = HttpExecutableAgentRegistrar::new(&url, "secret-token").unwrap();
        assert_eq!(
            client.register(registration()).await.unwrap(),
            ExecutableAgentRegistrationOutcome::RegisteredCurrent,
            "R1"
        );
        assert_eq!(
            client.register(registration()).await.unwrap(),
            ExecutableAgentRegistrationOutcome::AlreadyRegistered,
            "R2"
        );
        let unauthorized = HttpExecutableAgentRegistrar::new(&url, "wrong-token").unwrap();
        assert!(
            matches!(
                unauthorized.register(registration()).await,
                Err(ExecutableAgentRegistrationError::Invalid(_))
            ),
            "R3"
        );
        assert_eq!(
            client
                .withdraw(ExecutableAgentWithdrawal {
                    workspace_id: "workspace-a".into(),
                    agent_id: "agent-a".into(),
                    lifecycle_revision: 8,
                })
                .await
                .unwrap(),
            ExecutableAgentWithdrawalOutcome::WithdrawnCurrent,
            "R4"
        );
        assert!(catalog.current("workspace-a", "agent-a").is_none(), "R4");
        assert!(catalog.exact("workspace-a", "fp-a").is_some(), "R4");
        task.abort();
    }
}
