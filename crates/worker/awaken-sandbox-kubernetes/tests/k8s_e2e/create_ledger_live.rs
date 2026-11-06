use super::*;
use awaken_container_contract::K8sCreateBrokerEffects;
use awaken_container_contract::{
    CreateTicketRequest, ISOLATION_LABEL, K8sLedgerBinding, RequestJoin, TicketAdmission,
    TicketResult,
};
use awaken_container_contract::{K8sCreateRequest, K8sCreateResponse};

use awaken_worker_fleet_kubernetes::K8sCreateTransport;
use awaken_worker_fleet_kubernetes::K8sWorkerCreateLedger;

const GUARDED_CLAIM_GROUP: &str = "guarded-claim-effect";

/// The real API accepts the original PVC while the old Worker loses its Pod
/// Create response. The same persisted ledger is reopened for terminal join.
#[derive(Clone)]
struct ClaimOnlyBroker {
    client: kube::Client,
    ledger: K8sWorkerCreateLedger,
    sandbox_namespace: String,
    ledger_name: String,
    ledger_uid: String,
    scope: String,
    predecessor: awaken_provisioning_contract::SandboxEffectFence,
    created_claim: Arc<Mutex<Option<(String, String)>>>,
}

impl ClaimOnlyBroker {
    fn binding(&self) -> K8sLedgerBinding<'_> {
        K8sLedgerBinding {
            worker_namespace: &self.sandbox_namespace,
            worker_name: "claim-worker",
            worker_uid: "claim-worker-uid",
            worker_incarnation: "claim-worker-uid",
            ledger_name: &self.ledger_name,
            ledger_uid: &self.ledger_uid,
            configuration_fingerprint: self.ledger.configuration_fingerprint(),
        }
    }
}

#[async_trait]
impl K8sCreateTransport for ClaimOnlyBroker {
    async fn create(
        &self,
        request: K8sCreateRequest,
    ) -> Result<K8sCreateResponse, awaken_container_contract::RuntimeError> {
        use awaken_container_contract::RuntimeError;
        use k8s_openapi::api::core::v1::PersistentVolumeClaim;
        use kube::Api;
        use kube::api::PostParams;
        if request.path.ends_with("/pods") {
            return Err(RuntimeError::MayHaveCommitted(
                "injected original Worker loss before Pod request admission".into(),
            ));
        }
        if !request.path.ends_with("/persistentvolumeclaims") {
            return Err(RuntimeError::Backend(
                "claim-only fixture received another Create Kind".into(),
            ));
        }
        let claim: PersistentVolumeClaim = serde_json::from_value(request.object)
            .map_err(|error| RuntimeError::Backend(error.to_string()))?;
        let name = claim
            .metadata
            .name
            .as_deref()
            .ok_or_else(|| RuntimeError::Backend("original PVC has no name".into()))?;
        let ticket = CreateTicketRequest::new(
            GUARDED_CLAIM_GROUP,
            "persistentvolumeclaims",
            &self.sandbox_namespace,
            name,
            "a".repeat(64),
        )?;
        if self.ledger.admit_request(self.binding(), &ticket).await?.0 != TicketAdmission::Forward {
            return Err(RuntimeError::Backend(
                "original PVC did not win one durable Create ticket".into(),
            ));
        }
        let created =
            Api::<PersistentVolumeClaim>::namespaced(self.client.clone(), &self.sandbox_namespace)
                .create(&PostParams::default(), &claim)
                .await
                .map_err(|error| RuntimeError::Backend(error.to_string()))?;
        let uid = created
            .metadata
            .uid
            .clone()
            .ok_or_else(|| RuntimeError::Backend("original PVC has no UID".into()))?;
        self.ledger
            .finish_request(
                self.binding(),
                &ticket,
                &TicketResult::Created { uid: uid.clone() },
            )
            .await?;
        *self.created_claim.lock().unwrap() = Some((name.to_owned(), uid));
        Ok(K8sCreateResponse {
            status: 201,
            object: serde_json::to_value(created)
                .map_err(|error| RuntimeError::Backend(error.to_string()))?,
        })
    }

    async fn seal_predecessor(
        &self,
        scope: &str,
        predecessor: &awaken_provisioning_contract::SandboxEffectFence,
        expected_pod_name: &str,
    ) -> Result<RequestJoin, awaken_container_contract::RuntimeError> {
        if scope != self.scope || !predecessor.same_effect_identity(&self.predecessor) {
            return Ok(RequestJoin::Unknown);
        }
        self.ledger.seal_worker(self.binding()).await?;
        self.ledger
            .seal_group_and_join(self.binding(), GUARDED_CLAIM_GROUP)
            .await?;
        self.ledger
            .reserve_terminal_claim_pod_and_join(
                self.binding(),
                GUARDED_CLAIM_GROUP,
                scope,
                expected_pod_name,
            )
            .await
    }
}

const PROTECTED_OTHER_KINDS: [&str; 5] = [
    "persistentvolumeclaims",
    "configmaps",
    "secrets",
    "jobs",
    "daemonsets",
];

fn protected_other_kind_resource(resource: &str) -> kube::core::ApiResource {
    use kube::core::{ApiResource, GroupVersionKind};
    let (group, kind) = match resource {
        "persistentvolumeclaims" => ("", "PersistentVolumeClaim"),
        "configmaps" => ("", "ConfigMap"),
        "secrets" => ("", "Secret"),
        "jobs" => ("batch", "Job"),
        "daemonsets" => ("apps", "DaemonSet"),
        _ => panic!("unsupported protected reservation Kind"),
    };
    ApiResource::from_gvk_with_plural(&GroupVersionKind::gvk(group, "v1", kind), resource)
}

fn generated_namespace(prefix: &str) -> k8s_openapi::api::core::v1::Namespace {
    serde_json::from_value(serde_json::json!({"metadata": {"generateName": prefix}})).unwrap()
}

fn install_canonical_sandbox_network_policy(
    namespace: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let network_graph = format!(
        "{}/../../../deploy/k3d/bases/sandbox-network-policy",
        env!("CARGO_MANIFEST_DIR")
    );
    let installed = kubectl(&["-n", namespace, "apply", "-k", &network_graph]);
    if !installed.status.success() {
        return Err(std::io::Error::other(format!(
            "canonical Sandbox NetworkPolicy installation failed: {}",
            String::from_utf8_lossy(&installed.stderr)
        ))
        .into());
    }
    Ok(())
}

async fn delete_namespace_by_uid(
    api: &kube::Api<k8s_openapi::api::core::v1::Namespace>,
    object: &k8s_openapi::api::core::v1::Namespace,
) -> Result<(), kube::Error> {
    use kube::ResourceExt as _;
    api.delete(
        &object.name_any(),
        &kube::api::DeleteParams::default().preconditions(kube::api::Preconditions {
            uid: object.metadata.uid.clone(),
            resource_version: None,
        }),
    )
    .await?;
    Ok(())
}

