// @formal entrypoint credential.managed_rollout_abstract_ack_requires_every_target_adoption
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
    client: awaken_outbound_http::InternalHttpClient,
}

impl HttpManagedCredentialRolloutTarget {
    pub fn with_token_source(
        base_url: impl Into<String>,
        token_source: Arc<dyn ServiceBearerTokenSource>,
    ) -> Result<Self, String> {
        let base_url = awaken_outbound_http::normalize_http_base_url(&base_url.into()).map_err(
            |rejected| match rejected {
                awaken_outbound_http::BaseUrlRejected::Malformed => {
                    format!("invalid Coordinator rollout URL: {rejected}")
                }
                _ => "Coordinator rollout URL must be an http(s) base URL without query or \
                      fragment"
                    .to_owned(),
            },
        )?;
        awaken_service_auth_contract::resolve_service_bearer_token(token_source.as_ref())?;
        Ok(Self {
            endpoint: format!("{base_url}{PATH}"),
            token_source,
            client: awaken_outbound_http::InternalHttpClient::new(std::time::Duration::from_secs(
                30,
            ))
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
        let body = serde_json::to_string(event).map_err(|error| {
            ManagedCredentialAdoptionError::Unavailable(format!(
                "encode Managed credential rollout: {error}"
            ))
        })?;
        let response = self
            .client
            .send(
                &awaken_outbound_http::JsonRequest::post(&self.endpoint)
                    .bearer(token.as_ref())
                    .json_body(body),
            )
            .await
            .map_err(|error| {
                ManagedCredentialAdoptionError::Unavailable(format!(
                    "deliver Managed credential rollout: {error}"
                ))
            })?;
        if response.status() == StatusCode::NO_CONTENT.as_u16() {
            Ok(ManagedCredentialAdoptionProgress::Converged)
        } else if response.status() == StatusCode::ACCEPTED.as_u16() {
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
            Arc::new(ManagedState::new(RehydrateFake::default()).await),
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
    #[tokio::test]
    async fn http_rollout_acknowledges_only_exact_convergence_status() {
        // Causes: C1 authenticated delivery; C2 HTTP status; C3 transport dies.
        // Rules: 204 -> Converged; 202 -> Pending; 200/401/403/409/503 -> error;
        // closed listener -> error. No response body (possibly secret) becomes
        // a diagnostic. The Control reconciler separately proves error retains
        // the durable event, including for nominally successful non-204 codes.
        for status in [204, 202, 200, 401, 403, 409, 503] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let router = Router::new().route(PATH, post(move |headers: HeaderMap, Json(received): Json<ManagedCredentialRollout>| async move {
                assert_eq!(headers[header::AUTHORIZATION], "Bearer rollout-test");
                assert_eq!(received, event());
                (StatusCode::from_u16(status).unwrap(), "secret-response-canary")
            }));
            let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            let target = HttpManagedCredentialRolloutTarget::with_token_source(
                format!("http://{address}"),
                awaken_service_auth_contract::static_token_source("rollout-test").unwrap(),
            )
            .unwrap();
            let result = target.rollout(&event()).await;
            match status {
                204 => assert_eq!(
                    result.unwrap(),
                    ManagedCredentialAdoptionProgress::Converged
                ),
                202 => assert_eq!(result.unwrap(), ManagedCredentialAdoptionProgress::Pending),
                _ => assert!(
                    !result
                        .unwrap_err()
                        .to_string()
                        .contains("secret-response-canary")
                ),
            }
            server.abort();
            let _ = server.await;
            assert!(target.rollout(&event()).await.is_err(), "C3/{status}");
        }
    }
}
