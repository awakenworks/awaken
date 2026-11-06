// @formal surface obligation=sandbox.admission_never_weakens_isolation
// @formal reason: The one provider client wrapper routes Create to an injected
// @formal reason: broker while preserving typed realization verification. Its
// @formal reason: tests qualify routing only, not the unfinished closure proof.
//! One Kubernetes Create transport below the existing typed `Api<K>` callers.
//!
//! Worker deployments inject the signed Coordinator port. The wrapper sends
//! every control-client POST through that port and leaves typed 409/object
//! verification in the canonical realization owner. It never bypasses the
//! broker for an unrecognized POST.

use std::sync::Arc;

use async_trait::async_trait;
use http::{Method, Response, StatusCode, header};
use kube::Client;
use kube::client::Body;

use awaken_container_contract::RequestJoin;
use awaken_container_contract::RuntimeError;
use awaken_provisioning_contract::SandboxEffectFence;

use awaken_container_contract::{K8sCreateRequest, K8sCreateResponse};

#[async_trait]
pub trait K8sCreateTransport: Send + Sync {
    async fn create(&self, request: K8sCreateRequest) -> Result<K8sCreateResponse, RuntimeError>;
    async fn seal_predecessor(
        &self,
        scope: &str,
        predecessor: &SandboxEffectFence,
        expected_pod_name: &str,
    ) -> Result<RequestJoin, RuntimeError>;
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The one POST-intercepting kube client shared by Session realization,
/// package image preparation and node-image prewarm. Unknown POST routes are
/// sent to the broker for refusal; they never fall through to the Worker's
/// direct ServiceAccount client.
pub fn brokered_client(direct: Client, broker: Arc<dyn K8sCreateTransport>) -> Client {
    let namespace = direct.default_namespace().to_owned();
    let service = tower::service_fn(move |request: http::Request<Body>| {
        let direct = direct.clone();
        let broker = broker.clone();
        async move {
            if request.method() != Method::POST {
                return direct
                    .send(request)
                    .await
                    .map_err(|error| Box::new(error) as BoxError);
            }
            if request.uri().query().is_some_and(|query| !query.is_empty()) {
                return Err(Box::<dyn std::error::Error + Send + Sync>::from(
                    "brokered Kubernetes Create rejects a query-bearing POST",
                ));
            }
            let path = request.uri().path().to_owned();
            let bytes = request
                .into_body()
                .collect_bytes()
                .await
                .map_err(|error| Box::new(error) as BoxError)?;
            let object =
                serde_json::from_slice(&bytes).map_err(|error| Box::new(error) as BoxError)?;
            let response = broker
                .create(K8sCreateRequest { path, object })
                .await
                .map_err(|error| Box::new(error) as BoxError)?;
            let status = StatusCode::from_u16(response.status)
                .map_err(|error| Box::new(error) as BoxError)?;
            let body = serde_json::to_vec(&response.object)
                .map_err(|error| Box::new(error) as BoxError)?;
            Ok(Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .expect("validated HTTP status and header"))
        }
    });
    Client::new(service, namespace)
}
