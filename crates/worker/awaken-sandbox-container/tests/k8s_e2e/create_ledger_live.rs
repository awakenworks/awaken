use super::*;
use awaken_sandbox_container::k8s::{
    CreateTicketRequest, K8sLedgerBinding, K8sWorkerCreateLedger, RequestJoin, TicketAdmission,
    TicketResult,
};

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
    let create_namespace = |prefix: &str| {
        serde_json::from_value::<Namespace>(serde_json::json!({
            "metadata": {"generateName": prefix}
        }))
        .unwrap()
    };
    let sandbox = namespaces
        .create(&PostParams::default(), &create_namespace("ledger-sandbox-"))
        .await
        .unwrap();
    let control = namespaces
        .create(&PostParams::default(), &create_namespace("ledger-control-"))
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
        namespaces
            .delete(
                &object.name_any(),
                &DeleteParams::default().preconditions(Preconditions {
                    uid: object.metadata.uid,
                    resource_version: None,
                }),
            )
            .await
            .unwrap();
    }
}
