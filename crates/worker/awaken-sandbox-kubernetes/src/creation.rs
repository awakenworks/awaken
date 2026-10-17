// @formal entrypoint resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence
// @formal entrypoint sandbox_continuation.ephemeral_never_allocates_or_references_a_claim
//! Transactional realization of a Kubernetes Sandbox and retained claim.

use super::{
    K8sRuntime, SANDBOX_ADOPTION_ANNOTATION, SANDBOX_ATTEMPT_ANNOTATION,
    SANDBOX_REALIZATION_ANNOTATION, api_not_found, backend, continuation, create_ledger,
    create_or_verify, k8s_runtime_id, live_inputs, memory, participants, pod_effect_fence,
    pod_owner_reference, projection, projection_verification, realization, rebuild_claim_admission,
    rebuild_continuation_expectation, stamp_pod_realization, stamp_realization,
};
use crate::package_realization::stamp_sandbox_release_annotations;
#[cfg(test)]
use crate::package_realization::{
    PACKAGE_REALIZATION_CONTRACT_ANNOTATION, RESOLVED_IMAGE_ANNOTATION, SANDBOX_SCOPE_ANNOTATION,
};
use awaken_provisioning_contract as pc;
use awaken_sandbox_container::{
    ContainerCreateAttempt, ContainerEffectFence, ContainerPlan, ContainerRealizationContext,
    ContainerRealizationIntent, RuntimeError,
};
use k8s_openapi::api::core::v1::Pod;
use kube::api::PostParams;

pub(super) async fn prepare(
    runtime: &K8sRuntime,
    context: &ContainerRealizationContext<'_>,
    plan: &ContainerPlan,
    realization_fingerprint: &pc::SandboxRealizationFingerprint,
) -> Result<String, RuntimeError> {
    if let Some(effect_fence) = context.effect_fence {
        runtime.validate_effect_fence(effect_fence)?;
    }
    let runtime_id = runtime.realization_runtime_id(context.scope)?;
    let pods = runtime.pods();
    let expected_rebuild = rebuild_continuation_expectation(
        context.intent,
        continuation::claim_name(&runtime_id, plan, runtime.continuation_volume.is_some()),
    )?;
    let claim_outcome = if let Some(expected) = expected_rebuild.as_ref() {
        let claims = runtime.persistent_volume_claims();
        let observed = match claims.get(&expected.claim_name).await {
            Ok(claim) => claim,
            Err(error) if api_not_found(&error) => {
                return Err(backend(
                    "Kubernetes rebuild source continuation PVC disappeared before Pod creation",
                ));
            }
            Err(error) => return Err(backend(error)),
        };
        rebuild_claim_admission(&observed, &expected.claim_uid)?;
        Some(observed)
    } else {
        super::existing_realization::create_or_recover_claim(runtime, context, plan).await?
    };
    let claim_uid = claim_outcome
        .as_ref()
        .map(continuation::claim_uid)
        .transpose()?;
    if let Some(effect_fence) = context.effect_fence {
        runtime.validate_effect_fence(effect_fence)?;
    }
    let mut pod = runtime.pod_for_effect(&runtime_id, plan, context.effect_fence);
    // Create the Pod before its projected ConfigMaps/Secrets. Kubernetes admits
    // missing references as a non-running Pod, giving every later participant
    // effect one UID-fenced recovery root instead of leaving unobservable
    // auxiliary objects when the Worker crashes between writes.
    stamp_sandbox_release_annotations(&mut pod, context.scope, &plan.image);
    if let Some(uid) = claim_uid.as_deref() {
        continuation::bind_claim_uid(&mut pod, uid);
    }
    runtime.stamp_effect_evidence(&mut pod, context, realization_fingerprint);
    participants::declare(&mut pod, &runtime_id, plan)?;
    stamp_pod_realization(&mut pod)?;
    let mut created = create_or_verify(&pods, &pod).await?;
    projection_verification::verify_pod_projection(&pod, &created, None)?;
    let name = created
        .metadata
        .name
        .clone()
        .ok_or_else(|| backend("created pod has no name"))?;
    if created
        .metadata
        .labels
        .as_ref()
        .and_then(|labels| labels.get(awaken_sandbox_container::engine::RUNTIME_OWNER_LABEL))
        != Some(&runtime.owner_id)
    {
        created
            .metadata
            .labels
            .get_or_insert_with(Default::default)
            .insert(
                awaken_sandbox_container::engine::RUNTIME_OWNER_LABEL.to_string(),
                runtime.owner_id.clone(),
            );
        created = pods
            .replace(&name, &PostParams::default(), &created)
            .await
            .map_err(backend)?;
    }
    let _pod_uid = created
        .metadata
        .uid
        .as_deref()
        .ok_or_else(|| backend("created pod has no UID"))?;
    Ok(name)
}

