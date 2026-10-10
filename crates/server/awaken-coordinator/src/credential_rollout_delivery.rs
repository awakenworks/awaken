// @formal entrypoint credential.managed_rollout_abstract_ack_requires_every_target_adoption
//! Private Control→Coordinator delivery of durable Vault rollout events: the
//! Coordinator's service-authenticated route and the Control-side HTTP client.
//! Both ends of this internal channel live here so its path is defined once.

use std::sync::Arc;

use awaken_credential_contract::ManagedCredentialRollout;
use awaken_credential_contract::{
    ManagedCredentialAdoptionError, ManagedCredentialAdoptionProgress,
    ManagedCredentialAdoptionUnavailable, ManagedCredentialRolloutTarget,
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

const PATH: &str = "/internal/v1/managed-credential-rollouts";

#[derive(Clone)]
struct RolloutHttpState {
    target: Arc<dyn ManagedCredentialRolloutTarget>,
    authenticator: Arc<dyn ServiceRequestAuthenticator>,
}

/// The private route the Control rollout reconciler delivers to; `target` is
/// this Coordinator's Session adoption of a rollout.
pub fn credential_rollout_router_with_authenticator(
    target: Arc<dyn ManagedCredentialRolloutTarget>,
    authenticator: Arc<dyn ServiceRequestAuthenticator>,
) -> Router {
    Router::new()
        .route(PATH, post(apply_rollout))
        .with_state(RolloutHttpState {
            target,
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
    state
        .target
        .rollout(&event)
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
        let unavailable = |cause, detail: &dyn std::fmt::Display| {
            tracing::warn!(rollout_id = %event.id, %detail,
                "Managed credential rollout delivery failed");
            ManagedCredentialAdoptionError::Unavailable(cause)
        };
        let token =
            awaken_service_auth_contract::resolve_service_bearer_token(self.token_source.as_ref())
                .map_err(|error| {
                    unavailable(ManagedCredentialAdoptionUnavailable::ServiceToken, &error)
                })?;
        let body = serde_json::to_string(event)
            .map_err(|error| unavailable(ManagedCredentialAdoptionUnavailable::Encoding, &error))?;
        let response = self
            .client
            .send(
                &awaken_outbound_http::JsonRequest::post(&self.endpoint)
                    .bearer(token.as_ref())
                    .json_body(body),
            )
            .await
            .map_err(|error| {
                unavailable(ManagedCredentialAdoptionUnavailable::Transport, &error)
            })?;
        if response.status() == StatusCode::NO_CONTENT.as_u16() {
            Ok(ManagedCredentialAdoptionProgress::Converged)
        } else if response.status() == StatusCode::ACCEPTED.as_u16() {
            Ok(ManagedCredentialAdoptionProgress::Pending)
        } else {
            Err(ManagedCredentialAdoptionError::Unavailable(
                ManagedCredentialAdoptionUnavailable::Status(response.status()),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use awaken_credential_contract::CredentialSourceId;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    use super::*;

    /// An adopting Coordinator whose Sessions have all adopted the event.
    struct ConvergedTarget;

    #[async_trait::async_trait]
    impl ManagedCredentialRolloutTarget for ConvergedTarget {
        async fn rollout(
            &self,
            _event: &ManagedCredentialRollout,
        ) -> Result<ManagedCredentialAdoptionProgress, ManagedCredentialAdoptionError> {
            Ok(ManagedCredentialAdoptionProgress::Converged)
        }
    }

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
            Arc::new(ConvergedTarget),
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
        // Rules: 204 -> Converged; 202 -> Pending; 200/401/403/409/503 ->
        // Unavailable(Status(code)); closed listener -> Unavailable(Transport).
        // No response body (possibly secret) becomes a diagnostic. The Control
        // reconciler separately proves error retains the durable event,
        // including for nominally successful non-204 codes.
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
                _ => {
                    let error = result.unwrap_err();
                    assert!(!error.to_string().contains("secret-response-canary"));
                    // The cause is the closed status fact, never the body.
                    assert_eq!(
                        error,
                        ManagedCredentialAdoptionError::Unavailable(
                            ManagedCredentialAdoptionUnavailable::Status(status)
                        ),
                        "C2/{status}"
                    );
                }
            }
            server.abort();
            let _ = server.await;
            assert_eq!(
                target.rollout(&event()).await.unwrap_err(),
                ManagedCredentialAdoptionError::Unavailable(
                    ManagedCredentialAdoptionUnavailable::Transport
                ),
                "C3/{status}"
            );
        }
    }

    /// A token source that answers once (at construction) and then fails, as
    /// a rotated-away credential file does.
    struct ExpiringTokenSource(std::sync::atomic::AtomicBool);

    impl ServiceBearerTokenSource for ExpiringTokenSource {
        fn current_token(&self) -> Result<Arc<str>, String> {
            if self.0.swap(true, std::sync::atomic::Ordering::SeqCst) {
                Err("secret-token-path-canary is unreadable".into())
            } else {
                Ok(Arc::from("rollout-test"))
            }
        }
    }

    #[tokio::test]
    async fn an_unresolvable_service_token_is_the_service_token_cause() {
        // Cause C4: the delivery credential cannot be read at send time.
        // Effect: Unavailable(ServiceToken) before any request leaves, and the
        // source's own text never reaches the error.
        let target = HttpManagedCredentialRolloutTarget::with_token_source(
            "http://127.0.0.1:9",
            Arc::new(ExpiringTokenSource(std::sync::atomic::AtomicBool::new(
                false,
            ))),
        )
        .unwrap();
        let error = target.rollout(&event()).await.unwrap_err();
        assert_eq!(
            error,
            ManagedCredentialAdoptionError::Unavailable(
                ManagedCredentialAdoptionUnavailable::ServiceToken
            )
        );
        assert!(!error.to_string().contains("canary"));
    }
}
