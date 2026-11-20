//! Scope admission for the existing provider's resident process reaper.
use super::*;
use http::{Request, Response, StatusCode};
use kube::{Client, client::Body};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test]
async fn resident_reaper_requires_exact_protected_identity_before_any_exec() {
    // G0 exact Pod UID+name+namespace+both fingerprints+original guard ->
    // admit the physical operation (not a quiescence receipt). G1 missing
    // handle/fingerprint -> no API call. G2 foreign/deleting/unprotected/shared
    // namespace/missing claim -> refuse before exec. G3 failed API -> unknown,
    // never absence. One table negates each cause against the SAME projector,
    // G4 restored source uses its exact existing restore tuple/plan instead of
    // invented ordinary fingerprints. Foreign/incomplete restoration refuses.
    // client and production guard; no fixture may perform a mutation/stream.
    for rule in [
        "exact",
        "no-handle",
        "no-fingerprint",
        "uid",
        "name",
        "namespace",
        "guard",
        "deleting",
        "host-pid",
        "shared-pid",
        "adoption",
        "realization",
        "claim",
        "api",
        "restored",
        "restored-foreign",
        "restored-plan",
    ] {
        let mut runtime = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap());
        let mut pod = runtime.pod_for_effect("source", &crate::ephemeral_plan(), None);
        let name = pod.metadata.name.clone().unwrap();
        pod.metadata.namespace = Some(runtime.namespace.clone());
        pod.metadata.uid = Some("original-pod".into());
        pod.metadata.resource_version = Some("1".into());
        pod.metadata.finalizers = Some(vec![
            awaken_container_contract::ORIGINAL_GUARD_FINALIZER.into(),
        ]);
        let fingerprint: pc::SandboxRealizationFingerprint =
            serde_json::from_value(json!("fingerprint")).unwrap();
        let annotations = pod
            .metadata
            .annotations
            .get_or_insert_with(Default::default);
        annotations.insert(
            crate::SANDBOX_ADOPTION_ANNOTATION.into(),
            "fingerprint".into(),
        );
        annotations.insert(
            crate::SANDBOX_REALIZATION_ANNOTATION.into(),
            "fingerprint".into(),
        );
        pod.status = Some(k8s_openapi::api::core::v1::PodStatus {
            phase: Some("Running".into()),
            ..Default::default()
        });
        match rule {
            "uid" => pod.metadata.uid = Some("replacement".into()),
            "name" => pod.metadata.name = Some("foreign".into()),
            "namespace" => pod.metadata.namespace = Some("foreign".into()),
            "guard" => pod.metadata.finalizers = None,
            "deleting" => {
                pod.metadata.deletion_timestamp =
                    Some(serde_json::from_value(json!("2026-11-20T00:00:00Z")).unwrap())
            }
            "host-pid" => pod.spec.as_mut().unwrap().host_pid = Some(true),
            "shared-pid" => pod.spec.as_mut().unwrap().share_process_namespace = Some(true),
            "adoption" => {
                pod.metadata
                    .annotations
                    .as_mut()
                    .unwrap()
                    .insert(crate::SANDBOX_ADOPTION_ANNOTATION.into(), "foreign".into());
            }
            "realization" => {
                pod.metadata.annotations.as_mut().unwrap().insert(
                    crate::SANDBOX_REALIZATION_ANNOTATION.into(),
                    "foreign".into(),
                );
            }
            _ => {}
        }
        let restoration = pc::SandboxRestorationEvidence::from_exact_parts(
            "restore-effect",
            "generation",
            "checkpoint",
            "sha256:digest",
            "spec",
            "exclusions",
        )
        .unwrap();
        let restored = rule.starts_with("restored");
        if restored {
            let actual = if rule == "restored-foreign" {
                pc::SandboxRestorationEvidence::from_exact_parts(
                    "foreign-effect",
                    "generation",
                    "checkpoint",
                    "sha256:digest",
                    "spec",
                    "exclusions",
                )
                .unwrap()
            } else {
                restoration.clone()
            };
            super::super::realization::stamp_restoration(&mut pod, &actual);
            if rule != "restored-plan" {
                super::super::realization::stamp_restoration_plan(&mut pod, "exact-plan");
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let payload = if rule == "api" {
            json!({"kind":"Status","apiVersion":"v1","status":"Failure","reason":"ServiceUnavailable","message":"NotFound is not absence","code":503})
        } else {
            serde_json::to_value(pod).unwrap()
        };
        let service = tower::service_fn({
            let calls = calls.clone();
            move |request: Request<Body>| {
                let payload = payload.clone();
                let calls = calls.clone();
                async move {
                    assert_eq!(
                        request.method(),
                        http::Method::GET,
                        "no mutation or exec before identity admission"
                    );
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, std::io::Error>(
                        Response::builder()
                            .status(if rule == "api" {
                                StatusCode::SERVICE_UNAVAILABLE
                            } else {
                                StatusCode::OK
                            })
                            .body(Body::from(serde_json::to_vec(&payload).unwrap()))
                            .unwrap(),
                    )
                }
            }
        });
        runtime.clients.control = Client::new(service, "default");
        let handle =
            awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV2 {
                pod_uid: "original-pod".into(),
                claim_uid: if rule == "claim" {
                    Some("original-claim".into())
                } else {
                    None
                },
            };
        let expectation = ContainerObservationExpectation {
            purpose: pc::SandboxObservationPurpose::Recovery,
            container_id: &name,
            adoption_fingerprint: if rule == "no-fingerprint" || restored {
                None
            } else {
                Some(&fingerprint)
            },
            realization_fingerprint: if restored { None } else { Some(&fingerprint) },
            runtime_handle: if rule == "no-handle" {
                None
            } else {
                Some(&handle)
            },
            effect_fence: None,
            retained_attempt: None,
        };
        assert_eq!(
            runtime
                .protect_resident_source(
                    expectation,
                    if restored { Some(&restoration) } else { None }
                )
                .await
                .is_ok(),
            matches!(rule, "exact" | "restored"),
            "{rule}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            usize::from(!matches!(rule, "no-handle" | "no-fingerprint")),
            "{rule}"
        );
    }
}
