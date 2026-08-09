//! Control-to-Coordinator adapters for the one Environment create application.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awaken_admin_assistant::{AdminEnvironmentNetworking, EnvironmentAuthor, EnvironmentDraft};
use awaken_protocol_managed::EnvironmentApplication;
use awaken_session_contract::env_registry::{
    CreateEnvironmentCommand, CreateEnvironmentError, EnvironmentConfig, EnvironmentNetworking,
    EnvironmentPackages, EnvironmentPackagesKind,
};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::post;
use axum::{Json, Router, response::IntoResponse};
use serde::{Deserialize, Serialize};

pub const CREATE_ENVIRONMENT_PATH: &str = "/internal/v1/environments/create";
const ATTEMPTS: usize = 3;
const RETRY_DELAY: Duration = Duration::from_millis(25);

#[derive(Clone, Serialize, Deserialize)]
struct CreateCommand {
    command_id: String,
    name: String,
    config: EnvironmentDraft,
}

fn canonical_config(draft: EnvironmentDraft) -> EnvironmentConfig {
    match draft {
        EnvironmentDraft::SelfHosted => EnvironmentConfig::SelfHosted,
        EnvironmentDraft::Cloud {
            networking,
            packages,
        } => EnvironmentConfig::Cloud {
            networking: match networking {
                AdminEnvironmentNetworking::Unrestricted => EnvironmentNetworking::Unrestricted,
                AdminEnvironmentNetworking::Limited {
                    allowed_hosts,
                    allow_mcp_servers,
                    allow_package_managers,
                } => EnvironmentNetworking::Limited {
                    allowed_hosts,
                    allow_mcp_servers,
                    allow_package_managers,
                },
            },
            packages: EnvironmentPackages {
                kind: EnvironmentPackagesKind::Packages,
                apt: packages.apt,
                cargo: packages.cargo,
                gem: packages.gem,
                go: packages.go,
                npm: packages.npm,
                pip: packages.pip,
            },
        },
    }
}

async fn create(
    application: &EnvironmentApplication,
    command: CreateCommand,
) -> Result<String, CreateEnvironmentError> {
    application
        .create(CreateEnvironmentCommand {
            command_id: format!("control:{}", command.command_id),
            name: command.name,
            description: String::new(),
            metadata: Default::default(),
            scope: None,
            config: canonical_config(command.config),
        })
        .await
        .map(|item| item.id)
}

pub struct LocalEnvironmentAuthor {
    application: Arc<EnvironmentApplication>,
}

impl LocalEnvironmentAuthor {
    #[must_use]
    pub fn new(application: Arc<EnvironmentApplication>) -> Self {
        Self { application }
    }
}

#[async_trait]
impl EnvironmentAuthor for LocalEnvironmentAuthor {
    async fn create(
        &self,
        command_id: &str,
        name: &str,
        config: EnvironmentDraft,
    ) -> Result<String, String> {
        create(
            &self.application,
            CreateCommand {
                command_id: command_id.to_owned(),
                name: name.to_owned(),
                config,
            },
        )
        .await
        .map_err(|error| error.to_string())
    }
}

#[derive(Clone)]
struct BoundaryState {
    application: Arc<EnvironmentApplication>,
    bearer_token: Arc<str>,
}

pub fn router(
    application: Arc<EnvironmentApplication>,
    bearer_token: impl Into<String>,
) -> Result<Router, String> {
    let bearer_token = bearer_token.into();
    if bearer_token.trim().is_empty() {
        return Err("Environment service bearer token must not be empty".into());
    }
    Ok(Router::new()
        .route(CREATE_ENVIRONMENT_PATH, post(handle_create))
        .with_state(BoundaryState {
            application,
            bearer_token: Arc::from(bearer_token),
        }))
}

