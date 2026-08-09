//! Authenticated HTTP adapters for executable Environment registration.

use std::sync::Arc;
use std::time::Duration;

use awaken_executable_environment_contract::{
    ExecutableEnvironmentRegistrar, ExecutableEnvironmentRegistration,
    ExecutableEnvironmentRegistrationError, ExecutableEnvironmentRegistrationOutcome,
    ExecutableEnvironmentWithdrawal, ExecutableEnvironmentWithdrawalOutcome,
};
use awaken_service_auth_contract::service_bearer_token_matches;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::post;
use serde::Serialize;
use serde::de::DeserializeOwned;

pub const EXECUTABLE_ENVIRONMENT_REGISTER_PATH: &str =
    "/internal/v1/executable-environments/register";
pub const EXECUTABLE_ENVIRONMENT_WITHDRAW_PATH: &str =
    "/internal/v1/executable-environments/withdraw";
const IDEMPOTENT_ATTEMPTS: usize = 3;
const RETRY_DELAY: Duration = Duration::from_millis(25);

#[derive(Clone)]
struct RegistrationHttpState {
    registrar: Arc<dyn ExecutableEnvironmentRegistrar>,
    bearer_token: Arc<str>,
}

pub fn executable_environment_registration_router(
    registrar: Arc<dyn ExecutableEnvironmentRegistrar>,
    bearer_token: impl Into<String>,
) -> Result<Router, String> {
    let bearer_token = bearer_token.into();
    if bearer_token.trim().is_empty() {
        return Err("executable Environment registration bearer token must not be empty".into());
    }
    Ok(Router::new()
        .route(EXECUTABLE_ENVIRONMENT_REGISTER_PATH, post(register))
        .route(EXECUTABLE_ENVIRONMENT_WITHDRAW_PATH, post(withdraw))
        .with_state(RegistrationHttpState {
            registrar,
            bearer_token: Arc::from(bearer_token),
        }))
}

fn authorized(headers: &HeaderMap, expected: &str) -> bool {
    service_bearer_token_matches(
        headers
            .get(header::AUTHORIZATION)
            .map(|value| value.as_bytes()),
        expected,
    )
}

async fn register(
    State(state): State<RegistrationHttpState>,
    headers: HeaderMap,
    Json(command): Json<ExecutableEnvironmentRegistration>,
) -> impl IntoResponse {
    if !authorized(&headers, &state.bearer_token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match state.registrar.register(command).await {
        Ok(outcome) => (
            StatusCode::OK,
            Json(Some(Ok::<_, ExecutableEnvironmentRegistrationError>(
                outcome,
            ))),
        )
            .into_response(),
        Err(error) => error_response(error),
    }
}

async fn withdraw(
    State(state): State<RegistrationHttpState>,
    headers: HeaderMap,
    Json(command): Json<ExecutableEnvironmentWithdrawal>,
) -> impl IntoResponse {
    if !authorized(&headers, &state.bearer_token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match state.registrar.withdraw(command).await {
        Ok(outcome) => (
            StatusCode::OK,
            Json(Some(Ok::<_, ExecutableEnvironmentRegistrationError>(
                outcome,
            ))),
        )
            .into_response(),
        Err(error) => error_response(error),
    }
}

fn error_response(error: ExecutableEnvironmentRegistrationError) -> axum::response::Response {
    let status = match error {
        ExecutableEnvironmentRegistrationError::Invalid(_) => StatusCode::BAD_REQUEST,
        ExecutableEnvironmentRegistrationError::Conflict(_) => StatusCode::CONFLICT,
        ExecutableEnvironmentRegistrationError::Unavailable(_)
        | ExecutableEnvironmentRegistrationError::Storage(_) => StatusCode::SERVICE_UNAVAILABLE,
    };
    (status, Json(Some(Err::<serde_json::Value, _>(error)))).into_response()
}

#[derive(Clone)]
pub struct HttpExecutableEnvironmentRegistrar {
    base_url: String,
    bearer_token: String,
    client: reqwest::Client,
}

impl HttpExecutableEnvironmentRegistrar {
    pub fn new(
        base_url: impl Into<String>,
        bearer_token: impl Into<String>,
    ) -> Result<Self, ExecutableEnvironmentRegistrationError> {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        let bearer_token = bearer_token.into();
        if base_url.is_empty() || bearer_token.trim().is_empty() {
            return Err(ExecutableEnvironmentRegistrationError::Invalid(
                "Coordinator URL and registration bearer token are required".into(),
            ));
        }
        let parsed = reqwest::Url::parse(&base_url).map_err(|error| {
            ExecutableEnvironmentRegistrationError::Invalid(format!(
                "invalid Coordinator registration URL: {error}"
            ))
        })?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(ExecutableEnvironmentRegistrationError::Invalid(
                "Coordinator registration URL must be an http(s) base URL without query or fragment"
                    .into(),
            ));
        }
        Ok(Self {
            base_url,
            bearer_token,
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .map_err(|error| {
                    ExecutableEnvironmentRegistrationError::Unavailable(error.to_string())
                })?,
        })
    }

    async fn post<T: Serialize + ?Sized, O: DeserializeOwned>(
        &self,
        path: &str,
        command: &T,
    ) -> Result<O, ExecutableEnvironmentRegistrationError> {
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
                return Err(ExecutableEnvironmentRegistrationError::Invalid(
                    "Coordinator rejected registration credentials".into(),
                ));
            }
            let status = response.status();
            let decoded = response
                .json::<Option<Result<O, ExecutableEnvironmentRegistrationError>>>()
                .await;
            if status.is_success() {
                return decoded
                    .map_err(|error| {
                        ExecutableEnvironmentRegistrationError::Unavailable(format!(
                            "Coordinator registration response decode failed: {error}"
                        ))
                    })?
                    .ok_or_else(|| {
                        ExecutableEnvironmentRegistrationError::Unavailable(
                            "Coordinator returned an empty registration response".into(),
                        )
                    })?;
            }
            if let Ok(Some(Err(error))) = decoded {
                if matches!(
                    error,
                    ExecutableEnvironmentRegistrationError::Unavailable(_)
                        | ExecutableEnvironmentRegistrationError::Storage(_)
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
            return Err(ExecutableEnvironmentRegistrationError::Unavailable(
                format!("Coordinator registration returned {status}"),
            ));
        }
        Err(ExecutableEnvironmentRegistrationError::Unavailable(
            last_unavailable.unwrap_or_else(|| "Coordinator registration unavailable".into()),
        ))
    }
}

#[async_trait::async_trait]
impl ExecutableEnvironmentRegistrar for HttpExecutableEnvironmentRegistrar {
    async fn register(
        &self,
        registration: ExecutableEnvironmentRegistration,
    ) -> Result<ExecutableEnvironmentRegistrationOutcome, ExecutableEnvironmentRegistrationError>
    {
        self.post(EXECUTABLE_ENVIRONMENT_REGISTER_PATH, &registration)
            .await
    }

    async fn withdraw(
        &self,
        withdrawal: ExecutableEnvironmentWithdrawal,
    ) -> Result<ExecutableEnvironmentWithdrawalOutcome, ExecutableEnvironmentRegistrationError>
    {
        self.post(EXECUTABLE_ENVIRONMENT_WITHDRAW_PATH, &withdrawal)
            .await
    }
}