/// Reconstruct the same immutable Pod projection from its original stamped
/// attempt, then reuse the one Kubernetes Pod verifier before a replacement
/// process may turn a joined Create ticket into terminal cleanup evidence.
pub(super) fn verify_interrupted_pod_projection(
    runtime: &K8sRuntime,
    scope: &str,
    plan: &ContainerPlan,
    predecessor: &ContainerEffectFence,
    claim_uid: Option<&str>,
    observed: &Pod,
) -> Result<
    (
        ContainerEffectFence,
        pc::SandboxRealizationFingerprint,
        pc::SandboxRealizationFingerprint,
        ContainerCreateAttempt,
    ),
    RuntimeError,
> {
    let annotations = observed.metadata.annotations.as_ref();
    let original_fence = pod_effect_fence(annotations)?
        .filter(|fence| fence.same_effect_identity(predecessor))
        .ok_or_else(|| backend("interrupted Kubernetes Pod has another effect identity"))?;
    let stamped = |key: &str| {
        annotations
            .and_then(|values| values.get(key))
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| backend("interrupted Kubernetes Pod lacks realization evidence"))
    };
    let adoption: pc::SandboxRealizationFingerprint = serde_json::from_value(
        serde_json::Value::String(stamped(SANDBOX_ADOPTION_ANNOTATION)?.clone()),
    )
    .map_err(backend)?;
    let realization: pc::SandboxRealizationFingerprint = serde_json::from_value(
        serde_json::Value::String(stamped(SANDBOX_REALIZATION_ANNOTATION)?.clone()),
    )
    .map_err(backend)?;
    let attempt = ContainerCreateAttempt::new(stamped(SANDBOX_ATTEMPT_ANNOTATION)?.clone());
    let intent = ContainerRealizationIntent::Create;
    let context = ContainerRealizationContext::new(
        scope,
        &adoption,
        Some(&original_fence),
        &intent,
        &attempt,
    );
    let runtime_id = runtime.realization_runtime_id(scope)?;
    let mut expected = runtime.pod_for_effect(&runtime_id, plan, Some(&original_fence));
    stamp_sandbox_release_annotations(&mut expected, scope, &plan.image);
    if let Some(uid) = claim_uid {
        continuation::bind_claim_uid(&mut expected, uid);
    }
    runtime.stamp_effect_evidence(&mut expected, &context, &realization);
    if observed
        .metadata
        .annotations
        .as_ref()
        .is_some_and(|values| values.contains_key(participants::DECLARATION))
    {
        participants::declare(&mut expected, &runtime_id, plan)?;
    } else if participants::retained(observed)?.is_some() {
        return Err(backend("original projected results lack their declaration"));
    }
    stamp_pod_realization(&mut expected)?;
    projection_verification::verify_pod_projection(&expected, observed, None)?;
    Ok((original_fence, adoption, realization, attempt))
}

