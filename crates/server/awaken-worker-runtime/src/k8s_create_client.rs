// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: This Worker transport projects the exact Registry identity
// @formal reason: into the existing signed control plane. It owns no provider
// @formal reason: ticket, Session state, Dispatch attempt or cleanup receipt.
//! Worker-side Kubernetes Create over the already authenticated control plane.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use awaken_run_ingress_contract::WorkerIdentity;
use awaken_sandbox_container::RuntimeError;
use awaken_sandbox_container::k8s::{K8sCreateRequest, K8sCreateResponse, K8sCreateTransport};
use awaken_worker_transport_security::WorkerUpstream;
use serde::Serialize;

const PATH: &str = "/v1/worker/k8s/create";

/// The exact Registry identity is installed once from the registration
/// receipt, after the provider has been assembled but before Worker Ready.
#[derive(Clone)]
pub struct WorkerK8sCreateClient {
    upstream: WorkerUpstream,
    identity: Arc<OnceLock<WorkerIdentity>>,
}

impl WorkerK8sCreateClient {
    #[must_use]
    pub fn new(upstream: WorkerUpstream) -> Self {
        Self {
            upstream,
            identity: Arc::new(OnceLock::new()),
        }
    }

    pub fn bind_identity(&self, identity: WorkerIdentity) -> Result<(), String> {
        if identity.worker_id != self.upstream.worker_id()
            || identity.generation == 0
            || identity.incarnation_id.trim().is_empty()
        {
            return Err("Kubernetes Create client requires the exact registered Worker".into());
        }
        if let Some(existing) = self.identity.get() {
            return if existing == &identity {
                Ok(())
            } else {
                Err("Kubernetes Create client cannot replace its Worker incarnation".into())
            };
        }
        self.identity
            .set(identity)
            .map_err(|_| "Kubernetes Create client identity was concurrently bound".into())
    }
}

#[derive(Serialize)]
struct CreateWire<'a> {
    identity: &'a WorkerIdentity,
    create: K8sCreateRequest,
}

#[async_trait]
impl K8sCreateTransport for WorkerK8sCreateClient {
    async fn create(&self, request: K8sCreateRequest) -> Result<K8sCreateResponse, RuntimeError> {
        let identity = self.identity.get().ok_or_else(|| {
            RuntimeError::Backend("Kubernetes Create attempted before Worker registration".into())
        })?;
        let upstream = self.upstream.clone().with_worker_identity(identity.clone());
        let builder = upstream
            .client()
            .post(format!("{}{}", upstream.base_url(), PATH));
        let builder = upstream
            .authorize_request("POST", PATH, builder)
            .map_err(RuntimeError::Backend)?;
        let response = builder
            .json(&CreateWire {
                identity,
                create: request,
            })
            .send()
            .await
            .map_err(|error| {
                RuntimeError::MayHaveCommitted(format!(
                    "Kubernetes Create broker response unavailable: {error}"
                ))
            })?;
        if !response.status().is_success() {
            return Err(RuntimeError::MayHaveCommitted(format!(
                "Kubernetes Create broker refused or retained uncertainty: {}",
                response.status()
            )));
        }
        response.json().await.map_err(|error| {
            RuntimeError::MayHaveCommitted(format!(
                "Kubernetes Create broker response could not be decoded: {error}"
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;
    use axum::{Json, Router};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn create_client_cannot_use_or_replace_an_unregistered_incarnation() {
        // C1 provider is assembled before registration; C2 exact Registry
        // receipt arrives; C3 a same or foreign receipt is retried. Effects:
        // E1 C1 cannot send Create, E2 C2 binds once, E3 same identity is a
        // no-op and another Worker/generation/boot cannot replace it.
        let client = WorkerK8sCreateClient::new(WorkerUpstream::new("http://127.0.0.1:1"));
        let request = K8sCreateRequest {
            path: "/api/v1/namespaces/ns/pods".into(),
            object: serde_json::json!({"metadata":{"name":"pod"}}),
        };
        assert!(
            matches!(client.create(request).await, Err(RuntimeError::Backend(_))),
            "E1"
        );
        let exact = WorkerIdentity::new("awaken-worker", "boot", 1);
        client.bind_identity(exact.clone()).unwrap();
        client.bind_identity(exact.clone()).unwrap();
        for foreign in [
            WorkerIdentity::new("other", "boot", 1),
            WorkerIdentity::new("awaken-worker", "other", 1),
            WorkerIdentity::new("awaken-worker", "boot", 2),
        ] {
            assert!(client.bind_identity(foreign).is_err(), "E3");
        }
    }

    #[tokio::test]
    async fn create_client_uses_the_existing_worker_request_authorizer() {
        // C1 registration binds the exact Worker identity; C2 the provider
        // sends one object with its API collection path. E1 the existing
        // WorkerUpstream authorizer supplies its bound identity header, E2
        // payload identity/path/object are unchanged, E3 the typed API result
        // returns without a second Worker credential or endpoint authority.
        let calls = Arc::new(AtomicUsize::new(0));
        let router = Router::new().route(
            PATH,
            post({
                let calls = calls.clone();
                move |headers: axum::http::HeaderMap, Json(body): Json<serde_json::Value>| {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        assert_eq!(
                            headers.get("x-awaken-worker-id").unwrap(),
                            "awaken-worker",
                            "E1"
                        );
                        assert_eq!(body["identity"]["incarnation_id"], "boot", "E2");
                        assert_eq!(body["create"]["path"], "/api/v1/namespaces/ns/pods", "E2");
                        assert_eq!(body["create"]["object"]["metadata"]["name"], "pod", "E2");
                        Json(serde_json::json!({
                            "status":201,
                            "object":{"apiVersion":"v1","kind":"Pod","metadata":{
                                "name":"pod","namespace":"ns","uid":"pod-uid"
                            }}
                        }))
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = WorkerK8sCreateClient::new(WorkerUpstream::new(format!("http://{address}")));
        client
            .bind_identity(WorkerIdentity::new("awaken-worker", "boot", 1))
            .unwrap();
        let response = client
            .create(K8sCreateRequest {
                path: "/api/v1/namespaces/ns/pods".into(),
                object: serde_json::json!({"metadata":{"name":"pod"}}),
            })
            .await
            .unwrap();
        assert_eq!(response.status, 201, "E3");
        assert_eq!(response.object["metadata"]["uid"], "pod-uid", "E3");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one signed request");
        server.abort();
    }
}
