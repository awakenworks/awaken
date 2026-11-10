// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: This Worker transport projects the exact Registry identity
// @formal reason: into the existing signed control plane. It owns no provider
// @formal reason: ticket, Session state, Dispatch attempt or cleanup receipt.
//! Worker-side Kubernetes Create over the already authenticated control plane.

use awaken_container_contract::{
    CreateReplyProfile, K8S_CREATE_CAPTURE_HEADER, K8S_CREATE_WIRE_VERSION, RequestJoin,
};
use awaken_container_contract::{K8sCreateRequest, K8sCreateResponse};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use awaken_container_contract::RuntimeError;
use awaken_provisioning_contract::SandboxEffectFence;
use awaken_worker_contract::WorkerIdentity;
use awaken_worker_fleet_kubernetes::K8sCreateTransport;
use awaken_worker_transport_security::WorkerUpstream;
use serde::Serialize;
use serde::de::DeserializeOwned;

const PATH: &str = "/v1/worker/k8s/create";
const SEAL_PATH: &str = "/v1/worker/k8s/create/seal";

/// The exact Registry identity is installed once from the registration
/// receipt, after the provider has been assembled but before Worker Ready.
#[derive(Clone)]
pub struct WorkerK8sCreateClient {
    upstream: WorkerUpstream,
    identity: Arc<OnceLock<WorkerIdentity>>,
    request_budget: std::time::Duration,
}

impl WorkerK8sCreateClient {
    #[must_use]
    pub fn new(upstream: WorkerUpstream) -> Self {
        Self {
            upstream,
            identity: Arc::new(OnceLock::new()),
            request_budget: std::time::Duration::from_secs(30),
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

    fn identity(&self) -> Result<&WorkerIdentity, RuntimeError> {
        self.identity.get().ok_or_else(|| {
            RuntimeError::Backend(
                "Kubernetes Create control used before Worker registration".into(),
            )
        })
    }

    #[tracing::instrument(name = "worker.kubernetes.control", skip_all, fields(rpc.path = path, http.status_code = tracing::field::Empty))]
    async fn post<T: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &T,
    ) -> Result<R, RuntimeError> {
        let upstream = self
            .upstream
            .clone()
            .with_worker_identity(self.identity()?.clone());
        // Serialize once: retries must join the exact durable ticket, including
        // the original attempt stamp. The broker remains the effect authority.
        let body =
            serde_json::to_vec(body).map_err(|error| RuntimeError::Backend(error.to_string()))?;
        let deadline = awaken_time::mono::MonoDeadline::after(self.request_budget);
        let mut delay = std::time::Duration::from_millis(10);
        loop {
            let result = tokio::time::timeout_at(deadline.instant(), async {
                let mut builder =
                    upstream
                        .client()
                        .post(format!("{}{}", upstream.base_url(), path));
                if let Some(traceparent) = awaken_observability::current_traceparent() {
                    builder = builder.header("traceparent", traceparent);
                }
                let builder = upstream
                    .authorize_request("POST", path, builder)
                    .map_err(|error| (RuntimeError::Backend(error), false))?;
                let response = builder
                    .header("content-type", "application/json")
                    .body(body.clone())
                    .send()
                    .await
                    .map_err(|error| {
                        (
                            RuntimeError::MayHaveCommitted(format!(
                                "Kubernetes broker response unavailable: {error}"
                            )),
                            !error.is_builder(),
                        )
                    })?;
                let status = response.status();
                tracing::Span::current().record("http.status_code", status.as_u16());
                if !status.is_success() {
                    return Err((
                        RuntimeError::MayHaveCommitted(format!(
                            "Kubernetes broker refused or retained uncertainty: {status}"
                        )),
                        status.is_server_error() || status.as_u16() == 429,
                    ));
                }
                let profile = response.headers().get(K8S_CREATE_CAPTURE_HEADER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(CreateReplyProfile::from_header)
                    .filter(|profile| match path {
                        PATH => matches!(profile, CreateReplyProfile::FreshCreate | CreateReplyProfile::LegacyRetainedCreate),
                        SEAL_PATH => *profile == CreateReplyProfile::WholeJoin,
                        _ => false,
                    })
                    .ok_or_else(|| (RuntimeError::MayHaveCommitted("Create control response lacks its exact current capture profile".into()), false))?;
                if let Some(limit) = profile.body_limit() {
                    let response: axum::http::Response<_> = response.into();
                    let body = axum::body::Body::new(response.into_body());
                    let bytes = axum::body::to_bytes(body, limit).await.map_err(|_| {
                        (RuntimeError::MayHaveCommitted("Create control response is incomplete or exceeds its full body policy".into()), true)
                    })?;
                    serde_json::from_slice::<R>(&bytes).map_err(|_| {
                        (RuntimeError::MayHaveCommitted("Create control response could not be decoded".into()), true)
                    })
                } else {
                    // Only an explicitly retained historical Create reply has
                    // the old whole-object read scope. Never infer it from a
                    // missing marker or downgrade the current request wire.
                    response.json::<R>().await.map_err(|error| {
                        (RuntimeError::MayHaveCommitted(format!("Kubernetes broker response could not be decoded: {error}")), true)
                    })
                }
            })
            .await;
            match result {
                Ok(Ok(response)) => return Ok(response),
                Ok(Err((error, false))) => return Err(error),
                Ok(Err((error, true))) => {
                    if deadline.has_passed() {
                        return Err(error);
                    }
                    tokio::time::sleep(delay.min(deadline.remaining())).await;
                    if deadline.has_passed() {
                        return Err(error);
                    }
                    delay = (delay * 2).min(std::time::Duration::from_secs(1));
                }
                Err(_) => return Err(RuntimeError::MayHaveCommitted(
                    "Kubernetes broker control budget expired; original request remains unproved"
                        .into(),
                )),
            }
        }
    }
}

#[derive(Serialize)]
struct CreateWire<'a> {
    wire_version: u8,
    identity: &'a WorkerIdentity,
    create: K8sCreateRequest,
}

#[derive(Serialize)]
struct SealWire<'a> {
    wire_version: u8,
    identity: &'a WorkerIdentity,
    scope: &'a str,
    predecessor: &'a SandboxEffectFence,
    expected_pod_name: &'a str,
}