/// Idempotently finish every participant behind an already UID-fenced Pod.
/// Both first creation and response-loss recovery call this one edge; successful
/// return is therefore the only Ready+projection completion witness.
pub(super) async fn converge(
    runtime: &K8sRuntime,
    runtime_id: &str,
    pod_name: &str,
    expected_pod_uid: &str,
    context: &ContainerRealizationContext<'_>,
    plan: &ContainerPlan,
    realization_fingerprint: &pc::SandboxRealizationFingerprint,
) -> Result<(), RuntimeError> {
    let pod = runtime.pods().get(pod_name).await.map_err(backend)?;
    pod_owner_reference(&pod, expected_pod_uid)?;
    participants::realize(
        runtime,
        pod,
        runtime_id,
        plan,
        Some((context, realization_fingerprint)),
        None,
    )
    .await?;
    let wait = runtime.unschedulable_wait;
    let pods = runtime.pods();
    let ready = realization::await_pod_ready(&pods, pod_name, wait, runtime.clock.as_ref());
    match ready.await {
        Err(error @ RuntimeError::CapacityUnavailable(_)) => {
            // Hand the claim back only once this exact Pending Pod is gone; a
            // stale owner (fence) or a failed disposal returns its own error.
            if let Some(effect_fence) = context.effect_fence {
                runtime.validate_effect_fence(effect_fence)?;
            }
            let admit = || context.admit_current_effect().map_err(backend);
            dispose_unscheduled_pod(
                runtime,
                pod_name,
                expected_pod_uid,
                context.attempt.as_str(),
                &admit,
            )
            .await?;
            return Err(error);
        }
        result => result?,
    }
    if let Some(effect_fence) = context.effect_fence {
        runtime.validate_effect_fence(effect_fence)?;
    }
    if let Some(effect_fence) = context.effect_fence {
        if projection::projection_complete(runtime, pod_name, effect_fence).await? {
            return Ok(());
        }
        // The Agent/Hand command is still blocked on this same marker. Once
        // it exists, a replay cannot clear Files added by a later generation.
        // An interrupted first projection remains safe to retry while blocked.
        if !plan.memory_mounts.is_empty() {
            memory::project_snapshots(runtime, pod_name, plan, effect_fence).await?;
        }
        live_inputs::project_manifest(runtime, pod_name, plan).await?;
        projection::mark_projection_complete(runtime, pod_name, effect_fence).await
    } else {
        if !plan.memory_mounts.is_empty() {
            return Err(backend(
                "Kubernetes Memory convergence requires an Environment effect fence",
            ));
        }
        live_inputs::project_manifest(runtime, pod_name, plan).await
    }
}

/// Delete one exact unscheduled Pod incarnation through the UID- and
/// resourceVersion-fenced Pod disposal, releasing the broker's original-Create
/// guard this attempt's Create installed. Another incarnation under the stable
/// name, or an absent Pod, is not ours to delete.
pub(super) async fn dispose_unscheduled_pod(
    runtime: &K8sRuntime,
    name: &str,
    uid: &str,
    attempt: &str,
    admit: &(dyn Fn() -> Result<(), RuntimeError> + Sync),
) -> Result<(), RuntimeError> {
    admit()?;
    let observed = runtime.pods().get(name).await;
    // Even absence or a replacement UID is a response, not a grant to return
    // the capacity claim after the original authority was lost.
    admit()?;
    let pod = match observed {
        Err(error) if api_not_found(&error) => return Ok(()),
        observed => observed.map_err(backend)?,
    };
    let Some(version) = pod
        .metadata
        .resource_version
        .clone()
        .filter(|_| pod.metadata.uid.as_deref() == Some(uid))
    else {
        return Ok(());
    };
    let projected = participants::retained(&pod)?
        .ok_or_else(|| backend("unscheduled Pod has no original projected-object set"))?;
    participants::finish_projected(runtime, &projected, name, true, admit).await?;
    super::lifecycle::begin_exact_pod_delete(runtime, &pod, uid, &version, Some(attempt), admit)
        .await?;
    if create_ledger::original_guard_release_required(Some(&pod), true)? {
        create_ledger::release_original_guard(&runtime.pods(), name, uid, admit).await?;
    }
    super::lifecycle::await_exact_pod_deleted(runtime, name, uid, admit).await?;
    participants::finish_projected(runtime, &projected, name, false, admit).await
}