/// Namespace finalization needs every aggregated APIService available. A
/// reachable node alone can strand an otherwise empty test Namespace.
async fn require_namespace_finalization_discovery(client: &kube::Client) {
    use k8s_openapi::kube_aggregator::pkg::apis::apiregistration::v1::APIService;
    let services: kube::Api<APIService> = kube::Api::all(client.clone());
    for attempt in 0..30 {
        let observed = services
            .list(&kube::api::ListParams::default())
            .await
            .expect("APIService list must be readable before Namespace creation");
        let unavailable = observed
            .items
            .iter()
            .filter(|item| {
                item.spec
                    .as_ref()
                    .and_then(|spec| spec.service.as_ref())
                    .is_some()
            })
            .filter(|item| {
                !item
                    .status
                    .as_ref()
                    .and_then(|status| status.conditions.as_ref())
                    .is_some_and(|conditions| {
                        conditions.iter().any(|condition| {
                            condition.type_ == "Available" && condition.status == "True"
                        })
                    })
            })
            .map(|item| item.metadata.name.clone().unwrap_or_default())
            .collect::<Vec<_>>();
        if unavailable.is_empty() {
            return;
        }
        if attempt == 29 {
            panic!("namespace cleanup requires available aggregated APIs: {unavailable:?}");
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

#[tokio::test]
async fn first_pod_create_verifies_the_live_api_defaulted_projection() {
    // Causes: C1 a unique, effect-fenced zero-participant plan reaches the
    // actual Kubernetes Pod Create API; C2 the apiserver fills Pod defaults and
    // assigns a UID/resourceVersion. Effects: E1 the shared complete Pod
    // projection verifier accepts the exact object; E2 the exact UID is deleted
    // with a precondition before the test returns. Rule LCP1=C1+C2->E1+E2.
    // The unit decision table owns a copied-digest/spec-tamper refusal; this
    // live row qualifies the API's real defaulting, not process/exec liveness.
    if !require_live_cluster() {
        return;
    }
    use awaken_provisioning_contract::{SandboxCreateAttempt, SandboxEffectFence};
    use awaken_sandbox_container::{
        ContainerPlan, ContainerRealizationContext, ContainerRealizationIntent, NetworkMode,
        RootfsPlan,
    };
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::{DeleteParams, ListParams, Preconditions};
    use kube::{Api, ResourceExt};

    let client = awaken_worker_fleet_kubernetes::connect_control_client()
        .await
        .unwrap();
    let pods: Api<Pod> = Api::namespaced(client, "default");
    let scope = format!("k8s-create-projection-{}", uuid::Uuid::new_v4());
    let image = fixture_image();
    let plan = ContainerPlan {
        image: image.clone(),
        command: vec!["sh".into(), "-c".into(), "sleep 60".into()],
        env: Vec::new(),
        control_services: Default::default(),
        packages: Default::default(),
        binds: Vec::new(),
        outputs_volume: "/mnt/session/outputs".into(),
        network: NetworkMode::Open,
        egress_identity: Default::default(),
        requests: Default::default(),
        limits: Default::default(),
        filesystem_continuity: pc::FilesystemContinuity::Ephemeral,
        memory_mounts: Vec::new(),
        rootfs: RootfsPlan::Image(image),
    };
    let runtime = k8s_common::connect_with_clock("default", "127.0.0.1:1".parse().unwrap())
        .await
        .unwrap();
    let fingerprint = serde_json::from_value(serde_json::json!("live-create-projection")).unwrap();
    let fence = SandboxEffectFence::new(
        "live-create-projection",
        "worker",
        "worker-boot",
        1,
        u64::MAX,
    )
    .unwrap();
    let attempt = SandboxCreateAttempt::new(uuid::Uuid::new_v4().to_string());
    let intent = ContainerRealizationIntent::Create;
    let context =
        ContainerRealizationContext::new(&scope, &fingerprint, Some(&fence), &intent, &attempt);
    let result = runtime
        .prepare_for_effect(&context, &plan, &fingerprint)
        .await;
    // A failed verifier can still follow an accepted Create. Find only this
    // test's scope so cleanup remains exact even on the negative outcome.
    let matches = pods
        .list(&ListParams::default())
        .await
        .unwrap()
        .items
        .into_iter()
        .filter(|pod| {
            pod.metadata
                .annotations
                .as_ref()
                .and_then(|values| values.get("awaken.dev/sandbox-scope"))
                == Some(&scope)
        })
        .collect::<Vec<_>>();
    let count = matches.len();
    for pod in matches {
        let name = pod.name_any();
        let uid = pod.metadata.uid.clone().expect("created Pod has UID");
        pods.delete(
            &name,
            &DeleteParams {
                preconditions: Some(Preconditions {
                    uid: Some(uid.clone()),
                    resource_version: None,
                }),
                ..DeleteParams::default()
            },
        )
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while pods.get_opt(&name).await.unwrap().is_some() {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("E2 exact Pod deletion converges");
    }
    assert!(count <= 1, "one scope has at most one physical Pod");
    assert!(result.is_ok(), "E1 first Pod Create: {result:?}");
}

#[tokio::test]
async fn worker_create_ledger_joins_api_requests_and_rejects_recreated_uid() {
    // Causes: C1 two Coordinator instances contend on one group; C2 Create
    // admission wins before seal or seal wins first; C3 an admitted API request
    // remains InFlight after Worker loss; C4 independent completion records its
    // UID; C5 the ledger is deleted and recreated under the same name.
    // Effects: E1 exactly one CAS order wins, E2 no request is forwarded after
    // seal, E3 InFlight never proves closure, E4 exact completion joins, E5
    // the original Registry-frozen UID rejects replacement. Rules L1=C1+C2
    // => E1+E2, L2=admitted+C3=>E3, L3=L2+C4=>E4, L4=C5=>E5.
    // This live API test owns Kubernetes resourceVersion behavior; the pure
    // decision table and lost-response fixture own all logical partitions.
    if !require_live_cluster() {
        return;
    }
    use k8s_openapi::api::core::v1::{ConfigMap, Namespace};
    use kube::api::{DeleteParams, PostParams, Preconditions};
    use kube::{Api, ResourceExt};

    let client = awaken_worker_fleet_kubernetes::connect_control_client()
        .await
        .unwrap();
    let namespaces: Api<Namespace> = Api::all(client.clone());
    // C0 complete aggregated discovery; E0 a missing endpoint fails before
    // generated Namespace creation rather than during task-owned cleanup.
    require_namespace_finalization_discovery(&client).await;
    let sandbox = namespaces
        .create(
            &PostParams::default(),
            &generated_namespace("ledger-sandbox-"),
        )
        .await
        .unwrap();
    let control = namespaces
        .create(
            &PostParams::default(),
            &generated_namespace("ledger-control-"),
        )
        .await
        .unwrap();
    let sandbox_name = sandbox.name_any();
    let control_name = control.name_any();
    let first = K8sWorkerCreateLedger::new(
        client.clone(),
        &control_name,
        &sandbox_name,
        &sandbox_name,
        "provider-a",
    )
    .unwrap();
    let replacement = K8sWorkerCreateLedger::new(
        client.clone(),
        &control_name,
        &sandbox_name,
        &sandbox_name,
        "provider-a",
    )
    .unwrap();
    let (ledger_name, original_uid) = first
        .initialize(&sandbox_name, "worker", "worker-uid", "worker-uid")
        .await
        .unwrap();
    let binding = K8sLedgerBinding {
        worker_namespace: &sandbox_name,
        worker_name: "worker",
        worker_uid: "worker-uid",
        worker_incarnation: "worker-uid",
        ledger_name: &ledger_name,
        ledger_uid: &original_uid,
        configuration_fingerprint: first.configuration_fingerprint(),
    };
    let request = CreateTicketRequest::new(
        "effect-a",
        "pods",
        &sandbox_name,
        "sandbox-pod",
        "a".repeat(64),
    )
    .unwrap();
    let (admission, joined) = tokio::join!(
        first.admit_request(binding, &request),
        replacement.seal_group_and_join(binding, "effect-a")
    );
    let admission = admission.unwrap().0;
    let joined = joined.unwrap();
    match admission {
        TicketAdmission::Forward => {
            assert_eq!(
                joined,
                RequestJoin::Unknown,
                "L2 admitted request remains InFlight"
            );
            replacement
                .finish_request(
                    binding,
                    &request,
                    &TicketResult::Created {
                        uid: "pod-uid".into(),
                    },
                )
                .await
                .unwrap();
            assert!(
                matches!(replacement.seal_group_and_join(binding, "effect-a").await.unwrap(), RequestJoin::Complete(tickets) if tickets.len() == 1),
                "L3"
            );
        }
        TicketAdmission::Sealed => {
            assert_eq!(
                joined,
                RequestJoin::Complete(Vec::new()),
                "L1 seal won first"
            );
        }
        other => panic!("L1 unexpected first admission: {other:?}"),
    }
    assert_eq!(
        first
            .admit_request(
                binding,
                &CreateTicketRequest::new(
                    "effect-a",
                    "pods",
                    &sandbox_name,
                    "late-pod",
                    "b".repeat(64)
                )
                .unwrap()
            )
            .await
            .unwrap()
            .0,
        TicketAdmission::Sealed,
        "L1/E2"
    );

    let configmaps: Api<ConfigMap> = Api::namespaced(client.clone(), &control_name);
    configmaps
        .delete(
            &ledger_name,
            &DeleteParams::default().preconditions(Preconditions {
                uid: Some(original_uid.clone()),
                resource_version: None,
            }),
        )
        .await
        .unwrap();
    let resource = format!("configmap/{ledger_name}");
    let deleted = kubectl(&[
        "-n",
        &control_name,
        "wait",
        "--for=delete",
        &resource,
        "--timeout=20s",
    ]);
    assert!(
        deleted.status.success(),
        "exact test ledger deletion: {}",
        String::from_utf8_lossy(&deleted.stderr)
    );
    let (_, recreated_uid) = first
        .initialize(&sandbox_name, "worker", "worker-uid", "worker-uid")
        .await
        .unwrap();
    assert_ne!(recreated_uid, original_uid, "L4 name is not an identity");
    assert!(
        replacement
            .seal_group_and_join(binding, "effect-a")
            .await
            .is_err(),
        "L4/E5 frozen UID rejects replacement"
    );

    for object in [control, sandbox] {
        delete_namespace_by_uid(&namespaces, &object).await.unwrap();
    }
}

#[tokio::test]
async fn brokered_create_isolation_joins_six_real_api_uids_after_reservation() {
    // Causes: C1 a task-owned k3d cluster has the exact VAP and binding;
    // C2 each of the six original Kind tickets wins CAS but its Create is
    // never sent (crash-before-send); C3 Worker/group seal; C4 replay after
    // lost reservation response or Coordinator restart; C5 a controller
    // advances Job/DaemonSet resourceVersion. Effects: E1 C1+C2+C3 creates
    // six inert, exact-UID reservations with zero controller child Pods and
    // joins six Isolated tickets;
    // E2 C4 retains all UIDs; E3 the policy rejects exact dry-run DELETE and
    // label-removing UPDATE on every Kind without a stale-version false fail;
    // E4 after proof, task-owned finalizers, resources and policy are removed.
    // Rules L1=C1+C2+C3->E1+E3, L2=L1+C4->E2, L3=C5+dry-run patch->E3,
    // L4=teardown->E4. Delayed-original races use the separate API test.
    if std::env::var("AWAKEN_K8S_ISOLATION_E2E").as_deref() != Ok("1") {
        return;
    }
    assert!(require_live_cluster());
    use k8s_openapi::api::admissionregistration::v1::{
        ValidatingAdmissionPolicy, ValidatingAdmissionPolicyBinding,
    };
    use k8s_openapi::api::core::v1::{Namespace, Pod};
    use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams, Preconditions};
    use kube::{Api, ResourceExt};
    use std::io;
    use std::time::Duration;

    let context = kubectl(&["config", "current-context"]);
    assert!(
        context.status.success()
            && String::from_utf8_lossy(&context.stdout)
                .trim()
                .starts_with("k3d-"),
        "isolation live test requires a dedicated k3d context"
    );
    let client = awaken_worker_fleet_kubernetes::connect_control_client()
        .await
        .unwrap();
    // C0 incomplete aggregated discovery blocks Namespace finalization; E0
    // refuse before creating any of the six task-owned reservations.
    require_namespace_finalization_discovery(&client).await;
    let policies: Api<ValidatingAdmissionPolicy> = Api::all(client.clone());
    let bindings: Api<ValidatingAdmissionPolicyBinding> = Api::all(client.clone());
    assert!(
        policies
            .get_opt("awaken-create-isolation")
            .await
            .unwrap()
            .is_none()
            && bindings
                .get_opt("awaken-create-isolation")
                .await
                .unwrap()
                .is_none(),
        "test refuses to replace a deployment-owned admission policy"
    );
    let manifests =
        include_str!("../../../../../deploy/k3d/product-backend/create-control/resources.yaml");
    let docs = serde_saphyr::from_multiple::<serde_json::Value>(manifests).unwrap();
    let policy: ValidatingAdmissionPolicy = serde_json::from_value(
        docs.iter()
            .find(|doc| doc["kind"] == "ValidatingAdmissionPolicy")
            .unwrap()
            .clone(),
    )
    .unwrap();
    let binding: ValidatingAdmissionPolicyBinding = serde_json::from_value(
        docs.iter()
            .find(|doc| doc["kind"] == "ValidatingAdmissionPolicyBinding")
            .unwrap()
            .clone(),
    )
    .unwrap();

    let namespaces: Api<Namespace> = Api::all(client.clone());
    let mut policy_uid = None;
    let mut binding_uid = None;
    let mut sandbox = None;
    let mut control = None;
    let proof: Result<(), Box<dyn std::error::Error>> = async {
        policy_uid = policies.create(&PostParams::default(), &policy).await?.metadata.uid;
        binding_uid = bindings.create(&PostParams::default(), &binding).await?.metadata.uid;
        sandbox = Some(
            namespaces
                .create(&PostParams::default(), &generated_namespace("isolation-sandbox-"))
                .await?,
        );
        control = Some(
            namespaces
                .create(&PostParams::default(), &generated_namespace("isolation-control-"))
                .await?,
        );
        let sandbox_name = sandbox.as_ref().unwrap().name_any();
        let control_name = control.as_ref().unwrap().name_any();
        install_canonical_sandbox_network_policy(&sandbox_name)?;
        let ledger = K8sWorkerCreateLedger::new(
            client.clone(), &control_name, &sandbox_name, &sandbox_name, "provider-isolation")?;
        let (ledger_name, ledger_uid) = ledger
            .initialize(&sandbox_name, "worker", "worker-uid", "worker-uid")
            .await?;
        let anchor = K8sLedgerBinding {
            worker_namespace: &sandbox_name,
            worker_name: "worker",
            worker_uid: "worker-uid",
            worker_incarnation: "worker-uid",
            ledger_name: &ledger_name,
            ledger_uid: &ledger_uid,
            configuration_fingerprint: ledger.configuration_fingerprint(),
        };
        let request = CreateTicketRequest::new(
            "old-effect", "pods", &sandbox_name, "sandbox-pod", "a".repeat(64),
        )?;
        let mut requests = vec![request.clone()];
        for resource in PROTECTED_OTHER_KINDS {
            requests.push(CreateTicketRequest::new(
                "old-effect", resource, &sandbox_name,
                format!("reserved-{resource}"), "a".repeat(64),
            )?);
        }
        for ticket in &requests {
            if ledger.admit_request(anchor, ticket).await?.0 != TicketAdmission::Forward {
                return Err(io::Error::other(format!("first {resource} ticket did not win CAS", resource = ticket.resource)).into());
            }
        }
        ledger.seal_worker(anchor).await?;
        let _ = ledger.seal_group_and_join(anchor, "old-effect").await?;
        let mut isolated = None;
        // awaken-allow(wait): Kubernetes policy activation is an external physical fact; each retry reads the same durable ticket join.
        for _ in 0..60 {
            let joined = ledger.isolate_sealed_group_and_join(anchor, "old-effect").await?;
            if let RequestJoin::Complete(tickets) = joined {
                let observed = tickets.iter().filter_map(|ticket| {
                    if ticket.isolation_pending { return None; }
                    match &ticket.status {
                        awaken_container_contract::TicketStatus::Finished(TicketResult::Isolated { uid }) =>
                            Some((ticket.request.resource.clone(), uid.clone())),
                        _ => None,
                    }
                }).collect::<std::collections::BTreeMap<_, _>>();
                if observed.len() == requests.len() {
                    isolated = Some(observed);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let isolated = match isolated {
            Some(isolated) => isolated,
            None => {
                let mut details = Vec::new();
                for request in &requests {
                    let observation = if request.resource == "pods" {
                        let api: Api<Pod> = Api::namespaced(client.clone(), &sandbox_name);
                        api.get_opt(&request.name).await?
                            .map(|pod| serde_json::json!({
                                "uid":pod.metadata.uid,"phase":pod.status.and_then(|status| status.phase),
                                "finalizers":pod.metadata.finalizers
                            }))
                    } else {
                        use kube::core::DynamicObject;
                        let resource = protected_other_kind_resource(&request.resource);
                        let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), &sandbox_name, &resource);
                        api.get_opt(&request.name).await?.map(|object| {
                            let value = serde_json::to_value(object).expect("Kubernetes object is serializable");
                            serde_json::json!({
                                "uid":value["metadata"]["uid"],"finalizers":value["metadata"]["finalizers"],
                                "spec":value["spec"],"status":value["status"],
                                "immutable":value["immutable"],"type":value["type"]
                            })
                        })
                    };
                    details.push((request.resource.clone(), ledger.ticket_for(anchor, request).await?, observation));
                }
                return Err(io::Error::other(format!("six real Kind isolations did not join: {details:?}")).into());
            }
        };
        let uid = isolated.get("pods").ok_or_else(|| io::Error::other("Pod UID missing"))?;
        let pods: Api<Pod> = Api::namespaced(client.clone(), &sandbox_name);
        let observed = pods.get("sandbox-pod").await?;
        if observed.metadata.uid.as_deref() != Some(uid.as_str())
            || observed.spec.as_ref().and_then(|spec| spec.node_name.as_ref()).is_some()
            || observed.metadata.deletion_timestamp.is_some()
        {
            return Err(io::Error::other("isolated Pod changed its exact unbound UID").into());
        }
        let child_uids = ["jobs", "daemonsets"]
            .into_iter()
            .filter_map(|resource| isolated.get(resource))
            .collect::<Vec<_>>();
        if pods.list(&ListParams::default()).await?.items.iter().any(|pod| {
            pod.metadata.owner_references.as_ref().is_some_and(|owners| {
                owners.iter().any(|owner| child_uids.iter().any(|uid| owner.uid.as_str() == uid.as_str()))
            })
        }) {
            return Err(io::Error::other("inert controller reservation created a child Pod").into());
        }
        for resource in PROTECTED_OTHER_KINDS {
            use kube::core::DynamicObject;
            let api_resource = protected_other_kind_resource(resource);
            let api: Api<DynamicObject> =
                Api::namespaced_with(client.clone(), &sandbox_name, &api_resource);
            let name = format!("reserved-{resource}");
            let created = api.get(&name).await?;
            let uid = isolated.get(resource).ok_or_else(|| io::Error::other("Kind UID missing"))?;
            if created.metadata.uid.as_deref() != Some(uid.as_str()) {
                return Err(io::Error::other(format!("{resource} isolation changed UID")).into());
            }
            let denied = api
                .delete(
                    &name,
                    &DeleteParams {
                        dry_run: true,
                        preconditions: Some(Preconditions {
                            uid: Some(uid.clone()),
                            resource_version: None,
                        }),
                        ..DeleteParams::default()
                    },
                )
                .await;
            if !matches!(&denied, Err(kube::Error::Api(error))
                if matches!(error.code, 403 | 422)
                    && error.message.contains("awaken-create-isolation"))
            {
                return Err(io::Error::other(format!(
                    "{resource} selected dry-run DELETE was not denied by policy: {denied:?}"
                ))
                .into());
            }
            let denied = api
                .patch(
                    &name,
                    &PatchParams::default().dry_run(),
                    &Patch::Merge(serde_json::json!({
                        "metadata":{"labels":{(ISOLATION_LABEL):null}}
                    })),
                )
                .await;
            if !matches!(&denied, Err(kube::Error::Api(error))
                if matches!(error.code, 403 | 422)
                    && error.message.contains("awaken-create-isolation"))
            {
                return Err(io::Error::other(format!(
                    "{resource} label-removing UPDATE was not denied by policy: {denied:?}"
                ))
                .into());
            }
        }
        let replacement = K8sWorkerCreateLedger::new(
            client.clone(), &control_name, &sandbox_name, &sandbox_name, "provider-isolation")?;
        let replay = replacement.isolate_sealed_group_and_join(anchor, "old-effect").await?;
        if !matches!(replay, RequestJoin::Complete(tickets)
            if tickets.len() == requests.len()
                && tickets.iter().all(|ticket|
                    !ticket.isolation_pending &&
                    matches!(&ticket.status, awaken_container_contract::TicketStatus::Finished(TicketResult::Isolated { uid })
                        if isolated.get(&ticket.request.resource) == Some(uid))))
        {
            return Err(io::Error::other("isolation replay changed the joined UID").into());
        }

        // Guarded-claim terminal table. C1 the old brokered Worker created an
        // exact retained PVC but lost its Pod Create response; C2 its process
        // exit lets a reopened Coordinator seal the same ConfigMap ledger;
        // C3 the provider's inert Pod reservation wins same-name Create and
        // remains policy-protected; C4 the Session's original terminal owner
        // prepares, then supplies a distinct current disposal authorization;
        // C5 that disposal response or final Session receipt is lost. Effects:
        // E1 C1+C2+C3 yields a typed, non-runnable cleanup target and zero
        // early PVC DELETEs; E2 !C4 yields zero DELETEs; E3 C4 deletes only
        // the original claim UID while the guard blocks a late Pod; E4 C5
        // replays to proved absence under the same guard. Rules GC1 all
        // causes=>E1+E3+E4, GC2 !C2|!C3=>Unknown, GC3 !C4=>E2. Unit tables
        // cover CAS ordering, policy/UID drift and every negative projection.
        let claim_scope = format!("guardedclaim{}", uuid::Uuid::new_v4().simple());
        let claim_fence = effect_fence(&claim_scope, "create", "claim-worker", 1);
        let terminal_fence = effect_fence(&claim_scope, "terminal", "claim-worker", 2);
        let (claim_ledger_name, claim_ledger_uid) = ledger
            .initialize(&sandbox_name, "claim-worker", "claim-worker-uid", "claim-worker-uid")
            .await?;
        let claim_result = Arc::new(Mutex::new(None));
        let broker = Arc::new(ClaimOnlyBroker {
            client: client.clone(),
            ledger: ledger.clone(),
            sandbox_namespace: sandbox_name.clone(),
            ledger_name: claim_ledger_name,
            ledger_uid: claim_ledger_uid,
            scope: claim_scope.clone(),
            predecessor: claim_fence.clone(),
            created_claim: claim_result.clone(),
        });
        let volume = awaken_container_contract::K8sContinuationVolume {
            storage_class_name: None,
            size: "1Gi".into(),
        };
        let first_runtime = k8s_common::connect_with_clock(
            &sandbox_name,
            "127.0.0.1:1".parse().unwrap(),
        )
        .await?
        .with_continuation_volume(volume.clone())
        .with_create_transport(broker.clone())?;
        let first_provider = ContainerProvider::new(Arc::new(first_runtime), fixture_image())
            .with_clock(awaken_time::system_clock());
        let frozen = spec_with_command(
            &claim_scope,
            fixture_image(),
            vec!["sh".into(), "-c".into(), "sleep 30".into()],
        );
        let create_result = SandboxProvider::claim_for_effect(&first_provider, &frozen, awaken_provisioning_contract::SandboxEffect::Fenced { fence: &claim_fence, authority: &EffectAuthority::new(&claim_fence) }, awaken_provisioning_contract::SandboxRealizationIntent::Create)
        .await;
        if create_result.is_ok() {
            return Err(io::Error::other("GC1 injected lost Pod Create unexpectedly returned a handle").into());
        }
        let (claim_name, claim_uid) = claim_result
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| io::Error::other("GC1 original PVC was not admitted and created"))?;
        let claims: Api<k8s_openapi::api::core::v1::PersistentVolumeClaim> =
            Api::namespaced(client.clone(), &sandbox_name);
        if claims.get(&claim_name).await?.metadata.uid.as_deref() != Some(&claim_uid) {
            return Err(io::Error::other("GC1 original PVC UID was not retained").into());
        }
        drop(first_provider);

        let reopened = Arc::new(ClaimOnlyBroker {
            ledger: K8sWorkerCreateLedger::new(
                client.clone(),
                &control_name,
                &sandbox_name,
                &sandbox_name,
                "provider-isolation")?,
            ..(*broker).clone()
        });
        // GC4 cause: the requested Pod coordinate does not map to the one
        // retained PVC ticket. Effect: sealing may join that PVC, but it
        // cannot mint a foreign Pod guard or a terminal cleanup target.
        let wrong_coordinate = reopened
            .seal_predecessor(&claim_scope, &claim_fence, "awaken-foreign")
            .await?;
        if !matches!(wrong_coordinate, RequestJoin::Complete(tickets)
            if tickets.len() == 1 && tickets[0].request.resource == "persistentvolumeclaims")
        {
            return Err(io::Error::other("GC4 foreign Pod coordinate minted a guard").into());
        }
        let replacement_runtime = k8s_common::connect_with_clock(
            &sandbox_name,
            "127.0.0.1:1".parse().unwrap(),
        )
        .await?
        .with_continuation_volume(volume)
        .with_create_transport(reopened.clone())?;
        let replacement = ContainerProvider::new(Arc::new(replacement_runtime), fixture_image())
            .with_clock(awaken_time::system_clock());
        let authority = EffectAuthority::new(&terminal_fence);
        let target = SandboxProvider::prepare_terminal_for_effect(
            &replacement,
            &frozen,
            None,
            Some(&claim_fence),
            &terminal_fence,
            &authority,
        )
        .await?
        .ok_or_else(|| io::Error::other("GC1 live PVC requires a cleanup target"))?;
        let handle = target.handle();
        let (guard_name, guard_uid) = match handle.container_payload()?.runtime_handle {
            Some(awaken_sandbox_container::ContainerContinuationHandle::KubernetesGuardedClaim(
                guarded,
            )) if guarded.claim_uid() == claim_uid => (
                guarded.guard_name().to_owned(),
                guarded
                    .guard_uid()
                    .ok_or_else(|| io::Error::other("GC1 guard UID missing"))?
                    .to_owned(),
            ),
            other => {
                return Err(io::Error::other(format!(
                    "GC1 terminal target did not retain a guarded claim: {other:?}"
                ))
                .into());
            }
        };
        if claims.get(&claim_name).await?.metadata.deletion_timestamp.is_some() {
            return Err(io::Error::other("GC1 preparation deleted the original PVC").into());
        }
        let prepared = target
            .prepare_disposal_for_effect(&terminal_fence, &authority)
            .await?;
        let authorization = disposal_authorization(&prepared);
        if target
            .dispose_for_effect(&authorization, &EffectAuthority::denied())
            .await
            .is_ok()
        {
            return Err(io::Error::other("GC3 revoked authority deleted the claim").into());
        }
        if claims.get(&claim_name).await?.metadata.deletion_timestamp.is_some() {
            return Err(io::Error::other("GC3 revoked authority had a PVC effect").into());
        }
        target
            .dispose_for_effect(
                &authorization,
                &EffectAuthority::new(authorization.effect_fence()),
            )
            .await?;
        if claims.get_opt(&claim_name).await?.is_some() {
            return Err(io::Error::other("GC1 original PVC survived exact disposal").into());
        }
        let pods: Api<Pod> = Api::namespaced(client.clone(), &sandbox_name);
        let guard = pods.get(&guard_name).await?;
        if guard.metadata.uid.as_deref() != Some(&guard_uid)
            || guard.spec.as_ref().and_then(|spec| spec.node_name.as_ref()).is_some()
        {
            return Err(io::Error::other("GC1 retained Pod name lost its inert exact UID").into());
        }
        let foreign: Pod = serde_json::from_value(serde_json::json!({
            "metadata":{"name":guard_name},
            "spec":{"restartPolicy":"Never","containers":[{
                "name":"foreign","image":fixture_image(),"command":["sh","-c","true"]
            }]}
        }))?;
        let late = pods.create(&PostParams::default(), &foreign).await;
        if !matches!(late, Err(kube::Error::Api(error)) if error.code == 409) {
            return Err(io::Error::other("GC1 late same-name Pod bypassed the guard").into());
        }
        if SandboxProvider::prepare_terminal_for_effect(
            &replacement,
            &frozen,
            None,
            Some(&claim_fence),
            &terminal_fence,
            &authority,
        )
        .await?
        .is_some()
        {
            return Err(io::Error::other("GC1 cleanup response replay rebuilt a live target").into());
        }
        Ok(())
    }
    .await;

    let mut cleanup_errors = Vec::new();
    for (api, uid) in [
        ("binding", binding_uid.as_ref()),
        ("policy", policy_uid.as_ref()),
    ] {
        let params = DeleteParams::default().preconditions(Preconditions {
            uid: uid.cloned(),
            resource_version: None,
        });
        let result = if api == "binding" && binding_uid.is_some() {
            bindings
                .delete("awaken-create-isolation", &params)
                .await
                .map(|_| ())
        } else if api == "policy" && policy_uid.is_some() {
            policies
                .delete("awaken-create-isolation", &params)
                .await
                .map(|_| ())
        } else {
            Ok(())
        };
        if let Err(error) = result {
            cleanup_errors.push(format!("{api} removal: {error}"));
        }
    }
    if binding_uid.is_some() {
        match bindings.get_opt("awaken-create-isolation").await {
            Ok(None) => {}
            Ok(Some(_)) => cleanup_errors.push("task-owned admission binding remained".into()),
            Err(error) => cleanup_errors.push(format!("binding absence read: {error}")),
        }
    }
    if policy_uid.is_some() {
        match policies.get_opt("awaken-create-isolation").await {
            Ok(None) => {}
            Ok(Some(_)) => cleanup_errors.push("task-owned admission policy remained".into()),
            Err(error) => cleanup_errors.push(format!("policy absence read: {error}")),
        }
    }
    if let Some(sandbox) = &sandbox {
        let name = sandbox.name_any();
        let claims: Api<k8s_openapi::api::core::v1::PersistentVolumeClaim> =
            Api::namespaced(client.clone(), &name);
        match claims
            .list(&ListParams::default().labels("awaken-continuation=active"))
            .await
        {
            Ok(list) => {
                for claim in list.items {
                    let (Some(claim_name), Some(uid), Some(resource_version)) = (
                        claim.metadata.name,
                        claim.metadata.uid,
                        claim.metadata.resource_version,
                    ) else {
                        cleanup_errors.push("original retained PVC lacks cleanup identity".into());
                        continue;
                    };
                    if let Err(error) = claims
                        .delete(
                            &claim_name,
                            &DeleteParams::default().preconditions(Preconditions {
                                uid: Some(uid),
                                resource_version: Some(resource_version),
                            }),
                        )
                        .await
                    {
                        cleanup_errors.push(format!("original retained PVC deletion: {error}"));
                    }
                }
            }
            Err(error) => cleanup_errors.push(format!("original PVC cleanup scan: {error}")),
        }
        for resource in PROTECTED_OTHER_KINDS {
            use kube::core::DynamicObject;
            let api_resource = protected_other_kind_resource(resource);
            let api: Api<DynamicObject> =
                Api::namespaced_with(client.clone(), &name, &api_resource);
            let object_name = format!("reserved-{resource}");
            // awaken-allow(wait): a selected admission policy can outlive its binding
            // deletion briefly; only an exact UID-fenced DELETE and absence
            // close each task-owned reservation.
            for _ in 0..60 {
                let observed = match api.get_opt(&object_name).await {
                    Ok(None) => break,
                    Ok(Some(observed)) => observed,
                    Err(error) => {
                        cleanup_errors.push(format!("{resource} read: {error}"));
                        break;
                    }
                };
                if observed
                    .metadata
                    .labels
                    .as_ref()
                    .and_then(|labels| labels.get(ISOLATION_LABEL))
                    .map(String::as_str)
                    != Some("true")
                {
                    cleanup_errors.push(format!("{resource} has a foreign reservation label"));
                    break;
                }
                let Some(uid) = observed.metadata.uid.clone().filter(|uid| !uid.is_empty()) else {
                    cleanup_errors.push(format!("{resource} reservation has no exact UID"));
                    break;
                };
                let release = api
                    .patch(
                        &object_name,
                        &PatchParams::default(),
                        &Patch::Merge(serde_json::json!({
                            "metadata":{"finalizers":[]}
                        })),
                    )
                    .await;
                match release {
                    Ok(updated) if updated.metadata.uid.as_deref() == Some(uid.as_str()) => {}
                    Err(kube::Error::Api(error))
                        if matches!(error.code, 403 | 422)
                            && error.message.contains("awaken-create-isolation") =>
                    {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        continue;
                    }
                    other => {
                        cleanup_errors
                            .push(format!("{resource} exact finalizer release: {other:?}"));
                        break;
                    }
                }
                let result = api
                    .delete(
                        &object_name,
                        &DeleteParams::default().preconditions(Preconditions {
                            uid: Some(uid),
                            resource_version: None,
                        }),
                    )
                    .await;
                match result {
                    Ok(_) => {}
                    Err(kube::Error::Api(error)) if error.code == 404 => {}
                    Err(kube::Error::Api(error))
                        if matches!(error.code, 403 | 422)
                            && error.message.contains("awaken-create-isolation") =>
                    {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        continue;
                    }
                    Err(error) => {
                        cleanup_errors.push(format!("{resource} deletion: {error}"));
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            match api.get_opt(&object_name).await {
                Ok(None) => {}
                Ok(Some(_)) => cleanup_errors.push(format!("{resource} reservation remained")),
                Err(error) => {
                    cleanup_errors.push(format!("{resource} final absence read: {error}"))
                }
            }
        }
        let pods: Api<Pod> = Api::namespaced(client.clone(), &name);
        let reserved = match pods
            .list(&ListParams::default().labels(&format!("{ISOLATION_LABEL}=true")))
            .await
        {
            Ok(list) => list.items,
            Err(error) => {
                cleanup_errors.push(format!("inert Pod cleanup scan: {error}"));
                Vec::new()
            }
        };
        for reserved_pod in reserved {
            let Some(pod_name) = reserved_pod.metadata.name else {
                cleanup_errors.push("inert Pod has no exact name".into());
                continue;
            };
            // awaken-allow(wait): Admission policy removal must propagate before the
            // exact task-owned finalizer can be released. Both original and
            // claim-only reservations use this one cleanup owner.
            for _ in 0..60 {
                match pods.get_opt(&pod_name).await {
                    Ok(None) => break,
                    Ok(Some(mut pod)) => {
                        let uid = pod.metadata.uid.clone();
                        pod.metadata.finalizers = Some(Vec::new());
                        if pods
                            .replace(&pod_name, &PostParams::default(), &pod)
                            .await
                            .is_ok()
                        {
                            if let Err(error) = pods
                                .delete(
                                    &pod_name,
                                    &DeleteParams::default().preconditions(Preconditions {
                                        uid,
                                        resource_version: None,
                                    }),
                                )
                                .await
                            {
                                cleanup_errors.push(format!("Pod {pod_name} deletion: {error}"));
                            }
                            break;
                        }
                    }
                    Err(error) => {
                        cleanup_errors.push(format!("Pod {pod_name} read: {error}"));
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            // awaken-allow(wait): each exact name is absent before Namespace cleanup.
            for _ in 0..60 {
                match pods.get_opt(&pod_name).await {
                    Ok(None) => break,
                    Ok(Some(_)) => tokio::time::sleep(Duration::from_millis(250)).await,
                    Err(error) => {
                        cleanup_errors.push(format!("Pod {pod_name} absence read: {error}"));
                        break;
                    }
                }
            }
            match pods.get_opt(&pod_name).await {
                Ok(None) => {}
                Ok(Some(_)) => cleanup_errors.push(format!("Pod {pod_name} remained")),
                Err(error) => {
                    cleanup_errors.push(format!("Pod {pod_name} final absence read: {error}"))
                }
            }
        }
    }
    for object in [control, sandbox].into_iter().flatten() {
        let name = object.name_any();
        if let Err(error) = delete_namespace_by_uid(&namespaces, &object).await {
            cleanup_errors.push(format!("Namespace {name}: {error}"));
        }
        // awaken-allow(wait): Namespace finalization is an external Kubernetes fact, read back by the exact generated name.
        for _ in 0..60 {
            if matches!(namespaces.get_opt(&name).await, Ok(None)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        match namespaces.get_opt(&name).await {
            Ok(None) => {}
            Ok(Some(_)) => cleanup_errors.push(format!("Namespace {name} remained")),
            Err(error) => cleanup_errors.push(format!("Namespace {name} absence read: {error}")),
        }
    }
    assert!(
        cleanup_errors.is_empty(),
        "task-owned cleanup: {cleanup_errors:?}"
    );
    assert!(proof.is_ok(), "live isolation proof: {proof:?}");
}

#[tokio::test]
async fn original_create_guard_keeps_a_deleted_pod_visible_until_exact_release() {
    // Causes: C1 a broker-stamped original Pod is stored with a result guard;
    // C2 UID-fenced DELETE is accepted before ticket-result retention; C3 a
    // same-name Create races while C2 is still pending. Effects: E1 the old
    // UID and deletionTimestamp remain readable; E2 C3 gets a real API 409;
    // E3 exact finalizer release allows Pod and Namespace absence. Rules
    // OG1=C1+C2->E1, OG2=OG1+C3->E2, OG3=exact release->E3. This physical
    // control forbids treating a reservation winner as proof that a previous
    // original Pod was created and vanished.
    if std::env::var("AWAKEN_K8S_ISOLATION_E2E").as_deref() != Ok("1") {
        return;
    }
    assert!(require_live_cluster());
    use k8s_openapi::api::core::v1::{Namespace, Pod};
    use kube::api::{DeleteParams, PostParams, Preconditions};
    use kube::{Api, ResourceExt};
    use std::io;
    use std::time::Duration;

    let client = awaken_worker_fleet_kubernetes::connect_control_client()
        .await
        .unwrap();
    let namespaces: Api<Namespace> = Api::all(client.clone());
    // C0 incomplete aggregated discovery blocks Namespace finalization; E0
    // refuse before creating the guarded task-owned Pod.
    require_namespace_finalization_discovery(&client).await;
    let namespace = namespaces
        .create(
            &PostParams::default(),
            &generated_namespace("original-guard-"),
        )
        .await
        .unwrap();
    let name = namespace.name_any();
    let pods: Api<Pod> = Api::namespaced(client, &name);
    let mut original_uid = None;
    let proof: Result<(), Box<dyn std::error::Error>> = async {
        let original: Pod = serde_json::from_value(serde_json::json!({
            "metadata": {
                "name": "original-pod", "namespace": name,
                "finalizers": [awaken_container_contract::ORIGINAL_GUARD_FINALIZER]
            },
            "spec": {
                "automountServiceAccountToken": false,
                "restartPolicy": "Never",
                "schedulingGates": [{"name": "awaken.dev/create-isolation"}],
                "containers": [{"name": "inert", "image": "invalid.invalid/awaken-original-guard:never", "imagePullPolicy": "Never", "command": ["/bin/false"]}]
            }
        }))?;
        let created = pods.create(&PostParams::default(), &original).await?;
        let uid = created.metadata.uid.clone().ok_or_else(|| io::Error::other("original Pod has no UID"))?;
        original_uid = Some(uid.clone());
        pods.delete(
            "original-pod",
            &DeleteParams::default().preconditions(Preconditions {
                uid: Some(uid.clone()), resource_version: None,
            }),
        ).await?;
        let retained = pods.get("original-pod").await?;
        if retained.metadata.uid.as_deref() != Some(&uid)
            || retained.metadata.deletion_timestamp.is_none()
            || !retained.metadata.finalizers.as_ref().is_some_and(|values| values.iter().any(|value| value == awaken_container_contract::ORIGINAL_GUARD_FINALIZER))
        {
            return Err(io::Error::other("original Pod disappeared before its result guard was released").into());
        }
        match pods.create(&PostParams::default(), &original).await {
            Err(kube::Error::Api(error)) if error.code == 409 => {}
            other => return Err(io::Error::other(format!("same-name original Pod Create was not refused: {other:?}")).into()),
        }
        Ok(())
    }.await;

    let mut cleanup_errors = Vec::new();
    match pods.get_opt("original-pod").await {
        Ok(Some(mut pod)) => {
            if pod.metadata.uid != original_uid {
                cleanup_errors.push("original Pod UID changed before test cleanup".to_owned());
            } else {
                pod.metadata.finalizers = Some(Vec::new());
                if let Err(error) = pods
                    .replace("original-pod", &PostParams::default(), &pod)
                    .await
                {
                    cleanup_errors.push(format!("original guard release: {error}"));
                }
            }
        }
        Ok(None) if original_uid.is_some() => {
            cleanup_errors.push("guarded original Pod vanished before exact release".into())
        }
        Ok(None) => {}
        Err(error) => cleanup_errors.push(format!("original Pod cleanup read: {error}")),
    }
    // awaken-allow(wait): Kubernetes finalizer release is verified by the exact Pod name after a UID-fenced deletion request.
    for _ in 0..60 {
        match pods.get_opt("original-pod").await {
            Ok(None) => break,
            Ok(Some(_)) => tokio::time::sleep(Duration::from_millis(250)).await,
            Err(error) => {
                cleanup_errors.push(format!("original Pod absence read: {error}"));
                break;
            }
        }
    }
    match pods.get_opt("original-pod").await {
        Ok(None) => {}
        Ok(Some(_)) => cleanup_errors.push("original Pod remained after guard release".into()),
        Err(error) => cleanup_errors.push(format!("original Pod final absence read: {error}")),
    }
    if let Err(error) = delete_namespace_by_uid(&namespaces, &namespace).await {
        cleanup_errors.push(format!("original guard Namespace deletion: {error}"));
    }
    // awaken-allow(wait): The generated test Namespace must be absent before the physical control can report success.
    for _ in 0..60 {
        if matches!(namespaces.get_opt(&name).await, Ok(None)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    match namespaces.get_opt(&name).await {
        Ok(None) => {}
        Ok(Some(_)) => cleanup_errors.push("original guard Namespace remained".into()),
        Err(error) => {
            cleanup_errors.push(format!("original guard Namespace absence read: {error}"))
        }
    }
    assert!(
        cleanup_errors.is_empty(),
        "original guard cleanup: {cleanup_errors:?}"
    );
    assert!(proof.is_ok(), "original guard physical proof: {proof:?}");
}

#[tokio::test]
async fn guarded_deleting_original_runs_authorized_pvc_disposal() {
    // Causes: C1 real API-created original Pod/PVC UIDs and empty original
    // projection; C2 terminal Pod is UID-deleted but retained by its original
    // guard; C3 separately prepared current aggregate disposal authority.
    // LGS1=C1+C2 -> exact source and live claim remain readable; same-name
    // replacement is excluded. LGS2=LGS1+C3 -> the production Runtime claims
    // the existing guard, deletes only the original claim/Pod and observes
    // absence. No new finalizer, manual status or production-data receipt is
    // used. Failure retains the task-owned Namespace for diagnosis. An explicit
    // replay namespace resumes that SAME published UID set through this same
    // disposal path; it never makes a second fixture after an ambiguous result.
    if !require_live_cluster() {
        return;
    }
    use k8s_openapi::api::core::v1::{Namespace, PersistentVolumeClaim, Pod};
    use kube::api::{DeleteParams, PostParams, Preconditions};
    use kube::{Api, ResourceExt};
    let client = awaken_worker_fleet_kubernetes::connect_control_client()
        .await
        .unwrap();
    require_namespace_finalization_discovery(&client).await;
    let namespaces: Api<Namespace> = Api::all(client.clone());
    let replay_namespace = std::env::var("AWAKEN_K8S_GUARD_REPLAY_NAMESPACE").ok();
    let namespace = if let Some(name) = replay_namespace.as_deref() {
        assert!(
            name.starts_with("guard-succession-"),
            "replay is limited to this test's retained namespace"
        );
        namespaces.get(name).await.unwrap()
    } else {
        namespaces
            .create(
                &PostParams::default(),
                &generated_namespace("guard-succession-"),
            )
            .await
            .unwrap()
    };
    let ns = namespace.name_any();
    let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(client, &ns);
    // Fixed fixture coordinates exercise the backend's canonical managed-name
    // inverse; no production caller constructs a competing name algorithm.
    let pod_name = "awaken-original-source";
    let claim_name = "awc-original-source";
    let result:Result<(),Box<dyn std::error::Error>>=async {
        install_canonical_sandbox_network_policy(&ns)?;
        if replay_namespace.is_none() {
        let claim:PersistentVolumeClaim=serde_json::from_value(serde_json::json!({
            "metadata":{"name":claim_name,"namespace":ns},
            "spec":{"accessModes":["ReadWriteOnce"],"resources":{"requests":{"storage":"1Gi"}}}
        }))?;
        let claim=claims.create(&PostParams::default(),&claim).await?;
        let claim_uid=claim.metadata.uid.clone().ok_or_else(||std::io::Error::other("created claim has no UID"))?;
        let pod:Pod=serde_json::from_value(serde_json::json!({
            "metadata":{"name":pod_name,"namespace":ns,
                "finalizers":[awaken_container_contract::ORIGINAL_GUARD_FINALIZER],
                "annotations":{"awaken.dev/continuation-claim-uid":claim_uid,
                    "awaken.dev/sandbox-attempt":"live-original-attempt"}},
            "spec":{"automountServiceAccountToken":false,"restartPolicy":"Never",
                "securityContext":{"runAsNonRoot":true,"runAsUser":1000,"runAsGroup":1000,"fsGroup":1000,
                    "seccompProfile":{"type":"RuntimeDefault"}},
                "containers":[{"name":"agent","image":session_image(),"imagePullPolicy":"IfNotPresent",
                    "command":["/bin/sh","-c","printf retained-source > /mnt/session/marker"],
                    "resources":{"requests":{"cpu":"10m","memory":"16Mi"},"limits":{"cpu":"100m","memory":"64Mi"}},
                    "securityContext":{"allowPrivilegeEscalation":false,"readOnlyRootFilesystem":true,"capabilities":{"drop":["ALL"]}},
                    "volumeMounts":[{"name":"continuation-state","mountPath":"/mnt/session"}]}],
                "volumes":[{"name":"continuation-state","persistentVolumeClaim":{"claimName":claim_name}}]}
        }))?;
        let created=pods.create(&PostParams::default(),&pod).await?;
        let pod_uid=created.metadata.uid.clone().ok_or_else(||std::io::Error::other("created Pod has no UID"))?;
        let waited=kubectl(&["-n",&ns,"wait","--for=jsonpath={.status.phase}=Succeeded",&format!("pod/{pod_name}"),"--timeout=90s"]);
        if !waited.status.success() {return Err(std::io::Error::other(format!("real fixture did not complete: {}",String::from_utf8_lossy(&waited.stderr))).into());}
        let mut published=pods.get(pod_name).await?;
        if published.metadata.uid.as_deref()!=Some(pod_uid.as_str()) {return Err(std::io::Error::other("original Pod changed before publication").into());}
        let original_set=serde_json::json!({"namespace":ns,"pod_uid":pod_uid,"objects":[],
            "origin":{"kind":"created","attempt":"live-original-attempt"}});
        let annotations=published.metadata.annotations.get_or_insert_with(Default::default);
        annotations.insert("awaken.dev/projected-object-declaration".into(),"[]".into());
        annotations.insert("awaken.dev/projected-object-results".into(),original_set.to_string());
        pods.replace(pod_name,&PostParams::default(),&published).await?;
        }
        // Resume only identities already published by the original fixture.
        // A current GET never supplies a replacement original UID.
        let published=pods.get(pod_name).await?;
        let annotations=published.metadata.annotations.as_ref().ok_or_else(||std::io::Error::other("original publication missing"))?;
        let original_set:serde_json::Value=serde_json::from_str(annotations.get("awaken.dev/projected-object-results").ok_or_else(||std::io::Error::other("original results missing"))?)?;
        let pod_uid=original_set.get("pod_uid").and_then(|value|value.as_str()).ok_or_else(||std::io::Error::other("original result UID missing"))?.to_owned();
        let claim_uid=annotations.get("awaken.dev/continuation-claim-uid").ok_or_else(||std::io::Error::other("original claim UID missing"))?.clone();
        if published.metadata.uid.as_deref()!=Some(pod_uid.as_str()) {return Err(std::io::Error::other("published original UID changed").into());}
        // The fixture producer completes its declared handoff BEFORE any
        // handle/cleanup consumer runs. These fixed fixture identity values
        // are not discovered from GET; original Pod/PVC UIDs stay those of
        // the accepted Creates recorded above. A conflicting declaration fails.
        let mut complete=published.clone();
        let fields=complete.metadata.annotations.get_or_insert_with(Default::default);
        for (key,value) in [("awaken.dev/sandbox-adoption","live-fixture-adoption"),("awaken.dev/sandbox-realization","live-fixture-realization")] {
            if fields.get(key).is_some_and(|existing|existing!=value) {return Err(std::io::Error::other("fixture creation identity conflicts").into());}
            fields.insert(key.into(),value.into());
        }
        pods.replace(pod_name,&PostParams::default(),&complete).await?;
        let runtime=k8s_common::connect_with_clock(&ns,"127.0.0.1:1".parse().unwrap()).await?;
        let handle=runtime.handle_extra(pod_name).await?.ok_or_else(||std::io::Error::other("original handle missing"))?;
        if published.metadata.deletion_timestamp.is_none() {
            pods.delete(pod_name,&DeleteParams::default().preconditions(Preconditions {uid:Some(pod_uid.clone()),resource_version:None})).await?;
        }
        let retained=pods.get(pod_name).await?;let retained_claim=claims.get(claim_name).await?;
        if retained.metadata.uid.as_deref()!=Some(pod_uid.as_str())||retained.metadata.deletion_timestamp.is_none()
            ||retained_claim.metadata.uid.as_deref()!=Some(claim_uid.as_str())||retained_claim.metadata.deletion_timestamp.is_some() {
            return Err(std::io::Error::other("LGS1 original retained source/claim not exact and live").into());
        }
        let terminal=effect_fence(&ns,"terminal","live-retirement-actor",2);
        let authorization=disposal_authorization(&terminal);
        runtime.remove_exact_incarnation(pod_name,Some(&handle),&authorization,&EffectAuthority::new(authorization.effect_fence())).await?;
        if pods.get_opt(pod_name).await?.is_some()||claims.get_opt(claim_name).await?.is_some() {
            return Err(std::io::Error::other("LGS2 original physical objects remain").into());
        }
        Ok(())
    }.await;
    assert!(
        result.is_ok(),
        "live guard succession failed; retained test Namespace {ns}: {result:?}"
    );
    delete_namespace_by_uid(&namespaces, &namespace)
        .await
        .unwrap();
    let deleted = kubectl(&[
        "wait",
        "--for=delete",
        &format!("namespace/{ns}"),
        "--timeout=30s",
    ]);
    assert!(
        deleted.status.success(),
        "exact test Namespace deletion: {}",
        String::from_utf8_lossy(&deleted.stderr)
    );
}
