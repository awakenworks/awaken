use super::*;
use awaken_sandbox_container::k8s::{
    CreateTicketRequest, K8sLedgerBinding, K8sWorkerCreateLedger, RequestJoin, TicketAdmission,
    TicketResult,
};

fn generated_namespace(prefix: &str) -> k8s_openapi::api::core::v1::Namespace {
    serde_json::from_value(serde_json::json!({"metadata": {"generateName": prefix}})).unwrap()
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
    use awaken_sandbox_container::{
        ContainerCreateAttempt, ContainerEffectFence, ContainerPlan, ContainerRealizationContext,
        ContainerRealizationIntent, NetworkMode, RootfsPlan,
    };
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::{DeleteParams, ListParams, Preconditions};
    use kube::{Api, ResourceExt};

    let client = awaken_sandbox_container::k8s::connect_control_client()
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
    let fence = ContainerEffectFence::new(
        "live-create-projection",
        "worker",
        "worker-boot",
        1,
        u64::MAX,
    )
    .unwrap();
    let attempt = ContainerCreateAttempt::new(uuid::Uuid::new_v4().to_string());
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

    let client = awaken_sandbox_container::k8s::connect_control_client()
        .await
        .unwrap();
    let namespaces: Api<Namespace> = Api::all(client.clone());
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
    let first =
        K8sWorkerCreateLedger::new(client.clone(), &control_name, &sandbox_name, "provider-a")
            .unwrap();
    let replacement =
        K8sWorkerCreateLedger::new(client.clone(), &control_name, &sandbox_name, "provider-a")
            .unwrap();
    let (ledger_name, original_uid) = first
        .initialize(&sandbox_name, "worker", "worker-uid")
        .await
        .unwrap();
    let binding = K8sLedgerBinding {
        worker_namespace: &sandbox_name,
        worker_name: "worker",
        worker_uid: "worker-uid",
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
    let admission = admission.unwrap();
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
            .unwrap(),
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
        .initialize(&sandbox_name, "worker", "worker-uid")
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
async fn pod_create_isolation_joins_one_real_api_uid_after_reservation() {
    // Causes: C1 a task-owned, isolated k3d cluster has the exact VAP and
    // binding; C2 the original Pod ticket wins CAS but its Create is never
    // sent (the crash-before-send fault cut); C3 the Worker/group are sealed;
    // C4 the first reservation response or Coordinator process may be lost.
    // Effects: E1 C1+C2+C3 yields one real unscheduled Pod UID and one
    // Isolated ticket; E2 C4 replay retains that UID; E3 the active policy
    // rejects exact dry-run deletion; E4 test-owned policy and objects are
    // removed only after this no-original-request fixture is complete.
    // Rules L1=C1+C2+C3->E1+E3, L2=L1+C4->E2, L3=teardown->E4. The
    // same-name delayed-original race and policy-denial mutant are covered by
    // the separate fault cut and unit decision table.
    if std::env::var("AWAKEN_K8S_ISOLATION_E2E").as_deref() != Ok("1") {
        return;
    }
    assert!(require_live_cluster());
    use k8s_openapi::api::admissionregistration::v1::{
        ValidatingAdmissionPolicy, ValidatingAdmissionPolicyBinding,
    };
    use k8s_openapi::api::core::v1::{Namespace, Pod};
    use kube::api::{DeleteParams, PostParams, Preconditions};
    use kube::{Api, ResourceExt};
    use serde::Deserialize as _;
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
    let client = awaken_sandbox_container::k8s::connect_control_client()
        .await
        .unwrap();
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
    let docs = serde_yaml::Deserializer::from_str(manifests)
        .map(|doc| serde_yaml::Value::deserialize(doc).unwrap())
        .map(|doc| serde_json::to_value(doc).unwrap())
        .collect::<Vec<serde_json::Value>>();
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
        let ledger = K8sWorkerCreateLedger::new(
            client.clone(), &control_name, &sandbox_name, "provider-isolation",
        )?;
        let (ledger_name, ledger_uid) = ledger
            .initialize(&sandbox_name, "worker", "worker-uid")
            .await?;
        let anchor = K8sLedgerBinding {
            worker_namespace: &sandbox_name,
            worker_name: "worker",
            worker_uid: "worker-uid",
            ledger_name: &ledger_name,
            ledger_uid: &ledger_uid,
            configuration_fingerprint: ledger.configuration_fingerprint(),
        };
        let request = CreateTicketRequest::new(
            "old-effect", "pods", &sandbox_name, "sandbox-pod", "a".repeat(64),
        )?;
        if ledger.admit_request(anchor, &request).await? != TicketAdmission::Forward {
            return Err(io::Error::other("first Pod ticket did not win CAS").into());
        }
        ledger.seal_worker(anchor).await?;
        let _ = ledger.seal_group_and_join(anchor, "old-effect").await?;
        let mut isolated_uid = None;
        // awaken-wait: Kubernetes policy activation is an external physical fact; each retry reads the same durable ticket join.
        for _ in 0..60 {
            let joined = ledger.isolate_sealed_group_and_join(anchor, "old-effect").await?;
            if let RequestJoin::Complete(tickets) = joined
                && let Some(ticket) = tickets.into_iter().find(|ticket| ticket.request == request)
                && let awaken_sandbox_container::k8s::TicketStatus::Finished(
                    TicketResult::Isolated { uid },
                ) = ticket.status
                && !ticket.isolation_pending
            {
                isolated_uid = Some(uid);
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let uid = isolated_uid.ok_or_else(|| io::Error::other("real Pod isolation did not join"))?;
        let pods: Api<Pod> = Api::namespaced(client.clone(), &sandbox_name);
        let observed = pods.get("sandbox-pod").await?;
        if observed.metadata.uid.as_deref() != Some(&uid)
            || observed.spec.as_ref().and_then(|spec| spec.node_name.as_ref()).is_some()
            || observed.metadata.deletion_timestamp.is_some()
        {
            return Err(io::Error::other("isolated Pod changed its exact unbound UID").into());
        }
        let replay = ledger.isolate_sealed_group_and_join(anchor, "old-effect").await?;
        if !matches!(replay, RequestJoin::Complete(tickets)
            if tickets.len() == 1
                && tickets[0].status == awaken_sandbox_container::k8s::TicketStatus::Finished(TicketResult::Isolated { uid }))
        {
            return Err(io::Error::other("isolation replay changed the joined UID").into());
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
        let pods: Api<Pod> = Api::namespaced(client.clone(), &name);
        // awaken-wait: Admission policy removal must propagate before this test can release its exact inert Pod finalizer.
        for _ in 0..60 {
            match pods.get_opt("sandbox-pod").await {
                Ok(None) => break,
                Ok(Some(mut pod)) => {
                    let uid = pod.metadata.uid.clone();
                    pod.metadata.finalizers = Some(Vec::new());
                    if pods
                        .replace("sandbox-pod", &PostParams::default(), &pod)
                        .await
                        .is_ok()
                    {
                        if let Err(error) = pods
                            .delete(
                                "sandbox-pod",
                                &DeleteParams::default().preconditions(Preconditions {
                                    uid,
                                    resource_version: None,
                                }),
                            )
                            .await
                        {
                            cleanup_errors.push(format!("Pod deletion: {error}"));
                        }
                        break;
                    }
                }
                Err(error) => {
                    cleanup_errors.push(format!("Pod read: {error}"));
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        // awaken-wait: Kubernetes deletion completion is checked by exact Pod name and UID, not elapsed time alone.
        for _ in 0..60 {
            match pods.get_opt("sandbox-pod").await {
                Ok(None) => break,
                Ok(Some(_)) => tokio::time::sleep(Duration::from_millis(250)).await,
                Err(error) => {
                    cleanup_errors.push(format!("Pod absence read: {error}"));
                    break;
                }
            }
        }
        match pods.get_opt("sandbox-pod").await {
            Ok(None) => {}
            Ok(Some(_)) => cleanup_errors.push("task-owned isolation Pod remained".into()),
            Err(error) => cleanup_errors.push(format!("Pod final absence read: {error}")),
        }
    }
    for object in [control, sandbox].into_iter().flatten() {
        let name = object.name_any();
        if let Err(error) = delete_namespace_by_uid(&namespaces, &object).await {
            cleanup_errors.push(format!("Namespace {name}: {error}"));
        }
        // awaken-wait: Namespace finalization is an external Kubernetes fact, read back by the exact generated name.
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

    let client = awaken_sandbox_container::k8s::connect_control_client()
        .await
        .unwrap();
    let namespaces: Api<Namespace> = Api::all(client.clone());
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
                "finalizers": [awaken_sandbox_container::k8s::ORIGINAL_GUARD_FINALIZER]
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
            || !retained.metadata.finalizers.as_ref().is_some_and(|values| values.iter().any(|value| value == awaken_sandbox_container::k8s::ORIGINAL_GUARD_FINALIZER))
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
    // awaken-wait: Kubernetes finalizer release is verified by the exact Pod name after a UID-fenced deletion request.
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
    // awaken-wait: The generated test Namespace must be absent before the physical control can report success.
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