/// Realize or adopt the one exact checkpoint-restore target. Ordinary
/// Environment creation remains on the predecessor-fenced path above; this
/// edge is selected only by the canonical `SandboxRestoreRequest`.
pub(super) async fn restore_or_adopt(
    runtime: &K8sRuntime,
    id: &str,
    plan: &ContainerPlan,
    plan_fingerprint: &str,
    evidence: &pc::SandboxRestorationEvidence,
) -> Result<awaken_sandbox_container::RuntimeRestoreTarget, RuntimeError> {
    if awaken_sandbox_container::engine::restoration_plan_fingerprint(plan) != plan_fingerprint {
        return Err(backend("Kubernetes restore plan fingerprint mismatch"));
    }
    if !plan.memory_mounts.is_empty() {
        return Err(backend(
            "Kubernetes exact restore plan retained an independently governed Memory mount",
        ));
    }

    let runtime_id = k8s_runtime_id(id)?;
    let claim_outcome = if continuation::claim_required(plan, runtime.continuation_volume.is_some())
    {
        let config = runtime
            .continuation_volume
            .as_ref()
            .expect("claim selector requires configured continuation storage");
        let mut claim = continuation::build_claim(&runtime_id, config)?;
        realization::stamp_restoration(&mut claim, evidence);
        realization::stamp_restoration_plan(&mut claim, plan_fingerprint);
        stamp_realization(&mut claim)?;
        Some(
            realization::create_or_verify_with_status_exact(
                &runtime.persistent_volume_claims(),
                &claim,
                |expected, observed| {
                    projection_verification::verify_claim_projection(expected, observed, None)
                },
            )
            .await?,
        )
    } else {
        None
    };
    let claim_uid = claim_outcome
        .as_ref()
        .map(|outcome| continuation::claim_uid(&outcome.object))
        .transpose()?;

    let projected = participants::create_objects(
        runtime,
        &runtime_id,
        plan,
        &runtime.owner,
        None,
        Some((evidence, plan_fingerprint)),
        &|| Ok(()),
    )
    .await?;

    let mut pod = runtime.pod(&runtime_id, plan);
    stamp_sandbox_release_annotations(&mut pod, id, &plan.image);
    if let Some(uid) = claim_uid.as_deref() {
        continuation::bind_claim_uid(&mut pod, uid);
    }
    realization::stamp_restoration(&mut pod, evidence);
    realization::stamp_restoration_plan(&mut pod, plan_fingerprint);
    participants::declare(&mut pod, &runtime_id, plan)?;
    stamp_pod_realization(&mut pod)?;
    let outcome = realization::create_or_verify_with_status_exact(
        &runtime.pods(),
        &pod,
        |expected, observed| {
            projection_verification::verify_pod_projection(expected, observed, None)
        },
    )
    .await?;
    let name = outcome
        .object
        .metadata
        .name
        .clone()
        .ok_or_else(|| backend("created restore Pod has no name"))?;
    let pod_uid = outcome
        .object
        .metadata
        .uid
        .clone()
        .ok_or_else(|| backend("created restore Pod has no UID"))?;
    let projected = awaken_sandbox_container::KubernetesProjectedParticipants::new(
        runtime.namespace.clone(),
        pod_uid.clone(),
        projected,
        None,
        Some((evidence.clone(), plan_fingerprint.to_owned())),
    )
    .map_err(backend)?;
    participants::publish(
        runtime,
        outcome.object.clone(),
        &pod_uid,
        &projected,
        &|| Ok(()),
    )
    .await?;
    let published = runtime.pods().get(&name).await.map_err(backend)?;
    realization::verify_restoration(&published, evidence)?;
    if realization::restoration_plan_fingerprint(&published) != Some(plan_fingerprint) {
        return Err(backend(
            "restore projected-object CAS changed its original operation",
        ));
    }
    realization::transfer_runtime_owner(&runtime.pods(), published, &pod_uid, &runtime.owner_id)
        .await?;
    let wait = runtime.unschedulable_wait;
    realization::await_pod_ready(&runtime.pods(), &name, wait, runtime.clock.as_ref()).await?;
    live_inputs::project_manifest(runtime, &name, plan).await?;
    Ok(awaken_sandbox_container::RuntimeRestoreTarget {
        container_id: name,
        disposition: if outcome.created {
            pc::SandboxRestoreTargetDisposition::Created
        } else {
            pc::SandboxRestoreTargetDisposition::Recovered
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{build_configmap, pod_name};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    #[tokio::test]
    async fn initial_pod_create_checks_the_same_complete_security_projection() {
        // Causes: C1 the API accepts the exact Pod Create; C2 a mutating API
        // response changes the workload image but retains the desired digest,
        // effect and identity annotations. Effects: E1 C1+!C2 accepts one
        // exact reservation; E2 C1+C2 rejects before provider activation or a
        // durable handle; E3 both rows send exactly one Pod POST. Rules:
        // P1=C1+!C2->E1+E3; P2=C1+C2->E2+E3. The restore and missing-handle
        // callers use this same verifier, not separate annotation checks.
        use http::{Request, Response};
        use kube::client::Body;
        use std::sync::atomic::{AtomicUsize, Ordering};

        for changed in [false, true] {
            let posts = std::sync::Arc::new(AtomicUsize::new(0));
            let service = tower::service_fn({
                let posts = posts.clone();
                move |request: Request<Body>| {
                    let posts = posts.clone();
                    async move {
                        assert_eq!(request.method(), http::Method::POST);
                        posts.fetch_add(1, Ordering::SeqCst);
                        let bytes = request.into_body().collect_bytes().await.unwrap();
                        let mut object: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                        if changed {
                            object["spec"]["containers"][0]["image"] = "foreign-image".into();
                        }
                        object["metadata"]["uid"] = "pod-uid".into();
                        object["metadata"]["resourceVersion"] = "7".into();
                        Ok::<_, std::io::Error>(
                            Response::builder()
                                .status(201)
                                .body(Body::from(serde_json::to_vec(&object).unwrap()))
                                .unwrap(),
                        )
                    }
                }
            });
            let mut runtime = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap());
            runtime.clients.control = kube::Client::new(service, "default");
            let plan = super::super::ephemeral_plan();
            let fingerprint = serde_json::from_value(serde_json::json!("frozen-spec")).unwrap();
            let fence =
                ContainerEffectFence::new("create-effect", "worker", "worker-boot", 1, u64::MAX)
                    .unwrap();
            let attempt = awaken_sandbox_container::engine::fresh_create_attempt();
            let intent = ContainerRealizationIntent::Create;
            let context = ContainerRealizationContext::new(
                "scope",
                &fingerprint,
                Some(&fence),
                &intent,
                &attempt,
            );
            let result = prepare(&runtime, &context, &plan, &fingerprint).await;
            assert_eq!(result.is_ok(), !changed, "E1/E2 changed={changed}");
            if changed {
                let message = result.unwrap_err().to_string();
                assert!(message.contains("spec fields: containers"));
                assert!(!message.contains("foreign-image"), "values remain private");
            }
            assert_eq!(posts.load(Ordering::SeqCst), 1, "E3 changed={changed}");
        }
    }

    fn package_plan(image: &str) -> ContainerPlan {
        ContainerPlan {
            image: image.into(),
            command: vec!["sleep".into(), "30".into()],
            env: vec![("PRIVATE_RUNTIME_INPUT".into(), "never-annotate-me".into())],
            control_services: Default::default(),
            packages: Default::default(),
            binds: Vec::new(),
            outputs_volume: "/mnt/session/outputs".into(),
            network: awaken_sandbox_container::NetworkMode::Open,
            egress_identity: Default::default(),
            requests: pc::ResourceRequests::default(),
            limits: pc::ResourceLimits::default(),
            filesystem_continuity: pc::FilesystemContinuity::Retained,
            memory_mounts: Vec::new(),
            rootfs: awaken_sandbox_container::RootfsPlan::Image(image.into()),
        }
    }

    #[tokio::test]
    async fn sandbox_release_annotations_preserve_scope_and_only_the_resolved_image() {
        /* Release-correlation cause/effect decision table — SR1/SR2:
         * C1 an opaque Sandbox scope is not itself the Kubernetes-safe runtime
         * id; C2 the frozen Environment resolved one exact package image; C3 the
         * runtime plan also contains private env and image-pull inputs; C4 an
         * otherwise valid opaque scope exceeds the optional evidence budget. Effects:
         * E1 the Pod name remains the adapter-local id; E2 annotations retain the
         * exact original scope and resolved image; E3 no runtime env, registry
         * Secret, or adapter id becomes correlation metadata; E4 C4 preserves
         * Sandbox realization with absent correlation proof and no hashed identity
         * substitute. Rules: SR1 C1+C2=>E1+E2; SR2 C1+C2+C3=>E2+E3; SR3
         * C1+C2+C4=>E1+E4. A deployment observer establishes package provenance
         * only by equality with the BuildKit termination digest; this neutral
         * Sandbox seam does not create another build fact.
         */
        let scope = "sesn_fnv1a64:fixture-release-scope";
        let runtime_id = k8s_runtime_id(scope).unwrap();
        let pod_runtime_name = pod_name(&runtime_id);
        let image = format!(
            "registry.local/environments/awaken-packages@sha256:{}",
            "a".repeat(64)
        );
        let plan = package_plan(&image);
        let runtime = K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap())
            .with_image_pull_secrets(["registry-auth-secret".into()]);
        let mut pod = runtime.pod(&runtime_id, &plan);

        assert!(
            stamp_sandbox_release_annotations(&mut pod, scope, &plan.image),
            "SR1"
        );
        stamp_pod_realization(&mut pod).unwrap();

        assert_eq!(
            pod.metadata.name.as_deref(),
            Some(pod_runtime_name.as_str()),
            "SR1"
        );
        let annotations = pod.metadata.annotations.as_ref().unwrap();
        assert_eq!(
            annotations
                .get(SANDBOX_SCOPE_ANNOTATION)
                .map(String::as_str),
            Some(scope),
            "SR1 exact original identity"
        );
        assert_eq!(
            annotations
                .get(RESOLVED_IMAGE_ANNOTATION)
                .map(String::as_str),
            Some(image.as_str()),
            "SR1 exact frozen image"
        );
        assert_eq!(
            annotations.len(),
            4,
            "SR2 versioned correlation facts plus the existing realization digest"
        );
        assert_eq!(
            annotations
                .get(PACKAGE_REALIZATION_CONTRACT_ANNOTATION)
                .map(String::as_str),
            Some(crate::package_realization::K8S_PACKAGE_REALIZATION_CONTRACT_VERSION),
            "SR2 exact contract version"
        );
        assert!(
            annotations.contains_key("awaken.dev/realization-digest"),
            "SR2 production stamping remains authoritative"
        );
        assert!(
            annotations.values().all(|value| {
                !value.contains("never-annotate-me")
                    && !value.contains("registry-auth-secret")
                    && value != &runtime_id
            }),
            "SR2 secrets and the adapter-local identity stay out of annotations"
        );

        let overlong_scope = "s".repeat(4 * 1024 + 1);
        let overlong_runtime_id = k8s_runtime_id(&overlong_scope).unwrap();
        let mut overlong_pod = runtime.pod(&overlong_runtime_id, &plan);
        assert!(
            !stamp_sandbox_release_annotations(&mut overlong_pod, &overlong_scope, &plan.image),
            "SR3"
        );
        stamp_pod_realization(&mut overlong_pod).unwrap();
        let overlong_annotations = overlong_pod.metadata.annotations.as_ref().unwrap();
        assert_eq!(
            overlong_annotations.len(),
            1,
            "SR3 realization survives while release proof is absent"
        );
        assert!(
            overlong_annotations.contains_key("awaken.dev/realization-digest"),
            "SR3 no correlation hash replaces the original scope"
        );
    }

    #[test]
    fn projected_participants_are_owned_by_the_exact_pod_incarnation() {
        /* Projected-participant ownership cause/effect table.
         * Causes: C1 the API-observed Pod has name+UID; C2 the caller expects
         * that exact/different UID; C3 the Pod is live/terminating. Effects: E1
         * create one Pod OwnerReference carrying the exact UID; E2 reject before
         * ConfigMap/Secret writes. Rules: O1 exact(C2)+live(C3)=>E1;
         * O2 different(C2)=>E2; O3 terminating(C3)=>E2. Because the Pod is the
         * only owner, Kubernetes GC is the sole absent-Pod cleanup authority.
         */
        let mut pod = Pod {
            metadata: ObjectMeta {
                name: Some("awaken-session-1".into()),
                uid: Some("pod-uid-1".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let owner = pod_owner_reference(&pod, "pod-uid-1").expect("O1");
        assert_eq!(owner.name, "awaken-session-1", "O1");
        assert_eq!(owner.uid, "pod-uid-1", "O1");
        assert_eq!(owner.controller, Some(true), "O1");
        let configmap = build_configmap("session-1", 0, Some("value"), None, &Some(owner));
        assert_eq!(
            configmap.metadata.owner_references.unwrap()[0].uid,
            "pod-uid-1",
            "O1"
        );

        assert!(pod_owner_reference(&pod, "pod-uid-old").is_err(), "O2");
        pod.metadata.deletion_timestamp =
            serde_json::from_str("\"2026-08-29T00:00:00Z\"").expect("valid Kubernetes timestamp");
        assert!(pod_owner_reference(&pod, "pod-uid-1").is_err(), "O3");
    }
}
