//! Authenticated HTTP adapters for the executable Agent registrar port.

use std::sync::Arc;
use std::time::Duration;

use awaken_executable_agent_contract::{
    ExecutableAgentRegistrar, ExecutableAgentRegistration, ExecutableAgentRegistrationError,
    ExecutableAgentRegistrationOutcome, ExecutableAgentWithdrawal,
    ExecutableAgentWithdrawalOutcome,
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

const IDEMPOTENT_ATTEMPTS: usize = 3;
const RETRY_DELAY: Duration = Duration::from_millis(25);

#[derive(Clone)]
struct RegistrationHttpState {
    registrar: Arc<dyn ExecutableAgentRegistrar>,
    bearer_token: Arc<str>,
}

/// Coordinator's private registration surface. The token is mandatory because
/// this mutation changes executable availability even though the route is not a
/// public management API.
pub fn executable_agent_registration_router(
    registrar: Arc<dyn ExecutableAgentRegistrar>,
    bearer_token: impl Into<String>,
) -> Result<Router, String> {
    let bearer_token = bearer_token.into();
    if bearer_token.trim().is_empty() {
        return Err("executable Agent registration bearer token must not be empty".into());
    }
    let state = RegistrationHttpState {
        registrar,
        bearer_token: Arc::from(bearer_token),
    };
    Ok(Router::new()
        .route(EXECUTABLE_AGENT_REGISTER_PATH, post(register))
        .route(EXECUTABLE_AGENT_WITHDRAW_PATH, post(withdraw))
        .with_state(state))
}

async fn register(
    State(state): State<RegistrationHttpState>,
    headers: HeaderMap,
    Json(command): Json<ExecutableAgentRegistration>,
) -> impl IntoResponse {
    if !authorized(&headers, &state.bearer_token) {
        return StatusCode::UNAUTHORIZED.into_response();
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
    if !authorized(&headers, &state.bearer_token) {
        return StatusCode::UNAUTHORIZED.into_response();
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

fn authorized(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|actual| actual.as_bytes() == expected.as_bytes())
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
    bearer_token: String,
    client: reqwest::Client,
}

impl HttpExecutableAgentRegistrar {
    pub fn new(
        base_url: impl Into<String>,
        bearer_token: impl Into<String>,
    ) -> Result<Self, ExecutableAgentRegistrationError> {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        let bearer_token = bearer_token.into();
        if base_url.is_empty() || bearer_token.trim().is_empty() {
            return Err(ExecutableAgentRegistrationError::Invalid(
                "Coordinator URL and registration bearer token are required".into(),
            ));
        }
        Ok(Self {
            base_url,
            bearer_token,
            client: reqwest::Client::builder()
                .no_proxy()
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
            let response = self
                .client
                .post(format!("{}{}", self.base_url, path))
                .bearer_auth(&self.bearer_token)
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
    use crate::{ExecutableAgentCatalog, LocalExecutableAgentRegistrar};
    use awaken_runtime_contract::snapshot::AgentId;
    use awaken_runtime_contract::{
        AgentConfigRevisionRef, AgentPublicationVersion, AgentSnapshotFingerprint,
        AgentSnapshotMetadata, CatalogFingerprint, ExecutableAgentSnapshot,
    };
    use awaken_session_contract::AgentConfigView;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn registration() -> ExecutableAgentRegistration {
        let mut snapshot = ExecutableAgentSnapshot::builder("agent-a")
            .fingerprint("fp-a")
            .build();
        snapshot.metadata = AgentSnapshotMetadata {
            source: AgentConfigRevisionRef {
                agent_id: AgentId("agent-a".into()),
                revision: 7,
            },
            publication_version: AgentPublicationVersion("v7".into()),
            resolution: Default::default(),
            fingerprint: AgentSnapshotFingerprint("fp-a".into()),
        };
        ExecutableAgentRegistration {
            workspace_id: "workspace-a".into(),
            agent_id: "agent-a".into(),
            source_revision: 7,
            snapshot,
            agent_view: AgentConfigView::default(),
            declared_hand: None,
        }
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
    }

    impl CountingRegistrar {
        fn new(catalog: Arc<ExecutableAgentCatalog>, unavailable_before: usize) -> Self {
            Self {
                delegate: LocalExecutableAgentRegistrar::new(catalog),
                unavailable_before,
                calls: AtomicUsize::new(0),
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
            self.delegate.withdraw(withdrawal).await
        }
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