#[async_trait]
impl K8sCreateTransport for WorkerK8sCreateClient {
    async fn create(&self, request: K8sCreateRequest) -> Result<K8sCreateResponse, RuntimeError> {
        self.post(
            PATH,
            &CreateWire {
                wire_version: K8S_CREATE_WIRE_VERSION,
                identity: self.identity()?,
                create: request,
            },
        )
        .await
    }

    async fn seal_predecessor(
        &self,
        scope: &str,
        predecessor: &SandboxEffectFence,
        expected_pod_name: &str,
    ) -> Result<RequestJoin, RuntimeError> {
        self.post(
            SEAL_PATH,
            &SealWire {
                wire_version: K8S_CREATE_WIRE_VERSION,
                identity: self.identity()?,
                scope,
                predecessor,
                expected_pod_name,
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    mod capture_tests;
    use super::*;
    use axum::routing::post;
    use axum::{Json, Router};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Let real loopback I/O finish without auto-advancing a paused Tokio
    /// clock past the protocol deadline. A live blocking task is visible to
    /// Tokio's paused-clock scheduler. The independent two-second watchdog
    /// diagnoses a stuck fixture; it does not change the client's budget.
    async fn with_frozen_http_clock<F: std::future::Future>(work: F) -> F::Output {
        let (release, released) = std::sync::mpsc::channel::<()>();
        let (expired, expiry) = tokio::sync::oneshot::channel();
        let watchdog = tokio::task::spawn_blocking(move || {
            if matches!(
                released.recv_timeout(std::time::Duration::from_secs(2)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ) {
                let _ = expired.send(());
            }
        });
        let result = tokio::select! {
            result = work => result,
            _ = expiry => panic!("real HTTP fixture did not settle while its protocol clock was frozen"),
        };
        drop(release);
        watchdog.await.unwrap();
        result
    }

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
        // E4 the predecessor seal uses the same bound transport identity.
        let calls = Arc::new(AtomicUsize::new(0));
        let router = Router::new()
            .route(
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
                            assert_eq!(
                                body["wire_version"], K8S_CREATE_WIRE_VERSION,
                                "V1 current only"
                            );
                            assert_eq!(body["identity"]["incarnation_id"], "boot", "E2");
                            assert_eq!(body["create"]["path"], "/api/v1/namespaces/ns/pods", "E2");
                            assert_eq!(body["create"]["object"]["metadata"]["name"], "pod", "E2");
                            (
                                [(
                                    K8S_CREATE_CAPTURE_HEADER,
                                    CreateReplyProfile::FreshCreate.header_value(),
                                )],
                                Json(serde_json::json!({
                                    "status":201,
                                    "object":{"apiVersion":"v1","kind":"Pod","metadata":{
                                        "name":"pod","namespace":"ns","uid":"pod-uid"
                                    }}
                                })),
                            )
                        }
                    }
                }),
            )
            .route(
                SEAL_PATH,
                post({
                    let calls = calls.clone();
                    move |headers: axum::http::HeaderMap, Json(body): Json<serde_json::Value>| {
                        let calls = calls.clone();
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            assert_eq!(
                                headers.get("x-awaken-worker-id").unwrap(),
                                "awaken-worker",
                                "E4"
                            );
                            assert_eq!(
                                body["wire_version"], K8S_CREATE_WIRE_VERSION,
                                "V1 current seal"
                            );
                            assert_eq!(body["identity"]["incarnation_id"], "boot", "E4");
                            assert_eq!(body["scope"], "scope", "E4");
                            assert_eq!(body["predecessor"]["operation_id"], "old-effect", "E4");
                            assert_eq!(body["expected_pod_name"], "awaken-realization-scope", "E4");
                            (
                                [(
                                    K8S_CREATE_CAPTURE_HEADER,
                                    CreateReplyProfile::WholeJoin.header_value(),
                                )],
                                Json(serde_json::json!("unknown")),
                            )
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
        let predecessor =
            SandboxEffectFence::new("old-effect", "old-worker", "old-boot", 1, u64::MAX).unwrap();
        assert_eq!(
            client
                .seal_predecessor("scope", &predecessor, "awaken-realization-scope")
                .await
                .unwrap(),
            RequestJoin::Unknown,
            "E4"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "one Create and one signed seal"
        );
        server.abort();
    }

    #[tokio::test]
    async fn create_response_loss_replays_the_exact_ledger_request_before_returning_unknown() {
        // C1 broker accepted the immutable Create request; C2 reply is lost or
        // temporarily unavailable; C3 same request re-enters the durable ticket.
        // R1 C1+C2+C3 -> exact replay returns its already-created disposition,
        // never another body/identity/effect. Permanent/auth failures and an
        // exhausted control budget remain Unknown, never termination evidence.
        let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
        let router = Router::new().route(PATH, post({
            let bodies = bodies.clone();
            move |Json(body): Json<serde_json::Value>| {
                let bodies = bodies.clone();
                async move {
                    let mut history = bodies.lock().unwrap();
                    history.push(body);
                    if history.len() == 1 {
                        (axum::http::StatusCode::SERVICE_UNAVAILABLE, [(K8S_CREATE_CAPTURE_HEADER, CreateReplyProfile::FreshCreate.header_value())], Json(serde_json::json!({"error":"reply unavailable after Create admission"})))
                    } else {
                        assert_eq!(history[0], history[1], "R1 identical ledger key and request digest");
                        (axum::http::StatusCode::OK, [(K8S_CREATE_CAPTURE_HEADER, CreateReplyProfile::FreshCreate.header_value())], Json(serde_json::json!({"status":409,"object":{"kind":"Status","reason":"AlreadyExists"}})))
                    }
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = WorkerK8sCreateClient::new(WorkerUpstream::new(format!("http://{address}")));
        client
            .bind_identity(WorkerIdentity::new("awaken-worker", "boot", 1))
            .unwrap();
        let result = client.create(K8sCreateRequest { path:"/api/v1/namespaces/ns/pods".into(), object:serde_json::json!({"metadata":{"name":"exact","annotations":{"attempt":"original"}}}) }).await.unwrap();
        assert_eq!(result.status, 409);
        assert_eq!(bodies.lock().unwrap().len(), 2);
        server.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn create_replay_is_budget_bounded_and_never_retries_authentication_rejection() {
        // C1 400/401/403 -> exactly one actual HTTP response and refusal.
        // Keep the 100ms protocol clock frozen during loopback I/O: an expired
        // scheduler budget before the handler runs cannot prove this rule.
        for status in [400_u16, 401, 403] {
            let calls = Arc::new(AtomicUsize::new(0));
            let router = Router::new().route(
                PATH,
                post({
                    let calls = calls.clone();
                    move || {
                        let calls = calls.clone();
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            (
                                axum::http::StatusCode::from_u16(status).unwrap(),
                                Json(serde_json::json!({"error":"fixture refusal"})),
                            )
                        }
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            let mut client =
                WorkerK8sCreateClient::new(WorkerUpstream::new(format!("http://{address}")));
            client.request_budget = std::time::Duration::from_millis(100);
            client
                .bind_identity(WorkerIdentity::new("awaken-worker", "boot", 1))
                .unwrap();
            let result = with_frozen_http_clock(client.create(K8sCreateRequest {
                path: "/api/v1/namespaces/ns/pods".into(),
                object: serde_json::json!({"metadata":{"name":"exact"}}),
            }))
            .await;
            let error = match result {
                Err(error) => error,
                Ok(_) => panic!("C1 authentication/permanent rejection cannot succeed"),
            };
            let expected = format!(
                "Kubernetes broker refused or retained uncertainty: {}",
                axum::http::StatusCode::from_u16(status).unwrap()
            );
            assert!(
                matches!(error, RuntimeError::MayHaveCommitted(ref reason) if reason == &expected),
                "C1 must observe the actual {status} response, not an unrelated timeout: {error:?}"
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            server.abort();
        }

        // C2: a real 503 causes a replay. Hold that replay in its real HTTP
        // handler, then advance to the original total deadline. This proves
        // both retry admission and cancellation of an in-flight attempt, rather
        // than assuming two TCP exchanges fit into 100ms of shared-host time.
        let calls = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (entered, mut entries) = tokio::sync::mpsc::unbounded_channel();
        let router = Router::new().route(
            PATH,
            post({
                let calls = calls.clone();
                let bodies = bodies.clone();
                move |Json(body): Json<serde_json::Value>| {
                    let calls = calls.clone();
                    let bodies = bodies.clone();
                    let entered = entered.clone();
                    async move {
                        bodies.lock().unwrap().push(body);
                        let attempt = calls.fetch_add(1, Ordering::SeqCst) + 1;
                        entered.send(attempt).unwrap();
                        if attempt > 1 {
                            std::future::pending::<()>().await;
                        }
                        (
                            axum::http::StatusCode::SERVICE_UNAVAILABLE,
                            Json(serde_json::json!({"error":"fixture refusal"})),
                        )
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
        let budget = client.request_budget;
        let start = awaken_time::mono::now();
        let operation = tokio::spawn(async move {
            client
                .create(K8sCreateRequest {
                    path: "/api/v1/namespaces/ns/pods".into(),
                    object: serde_json::json!({"metadata":{"name":"exact"}}),
                })
                .await
        });
        assert_eq!(with_frozen_http_clock(entries.recv()).await, Some(1));
        // Only the actual retry delay uses real time; the 30s production
        // deadline below is exercised by explicit virtual-clock advancement.
        tokio::time::resume();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), entries.recv())
                .await
                .expect("C2 a 503 must admit a replay"),
            Some(2)
        );
        tokio::time::pause();
        let before_deadline = budget
            .checked_sub(awaken_time::mono::now().duration_since(start))
            .and_then(|remaining| remaining.checked_sub(std::time::Duration::from_millis(1)))
            .expect("fixture reaches the retry before the original budget");
        tokio::time::advance(before_deadline).await;
        assert!(
            !operation.is_finished(),
            "C2 in-flight replay remains live before the deadline"
        );
        tokio::time::advance(std::time::Duration::from_millis(1)).await;
        let result = with_frozen_http_clock(operation).await.unwrap();
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("C2 in-flight replay cannot succeed after the original deadline"),
        };
        assert!(
            matches!(error, RuntimeError::MayHaveCommitted(ref reason) if reason == "Kubernetes broker control budget expired; original request remains unproved"),
            "C2 original total deadline must bound the in-flight replay: {error:?}"
        );
        assert_eq!(awaken_time::mono::now().duration_since(start), budget);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "C2 no post-deadline replay"
        );
        let history = bodies.lock().unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(
            history[0], history[1],
            "C2 replay preserves the exact request"
        );
        server.abort();
    }
}
