use super::*;
use awaken_sandbox_container::k8s::{
    CreateTicketRequest, K8sLedgerBinding, K8sWorkerCreateLedger, RequestJoin, TicketAdmission,
    TicketResult,
};

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
