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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::K8sRuntime;
    use awaken_sandbox_container::ContainerRuntime as _;
    use http::Request;
    use k8s_openapi::api::core::v1::{ConfigMap, PersistentVolumeClaim};
    use kube::Api;
    use kube::api::PostParams;
    use serde_json::{Value, json};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct RecordingBroker {
        requests: Mutex<Vec<(String, String)>>,
        join: RequestJoin,
        seal_calls: Arc<AtomicUsize>,
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
            SandboxEffectFence::new("old-effect", "worker", "worker-boot", 1, u64::MAX).unwrap();
        let fingerprint = serde_json::from_value(json!("fingerprint")).unwrap();
        let intent = awaken_sandbox_container::ContainerRealizationIntent::Create;
        let attempt = awaken_sandbox_container::engine::fresh_create_attempt();
        let context = awaken_sandbox_container::ContainerRealizationContext::new(
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
                seal_calls: Arc::new(AtomicUsize::new(0)),
            })
        };
        assert!(direct.with_create_transport(broker()).is_err(), "A1");
        drop(guard);
        let unfenced = awaken_sandbox_container::ContainerRealizationContext::new(
            "scope",
            &fingerprint,
            None,
            &intent,
            &attempt,
        );
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
            _predecessor: &SandboxEffectFence,
            _expected_pod_name: &str,
        ) -> Result<RequestJoin, RuntimeError> {
            self.seal_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.join.clone())
        }
    }

    #[tokio::test]
    async fn rich_brokered_terminal_seals_requests_without_granting_absence() {
        // Causes: C1 a plan has a projected participant and uses the broker;
        // C2 the old effect fence is exact/missing; C3 the broker joins all
        // requests or reports Unknown; C4 direct mode has no durable broker;
        // C5 Complete has no original Created Pod ticket/member handoff.
        // Effects: E1 C1+C2(exact) calls the one provider seal even when C3
        // is Complete, but C5 yields Unknown before any physical API call;
        // E2 missing C2 or C4 cannot borrow broker closure. R1=exact+Complete
        // +C5->E1, R2=exact+Unknown->E1, R3=missing|direct->E2. A sampled
        // 404 cannot supply the absent rich origin or authorize cleanup.
        let mut plan = super::super::ephemeral_plan();
        plan.binds.push(awaken_sandbox_container::BindPlan {
            source_ref: "content".into(),
            mount_path: "/mnt/content".into(),
            read_only: true,
            content: Some("body".into()),
            content_bytes: None,
            secret_content: None,
            secret_writeback: false,
            credential_file_path: None,
        });
        let fence =
            SandboxEffectFence::new("old-effect", "worker", "worker-boot", 1, u64::MAX).unwrap();
        for joined in [RequestJoin::Complete(Vec::new()), RequestJoin::Unknown] {
            let seals = Arc::new(AtomicUsize::new(0));
            let broker = Arc::new(RecordingBroker {
                requests: Mutex::new(Vec::new()),
                join: joined,
                seal_calls: seals.clone(),
            });
            let reads = Arc::new(AtomicUsize::new(0));
            let mut runtime = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap());
            runtime.clients.control = Client::new(
                tower::service_fn({
                    let reads = reads.clone();
                    move |_request: Request<Body>| {
                        let reads = reads.clone();
                        async move {
                            reads.fetch_add(1, Ordering::SeqCst);
                            Ok::<_, std::io::Error>(Response::builder().status(500)
                            .body(Body::from(serde_json::to_vec(&json!({
                                "apiVersion":"v1", "kind":"Status", "status":"Failure",
                                "reason":"UnexpectedPhysicalRead", "message":"missing original rich origin", "code":500
                            })).unwrap())).unwrap())
                        }
                    }
                }),
                "default",
            );
            let runtime = runtime.with_create_transport(broker).unwrap();
            assert_eq!(
                runtime
                    .missing_handle_terminal_evidence("scope", &plan, Some(&fence))
                    .await
                    .unwrap(),
                awaken_sandbox_container::MissingHandleTerminalEvidence::Unknown,
                "R1/R2 rich participant cannot receive absence"
            );
            assert_eq!(seals.load(Ordering::SeqCst), 1, "R1/R2 one provider seal");
            assert_eq!(
                runtime
                    .missing_handle_terminal_evidence("scope", &plan, None)
                    .await
                    .unwrap(),
                awaken_sandbox_container::MissingHandleTerminalEvidence::Unknown,
                "R3 no old fence"
            );
            assert_eq!(seals.load(Ordering::SeqCst), 1, "R3 no extra seal");
            assert_eq!(
                reads.load(Ordering::SeqCst),
                0,
                "R1-R3 no original ticket means zero API"
            );
        }
        let direct = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap());
        assert_eq!(
            direct
                .missing_handle_terminal_evidence("scope", &plan, Some(&fence))
                .await
                .unwrap(),
            awaken_sandbox_container::MissingHandleTerminalEvidence::Unknown,
            "R3 direct rich attempt remains unproven"
        );
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
            seal_calls: Arc::new(AtomicUsize::new(0)),
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
                    seal_calls: Arc::new(AtomicUsize::new(0)),
                }))
                .unwrap()
        };
        let fence =
            SandboxEffectFence::new("old-effect", "worker", "worker-boot", 1, u64::MAX).unwrap();
        assert_eq!(
            runtime(RequestJoin::Unknown)
                .missing_handle_terminal_evidence(
                    "scope",
                    &super::super::ephemeral_plan(),
                    Some(&fence)
                )
                .await
                .unwrap(),
            awaken_sandbox_container::MissingHandleTerminalEvidence::Unknown,
            "E1"
        );
        assert_eq!(
            reads.load(Ordering::SeqCst),
            0,
            "E1 no 404 used before join"
        );
        assert_eq!(
            runtime(RequestJoin::Complete(Vec::new()))
                .missing_handle_terminal_evidence(
                    "scope",
                    &super::super::ephemeral_plan(),
                    Some(&fence)
                )
                .await
                .unwrap(),
            awaken_sandbox_container::MissingHandleTerminalEvidence::Unmaterialized,
            "E2"
        );
        assert_eq!(
            reads.load(Ordering::SeqCst),
            2,
            "E2 exact Pod and claim reobserved"
        );
        let request = awaken_container_contract::CreateTicketRequest::new(
            "old-group",
            "pods",
            "default",
            "old-pod",
            "a".repeat(64),
        )
        .unwrap();
        let ticket = |status| awaken_container_contract::CreateTicket {
            request: request.clone(),
            status,
            origin: Default::default(),
            isolation_id: String::new(),
            isolation_pending: false,
        };
        assert_eq!(
            runtime(RequestJoin::Complete(vec![ticket(
                awaken_container_contract::TicketStatus::Finished(
                    awaken_container_contract::TicketResult::RejectedNoEffect { code: 403 },
                )
            )]))
            .missing_handle_terminal_evidence(
                "scope",
                &super::super::ephemeral_plan(),
                Some(&fence),
            )
            .await
            .unwrap(),
            awaken_sandbox_container::MissingHandleTerminalEvidence::Unmaterialized,
            "H2 definitive refusal"
        );
        for status in [
            awaken_container_contract::TicketStatus::Finished(
                awaken_container_contract::TicketResult::Created {
                    uid: "old-uid".into(),
                },
            ),
            awaken_container_contract::TicketStatus::InFlight,
        ] {
            assert_eq!(
                runtime(RequestJoin::Complete(vec![ticket(status)]))
                    .missing_handle_terminal_evidence(
                        "scope",
                        &super::super::ephemeral_plan(),
                        Some(&fence)
                    )
                    .await
                    .unwrap(),
                awaken_sandbox_container::MissingHandleTerminalEvidence::Unknown,
                "H3 a 404 cannot erase a materialized or unresolved Create"
            );
        }
    }

    #[tokio::test]
    async fn joined_created_pod_reconstructs_only_its_exact_live_incarnation() {
        // Causes: C1 old request group is sealed and every ticket finished;
        // C2 exactly one Created Pod ticket names the canonical scope and UID;
        // C3 current Pod has the same UID, frozen effect, and no PVC; C4 the
        // current Pod is foreign/terminating, an orphan claim exists, its
        // image/security spec differs despite copied annotations, or another
        // kind was Created/Isolated without a participant-close proof.
        // Effects: E1 C1+C2+C3 yields one typed incarnation for the existing
        // handle-bound cleanup; E2 any C4 or absent C1/C2 stays Unknown and
        // cannot turn a Pod GET into a cleanup receipt. Rules J1=C1+C2+C3->E1,
        // J2=C1+C2+C4->E2, J3=!C1|!C2->E2.
        let fence =
            SandboxEffectFence::new("old-effect", "worker", "worker-boot", 1, u64::MAX).unwrap();
        let plan = super::super::ephemeral_plan();
        let base = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap());
        let runtime_id = base.realization_runtime_id("scope").unwrap();
        let name = super::super::pod_name(&runtime_id);
        let fingerprint = serde_json::from_value(json!("fingerprint")).unwrap();
        let intent = awaken_sandbox_container::ContainerRealizationIntent::Create;
        let attempt = awaken_sandbox_container::engine::fresh_create_attempt();
        let context = awaken_sandbox_container::ContainerRealizationContext::new(
            "scope",
            &fingerprint,
            Some(&fence),
            &intent,
            &attempt,
        );
        let mut pod = base.pod_for_effect(&runtime_id, &plan, Some(&fence));
        crate::package_realization::stamp_sandbox_release_annotations(
            &mut pod,
            "scope",
            &plan.image,
        );
        base.stamp_effect_evidence(&mut pod, &context, &fingerprint);
        super::super::stamp_pod_realization(&mut pod).unwrap();
        pod.metadata.namespace = Some("default".into());
        pod.metadata.uid = Some("old-uid".into());
        pod.metadata.resource_version = Some("7".into());
        let current = Arc::new(Mutex::new(pod));
        let pod_present = Arc::new(AtomicBool::new(true));
        let calls = Arc::new(AtomicUsize::new(0));
        let current_claim = Arc::new(Mutex::new(None::<PersistentVolumeClaim>));
        let current_volume = Arc::new(Mutex::new(
            None::<awaken_container_contract::K8sContinuationVolume>,
        ));
        let service = tower::service_fn({
            let current = current.clone();
            let pod_present = pod_present.clone();
            let calls = calls.clone();
            let current_claim = current_claim.clone();
            let name = name.clone();
            move |request: Request<Body>| {
                let current = current.clone();
                let pod_present = pod_present.clone();
                let calls = calls.clone();
                let current_claim = current_claim.clone();
                let name = name.clone();
                async move {
                    assert_eq!(request.method(), Method::GET, "no new Create");
                    calls.fetch_add(1, Ordering::SeqCst);
                    let is_pod =
                        request.uri().path() == format!("/api/v1/namespaces/default/pods/{name}");
                    let (status, body) = if is_pod && pod_present.load(Ordering::SeqCst) {
                        (
                            200,
                            serde_json::to_value(&*current.lock().unwrap()).unwrap(),
                        )
                    } else if is_pod {
                        (
                            404,
                            json!({
                                "apiVersion":"v1", "kind":"Status", "status":"Failure",
                                "code":404, "reason":"NotFound"
                            }),
                        )
                    } else if let Some(claim) = current_claim.lock().unwrap().as_ref() {
                        (200, serde_json::to_value(claim).unwrap())
                    } else {
                        (
                            404,
                            json!({
                                "apiVersion":"v1", "kind":"Status", "status":"Failure",
                                "code":404, "reason":"NotFound"
                            }),
                        )
                    };
                    Ok::<_, std::io::Error>(
                        Response::builder()
                            .status(status)
                            .body(Body::from(serde_json::to_vec(&body).unwrap()))
                            .unwrap(),
                    )
                }
            }
        });
        let direct = Client::new(service, "default");
        let request = awaken_container_contract::CreateTicketRequest::new(
            "old-group",
            "pods",
            "default",
            &name,
            "a".repeat(64),
        )
        .unwrap();
        let created = awaken_container_contract::CreateTicket {
            request,
            status: awaken_container_contract::TicketStatus::Finished(
                awaken_container_contract::TicketResult::Created {
                    uid: "old-uid".into(),
                },
            ),
            origin: Default::default(),
            isolation_id: String::new(),
            isolation_pending: false,
        };
        let runtime = |tickets| {
            let mut runtime = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap());
            runtime.clients.control = direct.clone();
            runtime.continuation_volume = current_volume.lock().unwrap().clone();
            runtime
                .with_create_transport(Arc::new(RecordingBroker {
                    requests: Mutex::new(Vec::new()),
                    join: RequestJoin::Complete(tickets),
                    seal_calls: Arc::new(AtomicUsize::new(0)),
                }))
                .unwrap()
        };
        assert_eq!(
            runtime(vec![created.clone()])
                .missing_handle_terminal_evidence("scope", &plan, Some(&fence))
                .await
                .unwrap(),
            awaken_sandbox_container::MissingHandleTerminalEvidence::ExactIncarnation {
                container_id: name.clone(),
                runtime_handle: awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV3 {
                    pod_uid: "old-uid".into(),
                    claim_uid: None,
                    // J1 reuses the original sealed group's complete successful
                    // set plus exact old Pod projection. With no successful
                    // ConfigMap/Secret ticket this is an authoritative empty
                    // set; no current GET, new ledger or annotation fills it.
                    participants: Box::new(
                        awaken_sandbox_container::KubernetesProjectedParticipants::joined_empty(
                            "default".into(),
                            "old-uid".into(),
                            attempt.as_str().to_owned(),
                            fence.clone(),
                            "old-group".into()
                        )
                        .unwrap(),
                    ),
                },
            },
            "J1"
        );
        let original_spec = current.lock().unwrap().spec.clone();
        current.lock().unwrap().spec.as_mut().unwrap().containers[0].image =
            Some("foreign-image".into());
        assert_eq!(
            runtime(vec![created.clone()])
                .missing_handle_terminal_evidence("scope", &plan, Some(&fence))
                .await
                .unwrap(),
            awaken_sandbox_container::MissingHandleTerminalEvidence::Unknown,
            "J2 copied digest and effect annotations cannot authorize changed Pod spec"
        );
        current.lock().unwrap().spec = original_spec;
        current.lock().unwrap().metadata.uid = Some("foreign-uid".into());
        assert_eq!(
            runtime(vec![created.clone()])
                .missing_handle_terminal_evidence("scope", &plan, Some(&fence))
                .await
                .unwrap(),
            awaken_sandbox_container::MissingHandleTerminalEvidence::Unknown,
            "J2 same name with foreign UID"
        );
        current.lock().unwrap().metadata.uid = Some("old-uid".into());
        *current_claim.lock().unwrap() = Some(PersistentVolumeClaim {
            metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                name: Some("orphan".into()),
                namespace: Some("default".into()),
                uid: Some("claim-uid".into()),
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(
            runtime(vec![created.clone()])
                .missing_handle_terminal_evidence("scope", &plan, Some(&fence))
                .await
                .unwrap(),
            awaken_sandbox_container::MissingHandleTerminalEvidence::Unknown,
            "J2 an unbound PVC is still a physical participant"
        );
        *current_claim.lock().unwrap() = None;
        let mut other = created.clone();
        other.request.resource = "configmaps".into();
        assert_eq!(
            runtime(vec![other.clone()])
                .missing_handle_terminal_evidence("scope", &plan, Some(&fence))
                .await
                .unwrap(),
            awaken_sandbox_container::MissingHandleTerminalEvidence::Unknown,
            "J2 another created participant"
        );
        other.status = awaken_container_contract::TicketStatus::Finished(
            awaken_container_contract::TicketResult::Isolated {
                uid: "reserved-uid".into(),
            },
        );
        other.isolation_id = "0123456789abcdef0123456789abcdef".into();
        assert_eq!(
            runtime(vec![other])
                .missing_handle_terminal_evidence("scope", &plan, Some(&fence))
                .await
                .unwrap(),
            awaken_sandbox_container::MissingHandleTerminalEvidence::Unknown,
            "J2 an isolated other Kind is request closure, not participant closure"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            8,
            "J2 other kind rejects before GET"
        );

        // Retained-volume decision table: R1=joined original Pod and PVC
        // tickets + both original UIDs + immutable Pod/PVC projections and
        // effect fence => one typed Pod/PVC handle for the existing cleanup
        // CAS. R2=missing/foreign/terminating PVC, missing PVC ticket, altered
        // PVC spec, or altered Pod claim binding => Unknown and zero writes.
        // R3=PVC ticket without its original Pod => Unknown; a live claim is
        // never converted to absence by a Pod 404 or a partial Create join.
        let volume = awaken_container_contract::K8sContinuationVolume {
            storage_class_name: None,
            size: "1Gi".into(),
        };
        *current_volume.lock().unwrap() = Some(volume.clone());
        let mut retained = plan.clone();
        retained.filesystem_continuity =
            awaken_provisioning_contract::FilesystemContinuity::Retained;
        let retained_base =
            K8sRuntime::for_test("127.0.0.1:1".parse().unwrap()).with_continuation_volume(volume);
        let mut claim = retained_base
            .desired_create_claim_for_effect("scope", &retained, &fence)
            .unwrap()
            .unwrap();
        claim.metadata.namespace = Some("default".into());
        claim.metadata.uid = Some("claim-uid".into());
        claim.metadata.resource_version = Some("1".into());
        *current_claim.lock().unwrap() = Some(claim.clone());
        let mut retained_pod = retained_base.pod_for_effect(&runtime_id, &retained, Some(&fence));
        crate::package_realization::stamp_sandbox_release_annotations(
            &mut retained_pod,
            "scope",
            &retained.image,
        );
        super::super::continuation::bind_claim_uid(&mut retained_pod, "claim-uid");
        retained_base.stamp_effect_evidence(&mut retained_pod, &context, &fingerprint);
        super::super::stamp_pod_realization(&mut retained_pod).unwrap();
        retained_pod.metadata.namespace = Some("default".into());
        retained_pod.metadata.uid = Some("old-uid".into());
        retained_pod.metadata.resource_version = Some("7".into());
        *current.lock().unwrap() = retained_pod.clone();
        let claim_name = super::super::continuation_claim_for_pod(&name).unwrap();
        let claim_ticket = awaken_container_contract::CreateTicket {
            request: awaken_container_contract::CreateTicketRequest::new(
                "old-group",
                "persistentvolumeclaims",
                "default",
                &claim_name,
                "b".repeat(64),
            )
            .unwrap(),
            status: awaken_container_contract::TicketStatus::Finished(
                awaken_container_contract::TicketResult::Created {
                    uid: "claim-uid".into(),
                },
            ),
            origin: Default::default(),
            isolation_id: String::new(),
            isolation_pending: false,
        };
        let joined = || vec![claim_ticket.clone(), created.clone()];
        let exact = awaken_sandbox_container::MissingHandleTerminalEvidence::ExactIncarnation {
            container_id: name.clone(),
            runtime_handle:
                awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV3 {
                    pod_uid: "old-uid".into(),
                    claim_uid: Some("claim-uid".into()),
                    participants: Box::new(
                        awaken_sandbox_container::KubernetesProjectedParticipants::joined_empty(
                            "default".into(),
                            "old-uid".into(),
                            attempt.as_str().to_owned(),
                            fence.clone(),
                            "old-group".into(),
                        )
                        .unwrap(),
                    ),
                },
        };
        assert_eq!(
            runtime(joined())
                .missing_handle_terminal_evidence("scope", &retained, Some(&fence))
                .await
                .unwrap(),
            exact,
            "R1"
        );
        for tickets in [vec![created.clone()], vec![claim_ticket.clone()]] {
            assert_eq!(
                runtime(tickets)
                    .missing_handle_terminal_evidence("scope", &retained, Some(&fence))
                    .await
                    .unwrap(),
                awaken_sandbox_container::MissingHandleTerminalEvidence::Unknown,
                "R2/R3 partial participant join"
            );
        }
        let mut foreign = claim.clone();
        foreign.metadata.uid = Some("new-uid".into());
        *current_claim.lock().unwrap() = Some(foreign);
        assert_eq!(
            runtime(joined())
                .missing_handle_terminal_evidence("scope", &retained, Some(&fence))
                .await
                .unwrap(),
            awaken_sandbox_container::MissingHandleTerminalEvidence::Unknown,
            "R2 same-name foreign PVC UID"
        );
        let mut deleting = claim.clone();
        deleting.metadata.deletion_timestamp =
            Some(serde_json::from_value(json!("2026-10-07T00:00:00Z")).unwrap());
        *current_claim.lock().unwrap() = Some(deleting);
        assert_eq!(
            runtime(joined())
                .missing_handle_terminal_evidence("scope", &retained, Some(&fence))
                .await
                .unwrap(),
            awaken_sandbox_container::MissingHandleTerminalEvidence::Unknown,
            "R2 terminating PVC cannot be adopted as live continuity"
        );
        *current_claim.lock().unwrap() = Some(claim.clone());
        current_claim
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .spec
            .as_mut()
            .unwrap()
            .resources
            .as_mut()
            .unwrap()
            .requests
            .as_mut()
            .unwrap()
            .insert(
                "storage".into(),
                k8s_openapi::apimachinery::pkg::api::resource::Quantity("2Gi".into()),
            );
        assert_eq!(
            runtime(joined())
                .missing_handle_terminal_evidence("scope", &retained, Some(&fence))
                .await
                .unwrap(),
            awaken_sandbox_container::MissingHandleTerminalEvidence::Unknown,
            "R2 copied digest cannot authorize changed PVC spec"
        );
        *current_claim.lock().unwrap() = Some(claim.clone());
        super::super::continuation::bind_claim_uid(&mut current.lock().unwrap(), "foreign-uid");
        assert_eq!(
            runtime(joined())
                .missing_handle_terminal_evidence("scope", &retained, Some(&fence))
                .await
                .unwrap(),
            awaken_sandbox_container::MissingHandleTerminalEvidence::Unknown,
            "R2 Pod binds another PVC UID"
        );
        *current.lock().unwrap() = retained_pod;
        *current_claim.lock().unwrap() = None;
        assert_eq!(
            runtime(joined())
                .missing_handle_terminal_evidence("scope", &retained, Some(&fence))
                .await
                .unwrap(),
            awaken_sandbox_container::MissingHandleTerminalEvidence::Unknown,
            "R2 original PVC disappeared"
        );
        *current_claim.lock().unwrap() = Some(claim);
        pod_present.store(false, Ordering::SeqCst);
        assert_eq!(
            runtime(joined())
                .missing_handle_terminal_evidence("scope", &retained, Some(&fence))
                .await
                .unwrap(),
            awaken_sandbox_container::MissingHandleTerminalEvidence::Unknown,
            "R3 Pod 404 cannot erase the original live claim"
        );
        assert_eq!(
            runtime(vec![claim_ticket])
                .missing_handle_terminal_evidence("scope", &retained, Some(&fence))
                .await
                .unwrap(),
            awaken_sandbox_container::MissingHandleTerminalEvidence::Unknown,
            "R3 PVC-only Create cannot yield absence or a cleanup handle"
        );
    }

    struct FixedStatusBroker {
        status: u16,
        object: Value,
    }

    #[async_trait]
    impl K8sCreateTransport for FixedStatusBroker {
        async fn create(
            &self,
            _request: K8sCreateRequest,
        ) -> Result<K8sCreateResponse, RuntimeError> {
            Ok(K8sCreateResponse {
                status: self.status,
                object: self.object.clone(),
            })
        }

        async fn seal_predecessor(
            &self,
            _scope: &str,
            _predecessor: &SandboxEffectFence,
            _expected_pod_name: &str,
        ) -> Result<RequestJoin, RuntimeError> {
            Ok(RequestJoin::Unknown)
        }
    }

    #[tokio::test]
    async fn direct_and_brokered_create_refusals_classify_identically() {
        // ADR-0092 decision 1, transport agreement. Causes: C1 the apiserver
        // (direct) or the Coordinator broker (forwarding the upstream `Status`
        // unchanged, or its own stale-Worker `Forbidden`) answers a Pod Create
        // with status S and message M. Effect: E1 both transports produce the
        // same port class for every row of the classification table.
        use k8s_openapi::api::core::v1::Pod;
        let status_object = |code: u16, message: &str| {
            json!({
                "apiVersion": "v1", "kind": "Status", "metadata": {},
                "status": "Failure", "message": message, "reason": "Forbidden", "code": code
            })
        };
        let quota = "pods \"p\" is forbidden: exceeded quota: compute, requested: pods=1";
        let rows = [
            (403, quota, "CapacityUnavailable"),
            (403, "Forbidden", "Unauthorized"),
            (
                403,
                "pods is forbidden: User cannot create resource",
                "Unauthorized",
            ),
            (
                403,
                "failed quota: compute: must specify limits.cpu",
                "Unauthorized",
            ),
            (401, "Unauthorized", "Unauthorized"),
            (404, "not found", "NotFound"),
            (409, "already exists", "Conflict"),
            (422, "invalid", "Backend"),
        ];
        let class = |error: kube::Error| match crate::pc::SandboxError::from(
            super::super::error::backend(error),
        ) {
            crate::pc::SandboxError::CapacityUnavailable(_) => "CapacityUnavailable",
            crate::pc::SandboxError::Unauthorized(_) => "Unauthorized",
            crate::pc::SandboxError::NotFound(_) => "NotFound",
            crate::pc::SandboxError::Conflict(_) => "Conflict",
            crate::pc::SandboxError::Backend { .. } => "Backend",
            other => panic!("unexpected class {other:?}"),
        };
        for (code, message, expected) in rows {
            let object = status_object(code, message);
            let direct = Client::new(
                tower::service_fn({
                    let object = object.clone();
                    move |_request: Request<Body>| {
                        let object = object.clone();
                        async move {
                            Ok::<_, std::io::Error>(
                                Response::builder()
                                    .status(code)
                                    .header(header::CONTENT_TYPE, "application/json")
                                    .body(Body::from(serde_json::to_vec(&object).unwrap()))
                                    .unwrap(),
                            )
                        }
                    }
                }),
                "ns",
            );
            let brokered = brokered_client(
                direct.clone(),
                Arc::new(FixedStatusBroker {
                    status: code,
                    object,
                }),
            );
            let mut results = Vec::new();
            for client in [direct, brokered] {
                let error = Api::<Pod>::namespaced(client, "ns")
                    .create(&PostParams::default(), &Pod::default())
                    .await
                    .expect_err("refused Create");
                results.push(class(error));
            }
            assert_eq!(results, [expected, expected], "{code} {message}");
        }
    }
}
