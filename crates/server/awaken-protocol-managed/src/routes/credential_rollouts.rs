//! Private Control→Coordinator delivery of durable Vault rollout events.

use std::sync::Arc;

use awaken_credential_vault::repo::{
    ManagedCredentialAdoptionError, ManagedCredentialAdoptionProgress, ManagedCredentialRollout,
    ManagedCredentialRolloutTarget,
};
use awaken_service_auth_contract::{
    COORDINATOR_SERVICE_AUDIENCE, ServiceAuthError, ServiceAuthorizationRequirement,
    ServiceBearerTokenSource, ServiceRequestAuthenticator,
};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    routing::post,
};

use crate::ManagedState;

const PATH: &str = "/internal/v1/managed-credential-rollouts";

#[derive(Clone)]
struct RolloutHttpState {
    managed: Arc<ManagedState>,
    authenticator: Arc<dyn ServiceRequestAuthenticator>,
}

pub fn credential_rollout_router_with_authenticator(
    state: Arc<ManagedState>,
    authenticator: Arc<dyn ServiceRequestAuthenticator>,
) -> Router {
    Router::new()
        .route(PATH, post(apply_rollout))
        .with_state(RolloutHttpState {
            managed: state,
            authenticator,
        })
}

async fn apply_rollout(
    State(state): State<RolloutHttpState>,
    headers: HeaderMap,
    Json(event): Json<ManagedCredentialRollout>,
) -> Result<StatusCode, (StatusCode, String)> {
    let requirement = ServiceAuthorizationRequirement::new(
        COORDINATOR_SERVICE_AUDIENCE,
        "managed-credential.rollout",
    )
    .in_workspace(&event.workspace_id);
    state
        .authenticator
        .authenticate(
            headers
                .get(header::AUTHORIZATION)
                .map(|value| value.as_bytes()),
            requirement,
        )
        .map_err(|error| match error {
            ServiceAuthError::Unauthorized => (StatusCode::UNAUTHORIZED, error.to_string()),
            ServiceAuthError::Forbidden => (StatusCode::FORBIDDEN, error.to_string()),
            ServiceAuthError::Unavailable(_) => {
                (StatusCode::SERVICE_UNAVAILABLE, error.to_string())
            }
        })?;
    ManagedCredentialRolloutTarget::rollout(state.managed.as_ref(), &event)
        .await
        .map(|progress| match progress {
            ManagedCredentialAdoptionProgress::Converged => StatusCode::NO_CONTENT,
            ManagedCredentialAdoptionProgress::Pending => StatusCode::ACCEPTED,
        })
        .map_err(|error| match error {
            ManagedCredentialAdoptionError::InvalidEvent(_) => {
                (StatusCode::BAD_REQUEST, error.to_string())
            }
            ManagedCredentialAdoptionError::IdentityCollision => {
                (StatusCode::CONFLICT, error.to_string())
            }
            ManagedCredentialAdoptionError::Unauthorized => {
                (StatusCode::FORBIDDEN, error.to_string())
            }
            ManagedCredentialAdoptionError::Unavailable(_) => {
                (StatusCode::SERVICE_UNAVAILABLE, error.to_string())
            }
        })
}

/// Split-service adapter. Delivery is idempotent at both ends: Control retains
/// the durable outbox event until this call succeeds, while Coordinator derives
/// stable per-Session update identities from the event id.
pub struct HttpManagedCredentialRolloutTarget {
    endpoint: String,
    token_source: Arc<dyn ServiceBearerTokenSource>,
    client: reqwest::Client,
}

impl HttpManagedCredentialRolloutTarget {
    pub fn with_token_source(
        base_url: impl Into<String>,
        token_source: Arc<dyn ServiceBearerTokenSource>,
    ) -> Result<Self, String> {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        let parsed = reqwest::Url::parse(&base_url)
            .map_err(|error| format!("invalid Coordinator rollout URL: {error}"))?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(
                "Coordinator rollout URL must be an http(s) base URL without query or fragment"
                    .into(),
            );
        }
        awaken_service_auth_contract::resolve_service_bearer_token(token_source.as_ref())?;
        Ok(Self {
            endpoint: format!("{base_url}{PATH}"),
            token_source,
            client: reqwest::Client::builder()
                .no_proxy()
                .connect_timeout(std::time::Duration::from_secs(5))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .map_err(|error| error.to_string())?,
        })
    }
}

#[async_trait::async_trait]
impl ManagedCredentialRolloutTarget for HttpManagedCredentialRolloutTarget {
    async fn rollout(
        &self,
        event: &ManagedCredentialRollout,
    ) -> Result<ManagedCredentialAdoptionProgress, ManagedCredentialAdoptionError> {
        let token =
            awaken_service_auth_contract::resolve_service_bearer_token(self.token_source.as_ref())
                .map_err(ManagedCredentialAdoptionError::Unavailable)?;
        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(token.as_ref())
            .json(event)
            .send()
            .await
            .map_err(|error| {
                ManagedCredentialAdoptionError::Unavailable(format!(
                    "deliver Managed credential rollout: {error}"
                ))
            })?;
        if response.status() == StatusCode::NO_CONTENT {
            Ok(ManagedCredentialAdoptionProgress::Converged)
        } else if response.status() == StatusCode::ACCEPTED {
            Ok(ManagedCredentialAdoptionProgress::Pending)
        } else {
            Err(ManagedCredentialAdoptionError::Unavailable(format!(
                "Coordinator rollout service returned {}",
                response.status()
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use awaken_credential_contract::CredentialSourceId;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    use super::*;
    use crate::state::test_support::RehydrateFake;

    fn event() -> ManagedCredentialRollout {
        ManagedCredentialRollout {
            id: "rollout-1".into(),
            workspace_id: "ws".into(),
            vault_id: "vlt-1".into(),
            credential_id: "crd-1".into(),
            source_id: CredentialSourceId("cred-1".into()),
            source_version: 2,
            credential_revision: 2,
            operation: awaken_credential_vault::repo::ManagedCredentialOperation::Update,
        }
    }

    #[tokio::test]
    async fn private_rollout_requires_service_authentication_before_delivery() {
        let router = credential_rollout_router_with_authenticator(
            Arc::new(ManagedState::new(RehydrateFake::default())),
            awaken_service_auth_contract::static_token_authenticator("rollout-test").unwrap(),
        );
        let body = serde_json::to_vec(&event()).unwrap();
        let unauthorized = router
            .clone()
            .oneshot(
                Request::post(PATH)
                    .header("content-type", "application/json")
                    .body(Body::from(body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let accepted = router
            .oneshot(
                Request::post(PATH)
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer rollout-test")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::NO_CONTENT);
    }
}
