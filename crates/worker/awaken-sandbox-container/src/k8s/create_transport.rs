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
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{K8sRuntime, RequestJoin};
use crate::{ContainerEffectFence, RuntimeError};

/// Ephemeral wire payload. Object bytes may contain credential material and
/// must never enter the durable ticket ledger, debug logs or error rendering.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct K8sCreateRequest {
    pub path: String,
    pub object: Value,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct K8sCreateResponse {
    pub status: u16,
    pub object: Value,
}

#[async_trait]
pub trait K8sCreateTransport: Send + Sync {
    async fn create(&self, request: K8sCreateRequest) -> Result<K8sCreateResponse, RuntimeError>;
    async fn seal_predecessor(
        &self,
        scope: &str,
        predecessor: &ContainerEffectFence,
    ) -> Result<RequestJoin, RuntimeError>;
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The one POST-intercepting kube client shared by Session realization,
/// package image preparation and node-image prewarm. Unknown POST routes are
/// sent to the broker for refusal; they never fall through to the Worker's
/// direct ServiceAccount client.
pub(crate) fn brokered_client(direct: Client, broker: Arc<dyn K8sCreateTransport>) -> Client {
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

impl K8sRuntime {
    /// Select the independent Create executor once for this runtime. Every
    /// `Api<K>::create` on the control client then crosses the same broker;
    /// reads and UID-fenced disposal remain on the existing Kubernetes client.
    pub fn with_create_transport(
        mut self,
        transport: Arc<dyn K8sCreateTransport>,
    ) -> Result<Self, RuntimeError> {
        if self.create_transport.is_some()
            || self
                .create_mode_frozen
                .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(RuntimeError::Backend(
                "Kubernetes Create transport cannot replace an active admission mode".into(),
            ));
        }
        self.clients.control = brokered_client(self.clients.control.clone(), transport.clone());
        self.create_transport = Some(transport);
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ContainerRuntime as _;
    use http::Request;
    use k8s_openapi::api::core::v1::ConfigMap;
    use kube::Api;
    use kube::api::PostParams;
    use serde_json::json;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RecordingBroker {
        requests: Mutex<Vec<(String, String)>>,
        join: RequestJoin,
    }

    #[tokio::test]
    async fn create_admission_modes_cannot_overlap_or_switch_after_first_direct_effect() {
        // Causes: C1 direct runtime has entered an effect; C2 broker selection
        // occurs before/after C1; C3 brokered effect is admitted. Effects:
        // E1 C1+C2(after) rejects a mid-flight authority switch; E2 C2(before)
        // +C3 does not create a second process-local gate. C4 a direct entry
        // has no fence and therefore no local gate. Rules A1=C1+late
        // broker->E1, A2=early broker+C3->E2, A3=C4+late broker->E1.
        // The provider ticket remains the
        // only brokered Create admission and close authority.
        let fence =
            ContainerEffectFence::new("old-effect", "worker", "worker-boot", 1, u64::MAX).unwrap();
        let fingerprint = serde_json::from_value(json!("fingerprint")).unwrap();
        let intent = crate::ContainerRealizationIntent::Create;
        let attempt = crate::fresh_create_attempt();
        let context = crate::ContainerRealizationContext::new(
            "scope",
            &fingerprint,
            Some(&fence),
            &intent,
            &attempt,
        );
        let plan = super::super::ephemeral_plan();
        let direct = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap());
        let guard = direct
            .admit_observed_creation_attempt(&context, &plan)
            .await
            .unwrap()
            .expect("A1 direct gate exists");
        let broker = || {
            Arc::new(RecordingBroker {
                requests: Mutex::new(Vec::new()),
                join: RequestJoin::Unknown,
            })
        };
        assert!(direct.with_create_transport(broker()).is_err(), "A1");
        drop(guard);
        let unfenced =
            crate::ContainerRealizationContext::new("scope", &fingerprint, None, &intent, &attempt);
        let direct_unfenced = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap());
        assert!(
            direct_unfenced
                .admit_observed_creation_attempt(&unfenced, &plan)
                .await
                .unwrap()
                .is_none(),
            "A3 no process-local proof gate"
        );
        assert!(
            direct_unfenced.with_create_transport(broker()).is_err(),
            "A3 even an ungated direct effect freezes the transport mode"
        );
        let brokered = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap())
            .with_create_transport(broker())
            .unwrap();
        assert!(
            brokered
                .admit_observed_creation_attempt(&context, &plan)
                .await
                .unwrap()
                .is_none(),
            "A2 provider ledger owns admission"
        );
        assert!(
            brokered.creation_attempts.lock().unwrap().is_empty(),
            "A2 no parallel gate"
        );
    }

    #[async_trait]
    impl K8sCreateTransport for RecordingBroker {
        async fn create(
            &self,
            request: K8sCreateRequest,
        ) -> Result<K8sCreateResponse, RuntimeError> {
            let name = request.object["metadata"]["name"]
                .as_str()
                .unwrap_or("")
                .to_owned();
            self.requests
                .lock()
                .unwrap()
                .push((request.path.clone(), name));
            Ok(K8sCreateResponse {
                status: 201,
                object: json!({
                    "apiVersion":"v1", "kind":"ConfigMap",
                    "metadata":{"name":"cm", "namespace":"ns", "uid":"broker-uid"},
                    "data":{}
                }),
            })
        }

        async fn seal_predecessor(
            &self,
            _scope: &str,
            _predecessor: &ContainerEffectFence,
        ) -> Result<RequestJoin, RuntimeError> {
            Ok(self.join.clone())
        }
    }

    #[tokio::test]
    async fn brokered_control_client_forwards_every_create_and_keeps_reads_direct() {
        // Causes: C1 provider installs one transport; C2 typed Api issues
        // POST versus GET; C3 a query-bearing or unrecognized POST appears;
        // C4 composition tries a second transport. Effects: E1 C1+C2 POST
        // reaches only the broker and preserves typed response; E2 GET uses
        // the existing client; E3 C3 cannot bypass by direct API; E4 C4 fails
        // rather than layering two competing Create authorities.
        let direct_calls = Arc::new(Mutex::new(Vec::new()));
        let service =
            tower::service_fn({
                let direct_calls = direct_calls.clone();
                move |request: Request<Body>| {
                    let direct_calls = direct_calls.clone();
                    async move {
                        direct_calls.lock().unwrap().push(request.method().clone());
                        assert_eq!(request.method(), Method::GET, "no direct Create");
                        Ok::<_, std::io::Error>(Response::builder()
                        .status(200)
                        .body(Body::from(serde_json::to_vec(&json!({
                            "apiVersion":"v1", "kind":"ConfigMap",
                            "metadata":{"name":"cm", "namespace":"ns", "uid":"broker-uid"},
                            "data":{}
                        })).unwrap()))
                        .unwrap())
                    }
                }
            });
        let mut runtime = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap());
        runtime.clients.control = Client::new(service, "ns");
        let direct_configuration = runtime.realization_configuration().unwrap();
        let broker = Arc::new(RecordingBroker {
            requests: Mutex::new(Vec::new()),
            join: RequestJoin::Unknown,
        });
        let runtime = runtime.with_create_transport(broker.clone()).unwrap();
        let brokered_configuration = runtime.realization_configuration().unwrap();
        assert!(
            !direct_configuration.contains_key("create_request_transport"),
            "E5 historical direct realization identity"
        );
        assert_eq!(
            brokered_configuration
                .get("create_request_transport")
                .map(String::as_str),
            Some("brokered-v1"),
            "E5 brokered realization is distinct"
        );
        let api: Api<ConfigMap> = Api::namespaced(runtime.clients.control.clone(), "ns");
        let desired: ConfigMap = serde_json::from_value(json!({
            "metadata":{"name":"cm", "namespace":"ns"}, "data":{}
        }))
        .unwrap();
        assert_eq!(
            api.create(&PostParams::default(), &desired)
                .await
                .unwrap()
                .metadata
                .uid
                .as_deref(),
            Some("broker-uid"),
            "E1"
        );
        assert_eq!(
            broker.requests.lock().unwrap().as_slice(),
            [("/api/v1/namespaces/ns/configmaps".into(), "cm".into())],
            "E1 exact typed path"
        );
        assert_eq!(
            api.get("cm").await.unwrap().metadata.uid.as_deref(),
            Some("broker-uid"),
            "E2"
        );
        assert_eq!(
            direct_calls.lock().unwrap().as_slice(),
            [Method::GET],
            "E2 only read is direct"
        );

        let raw = Request::builder()
            .method(Method::POST)
            .uri("/api/v1/namespaces/ns/configmaps?dryRun=All")
            .body(Body::from(b"{}".to_vec()))
            .unwrap();
        assert!(
            runtime.clients.control.send(raw).await.is_err(),
            "E3 query-bearing POST refuses"
        );
        assert_eq!(
            broker.requests.lock().unwrap().len(),
            1,
            "E3 no hidden retry"
        );
        let unknown = Request::builder()
            .method(Method::POST)
            .uri("/apis/unrecognized/v1/namespaces/ns/objects")
            .body(Body::from(
                br#"{"metadata":{"name":"unexpected"}}"#.to_vec(),
            ))
            .unwrap();
        assert_eq!(
            runtime
                .clients
                .control
                .send(unknown)
                .await
                .unwrap()
                .status(),
            StatusCode::CREATED,
            "E3 unknown Create reaches broker for refusal"
        );
        assert_eq!(
            broker.requests.lock().unwrap()[1].0,
            "/apis/unrecognized/v1/namespaces/ns/objects",
            "E3 no direct fallback"
        );
        assert_eq!(
            direct_calls.lock().unwrap().as_slice(),
            [Method::GET],
            "E3 no direct POST"
        );
        assert!(runtime.with_create_transport(broker).is_err(), "E4");
    }

    #[tokio::test]
    async fn handle_free_absence_needs_independent_join_before_api_404() {
        // Causes: C1 one brokered Worker effect has no durable handle and a
        // zero-participant plan; C2 old Create lifetime is Unknown/Complete;
        // C3 the canonical Pod and claim both answer 404; C4 the joined
        // ticket set is empty, definitively refused, Created, or InFlight.
        // Effects: E1 Unknown never reads absence; E2 Complete+empty/refused
        // plus C3 permits narrow absence; E3 Created/InFlight refuses even
        // after both 404s. Decision rows: H1=Unknown->E1;
        // H2=Complete+(empty|RejectedNoEffect)+C3->E2;
        // H3=Complete+(Created|InFlight)+C3->E3.
        let reads = Arc::new(AtomicUsize::new(0));
        let service = tower::service_fn({
            let reads = reads.clone();
            move |request: Request<Body>| {
                let reads = reads.clone();
                async move {
                    assert_eq!(request.method(), Method::GET, "no direct Create");
                    reads.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, std::io::Error>(
                        Response::builder()
                            .status(404)
                            .body(Body::from(
                                serde_json::to_vec(&json!({
                                    "apiVersion":"v1", "kind":"Status", "status":"Failure",
                                    "code":404, "reason":"NotFound"
                                }))
                                .unwrap(),
                            ))
                            .unwrap(),
                    )
                }
            }
        });
        let direct = Client::new(service, "default");
        let runtime = |joined| {
            let mut runtime = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap());
            runtime.clients.control = direct.clone();
            runtime
                .with_create_transport(Arc::new(RecordingBroker {
                    requests: Mutex::new(Vec::new()),
                    join: joined,
                }))
                .unwrap()
        };
        let fence =
            ContainerEffectFence::new("old-effect", "worker", "worker-boot", 1, u64::MAX).unwrap();
        assert!(
            !runtime(RequestJoin::Unknown)
                .prove_unmaterialized_absence(
                    "scope",
                    &super::super::ephemeral_plan(),
                    Some(&fence)
                )
                .await
                .unwrap(),
            "E1"
        );
        assert_eq!(
            reads.load(Ordering::SeqCst),
            0,
            "E1 no 404 used before join"
        );
        assert!(
            runtime(RequestJoin::Complete(Vec::new()))
                .prove_unmaterialized_absence(
                    "scope",
                    &super::super::ephemeral_plan(),
                    Some(&fence)
                )
                .await
                .unwrap(),
            "E2"
        );
        assert_eq!(
            reads.load(Ordering::SeqCst),
            2,
            "E2 exact Pod and claim reobserved"
        );
        let request = super::super::CreateTicketRequest::new(
            "old-group",
            "pods",
            "default",
            "old-pod",
            "a".repeat(64),
        )
        .unwrap();
        let ticket = |status| super::super::CreateTicket {
            request: request.clone(),
            status,
        };
        assert!(
            runtime(RequestJoin::Complete(vec![ticket(
                super::super::TicketStatus::Finished(
                    super::super::TicketResult::RejectedNoEffect { code: 403 },
                )
            )]))
            .prove_unmaterialized_absence("scope", &super::super::ephemeral_plan(), Some(&fence))
            .await
            .unwrap(),
            "H2 definitive refusal"
        );
        for status in [
            super::super::TicketStatus::Finished(super::super::TicketResult::Created {
                uid: "old-uid".into(),
            }),
            super::super::TicketStatus::InFlight,
        ] {
            assert!(
                !runtime(RequestJoin::Complete(vec![ticket(status)]))
                    .prove_unmaterialized_absence(
                        "scope",
                        &super::super::ephemeral_plan(),
                        Some(&fence)
                    )
                    .await
                    .unwrap(),
                "H3 a 404 cannot erase a materialized or unresolved Create"
            );
        }
    }
}
