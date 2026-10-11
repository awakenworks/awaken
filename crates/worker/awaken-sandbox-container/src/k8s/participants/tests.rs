// @formal surface obligation=sandbox.fail_closed_policy_never_downgrades
// @formal reason: Original result/response-loss adapter decisions; these mocks do not qualify physical closure.
use super::super::ephemeral_plan;
use super::*;
use crate::{ContainerCreateAttempt, ContainerRealizationIntent};
use http::{Request, Response};
use kube::client::Body;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn original_projected_create_results_never_borrow_a_later_get_uid() {
    // Causes: C1 original successful POST or bare409/currentGET; C2 original
    // UID, missing/blank UID, changed owner, or backend failure. Effects: R1
    // original201+exactowner+UID -> retained identity; R2 bare409+same digest
    // -> Unknown, irrespective of currentGET UID; R3 each bad C2 -> no retained
    // result. R2 is the direct-mode response-loss boundary: its local join gate
    // has no durable original UID. Broker replay's original ticket+nonce+UID
    // positive case is tested at the same Coordinator create route.
    let runtime = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap());
    let pod = Pod {
        metadata: ObjectMeta {
            name: Some("awaken-subject".into()),
            uid: Some("pod-uid".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    let owner = Some(pod_owner_reference(&pod, "pod-uid").unwrap());
    for row in [
        "original",
        "bare409",
        "missing",
        "blank",
        "owner",
        "api-error",
    ] {
        let posts = Arc::new(AtomicUsize::new(0));
        let mut desired = build_configmap("subject", 0, Some("private-input"), None, &owner);
        let fingerprint = serde_json::from_value(json!("frozen-spec")).unwrap();
        let attempt = ContainerCreateAttempt::new("attempt");
        let intent = ContainerRealizationIntent::Create;
        let context =
            ContainerRealizationContext::new("scope", &fingerprint, None, &intent, &attempt);
        runtime.stamp_effect_evidence(&mut desired, &context, &fingerprint);
        stamp_realization(&mut desired).unwrap();
        let encoded = serde_json::to_value(&desired).unwrap();
        let service = tower::service_fn({
            let posts = posts.clone();
            move |request: Request<Body>| {
                let posts = posts.clone();
                let mut object = encoded.clone();
                async move {
                    let post = request.method() == http::Method::POST;
                    if post {
                        posts.fetch_add(1, Ordering::SeqCst);
                    }
                    let (code, value) = if row == "api-error" {
                        (
                            500,
                            json!({"kind":"Status","apiVersion":"v1","code":500,"reason":"Unavailable"}),
                        )
                    } else if row == "bare409" && post {
                        (
                            409,
                            json!({"kind":"Status","apiVersion":"v1","code":409,"reason":"AlreadyExists"}),
                        )
                    } else {
                        object["metadata"]["uid"] = "original-uid".into();
                        match row {
                            "missing" => {
                                object["metadata"].as_object_mut().unwrap().remove("uid");
                            }
                            "blank" => object["metadata"]["uid"] = " ".into(),
                            "owner" => {
                                object["metadata"]["ownerReferences"][0]["uid"] = "foreign".into()
                            }
                            _ => {}
                        };
                        (if post { 201 } else { 200 }, object)
                    };
                    Ok::<_, std::io::Error>(
                        Response::builder()
                            .status(code)
                            .body(Body::from(serde_json::to_vec(&value).unwrap()))
                            .unwrap(),
                    )
                }
            }
        });
        let api: Api<ConfigMap> = Api::namespaced(kube::Client::new(service, "default"), "default");
        let result = original_result(&api, &desired).await.and_then(|object| {
            KubernetesProjectedParticipants::new(
                "default".into(),
                "pod-uid".into(),
                vec![object],
                Some("attempt".into()),
                None,
            )
            .map_err(backend)
        });
        assert_eq!(result.is_ok(), row == "original", "R1-R3/{row}");
        assert_eq!(posts.load(Ordering::SeqCst), 1, "no duplicate POST/{row}");
    }
}

#[tokio::test]
async fn direct_complete_original_projection_replays_without_new_creates() {
    // R4: original complete empty result handoff + same original
    // attempt -> reuse exactly those UIDs, no new POST/GET/PUT. R5 missing
    // original declaration -> Unknown rather than borrowing today's empty plan.
    // R6 another Pod UID/attempt -> reject copied handoff; R7 authority denied
    // -> no replay publication. This is an original-result replay, not a GET
    // that repairs direct response loss before original UID persistence.
    let mut runtime = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap());
    runtime.clients.control = kube::Client::new(
        tower::service_fn(|_request: Request<Body>| async {
            panic!("complete replay must not send a new Kubernetes request");
            #[allow(unreachable_code)]
            Ok::<_, std::io::Error>(Response::new(Body::empty()))
        }),
        "default",
    );
    let plan = ephemeral_plan();
    let fingerprint = serde_json::from_value(json!("frozen-spec")).unwrap();
    let attempt = ContainerCreateAttempt::new("attempt");
    let intent = ContainerRealizationIntent::Create;
    let context = ContainerRealizationContext::new("scope", &fingerprint, None, &intent, &attempt);
    let mut pod = Pod {
        metadata: ObjectMeta {
            name: Some("awaken-subject".into()),
            uid: Some("pod-uid".into()),
            resource_version: Some("1".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    runtime.stamp_effect_evidence(&mut pod, &context, &fingerprint);
    declare(&mut pod, "subject", &plan).unwrap();
    let original = KubernetesProjectedParticipants::new(
        "default".into(),
        "pod-uid".into(),
        Vec::new(),
        Some("attempt".into()),
        None,
    )
    .unwrap();
    pod.metadata
        .annotations
        .as_mut()
        .unwrap()
        .insert(RESULTS.into(), serde_json::to_string(&original).unwrap());
    assert_eq!(
        realize(
            &runtime,
            pod.clone(),
            "subject",
            &plan,
            Some((&context, &fingerprint)),
            None
        )
        .await
        .unwrap(),
        original,
        "R4"
    );
    for row in ["missing-declaration", "foreign-pod", "foreign-attempt"] {
        let mut changed = pod.clone();
        match row {
            "missing-declaration" => {
                changed
                    .metadata
                    .annotations
                    .as_mut()
                    .unwrap()
                    .remove(DECLARATION);
            }
            "foreign-pod" => changed.metadata.uid = Some("new-pod".into()),
            "foreign-attempt" => {
                changed
                    .metadata
                    .annotations
                    .as_mut()
                    .unwrap()
                    .insert(SANDBOX_ATTEMPT_ANNOTATION.into(), "new-attempt".into());
            }
            _ => unreachable!(),
        }
        assert!(
            realize(
                &runtime,
                changed,
                "subject",
                &plan,
                Some((&context, &fingerprint)),
                None
            )
            .await
            .is_err(),
            "R5/R6/{row}"
        );
    }
    let fence = pc::SandboxEffectFence::new("create", "worker", "boot", 1, u64::MAX - 1).unwrap();
    let authority = pc::test_support::EffectAuthority::denied();
    let denied =
        ContainerRealizationContext::new("scope", &fingerprint, Some(&fence), &intent, &attempt)
            .with_effect(awaken_container_contract::ContainerEffect::Fenced {
                fence: &fence,
                authority: &authority,
            });
    assert!(
        realize(
            &runtime,
            pod,
            "subject",
            &plan,
            Some((&denied, &fingerprint)),
            None
        )
        .await
        .is_err(),
        "R7"
    );
}