async fn handle_create(
    State(state): State<BoundaryState>,
    headers: HeaderMap,
    Json(command): Json<CreateCommand>,
) -> axum::response::Response {
    if !awaken_executable_agent_contract::service_bearer_token_matches(
        headers
            .get(header::AUTHORIZATION)
            .map(|value| value.as_bytes()),
        &state.bearer_token,
    ) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match create(&state.application, command).await {
        Ok(id) => (StatusCode::OK, Json(Ok::<_, String>(id))).into_response(),
        Err(CreateEnvironmentError::IdempotencyConflict) => (
            StatusCode::CONFLICT,
            Json(Err::<String, _>(
                CreateEnvironmentError::IdempotencyConflict.to_string(),
            )),
        )
            .into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(Err::<String, _>(error.to_string())),
        )
            .into_response(),
    }
}

#[derive(Clone)]
pub struct HttpEnvironmentAuthor {
    endpoint: String,
    bearer_token: String,
    client: reqwest::Client,
}

impl HttpEnvironmentAuthor {
    pub fn new(
        base_url: impl Into<String>,
        bearer_token: impl Into<String>,
    ) -> Result<Self, String> {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        let bearer_token = bearer_token.into();
        reqwest::Url::parse(&base_url)
            .map_err(|error| format!("invalid Coordinator Environment URL: {error}"))?;
        if bearer_token.trim().is_empty() {
            return Err("Coordinator Environment bearer token is required".into());
        }
        Ok(Self {
            endpoint: format!("{base_url}{CREATE_ENVIRONMENT_PATH}"),
            bearer_token,
            client: reqwest::Client::new(),
        })
    }

    async fn send(&self, command: &CreateCommand) -> Result<String, String> {
        let mut last_error = "Coordinator Environment service unavailable".to_owned();
        for attempt in 0..ATTEMPTS {
            match self
                .client
                .post(&self.endpoint)
                .bearer_auth(&self.bearer_token)
                .json(command)
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => {
                    return response
                        .json::<Result<String, String>>()
                        .await
                        .map_err(|error| format!("decode Environment response: {error}"))?;
                }
                Ok(response) if response.status().is_client_error() => {
                    return Err(response
                        .json::<Result<String, String>>()
                        .await
                        .ok()
                        .and_then(Result::err)
                        .unwrap_or_else(|| "Environment command was rejected".into()));
                }
                Ok(response) => {
                    last_error = format!("Environment service returned {}", response.status())
                }
                Err(error) => last_error = format!("call Environment service: {error}"),
            }
            if attempt + 1 < ATTEMPTS {
                tokio::time::sleep(RETRY_DELAY).await;
            }
        }
        Err(last_error)
    }
}

#[async_trait]
impl EnvironmentAuthor for HttpEnvironmentAuthor {
    async fn create(
        &self,
        command_id: &str,
        name: &str,
        config: EnvironmentDraft,
    ) -> Result<String, String> {
        self.send(&CreateCommand {
            command_id: command_id.to_owned(),
            name: name.to_owned(),
            config,
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    #[tokio::test]
    async fn private_environment_command_decision_table() {
        // Causes/effects: R1 valid token + new command creates; R2 exact replay
        // returns the same id; R3 wrong token is rejected before mutation; R4
        // same command id with another payload conflicts.
        let state = awaken_protocol_managed::EnvironmentState::new();
        let app = router(state.application(), "secret").unwrap();
        let call = |token: &'static str, name: &'static str| {
            let body = serde_json::to_vec(&CreateCommand {
                command_id: "call-1".into(),
                name: name.into(),
                config: EnvironmentDraft::SelfHosted,
            })
            .unwrap();
            Request::post(CREATE_ENVIRONMENT_PATH)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(body))
                .unwrap()
        };
        assert_eq!(
            app.clone()
                .oneshot(call("secret", "stable"))
                .await
                .unwrap()
                .status(),
            StatusCode::OK,
            "R1"
        );
        assert_eq!(
            app.clone()
                .oneshot(call("secret", "stable"))
                .await
                .unwrap()
                .status(),
            StatusCode::OK,
            "R2"
        );
        assert_eq!(
            app.clone()
                .oneshot(call("wrong", "stable"))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED,
            "R3"
        );
        assert_eq!(
            app.oneshot(call("secret", "different"))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT,
            "R4"
        );
    }
}
