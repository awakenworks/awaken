//! Transactional realization of a Kubernetes Sandbox and retained claim.

use super::*;
use crate::k8s_package_realization::stamp_sandbox_release_annotations;
#[cfg(test)]
use crate::k8s_package_realization::{
    PACKAGE_REALIZATION_CONTRACT_ANNOTATION, RESOLVED_IMAGE_ANNOTATION, SANDBOX_SCOPE_ANNOTATION,
};

pub(super) async fn create(
    runtime: &K8sRuntime,
    context: &ContainerRealizationContext<'_>,
    plan: &ContainerPlan,
    realization_fingerprint: &pc::SandboxRealizationFingerprint,
) -> Result<String, RuntimeError> {
    if let Some(effect_fence) = context.effect_fence {
        crate::runtime::validate_runtime_effect_fence(effect_fence)?;
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
        runtime.create_or_recover_claim(context, plan).await?
    };
    let claim_uid = claim_outcome
        .as_ref()
        .map(continuation::claim_uid)
        .transpose()?;
    if let Some(effect_fence) = context.effect_fence {
        crate::runtime::validate_runtime_effect_fence(effect_fence)?;
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
    stamp_pod_realization(&mut pod)?;
    let mut created = create_or_verify(&pods, &pod).await?;
    let name = created
        .metadata
        .name
        .clone()
        .ok_or_else(|| backend("created pod has no name"))?;
    if created
        .metadata
        .labels
        .as_ref()
        .and_then(|labels| labels.get(crate::RUNTIME_OWNER_LABEL))
        != Some(&runtime.owner_id)
    {
        created
            .metadata
            .labels
            .get_or_insert_with(Default::default)
            .insert(
                crate::RUNTIME_OWNER_LABEL.to_string(),
                runtime.owner_id.clone(),
            );
        created = pods
            .replace(&name, &PostParams::default(), &created)
            .await
            .map_err(backend)?;
    }
    let pod_uid = created
        .metadata
        .uid
        .as_deref()
        .ok_or_else(|| backend("created pod has no UID"))?;
    converge(
        runtime,
        &runtime_id,
        &name,
        pod_uid,
        context,
        plan,
        realization_fingerprint,
    )
    .await?;
    Ok(name)
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
    let pod_owner = Some(pod_owner_reference(&pod, expected_pod_uid)?);
    let cms = runtime.configmaps();
    for (index, bind) in content_binds(plan).iter().enumerate() {
        if crate::live_inputs::manages(bind) {
            continue;
        }
        let mut configmap = build_configmap(
            runtime_id,
            index,
            bind.content.as_deref(),
            bind.content_bytes.as_deref(),
            &pod_owner,
        );
        runtime.stamp_effect_evidence(&mut configmap, context, realization_fingerprint);
        stamp_realization(&mut configmap)?;
        if let Some(effect_fence) = context.effect_fence {
            crate::runtime::validate_runtime_effect_fence(effect_fence)?;
        }
        let configmap = create_or_verify(&cms, &configmap).await?;
        verify_projected_content(
            &configmap.metadata,
            pod_name,
            expected_pod_uid,
            context.attempt.as_str(),
        )?;
    }
    let secrets = runtime.secrets();
    for (index, bind) in credential_binds(plan).iter().enumerate() {
        let mut secret = build_credential_secret(
            runtime_id,
            index,
            credential_key(bind),
            bind.secret_content
                .as_ref()
                .expect("credential bind has secret bytes")
                .expose(),
            &pod_owner,
        );
        runtime.stamp_effect_evidence(&mut secret, context, realization_fingerprint);
        stamp_realization(&mut secret)?;
        if let Some(effect_fence) = context.effect_fence {
            crate::runtime::validate_runtime_effect_fence(effect_fence)?;
        }
        let secret = create_or_verify(&secrets, &secret).await?;
        verify_projected_content(
            &secret.metadata,
            pod_name,
            expected_pod_uid,
            context.attempt.as_str(),
        )?;
    }
    realization::await_pod_ready(&runtime.pods(), pod_name).await?;
    if let Some(effect_fence) = context.effect_fence {
        crate::runtime::validate_runtime_effect_fence(effect_fence)?;
    }
    if !plan.memory_mounts.is_empty() {
        let effect_fence = context.effect_fence.ok_or_else(|| {
            backend("Kubernetes Memory convergence requires an Environment effect fence")
        })?;
        if memory::projection_complete(runtime, pod_name, effect_fence).await? {
            return Ok(());
        }
        // The Agent/Hand command is still blocked on the projection marker.
        // Replaying an interrupted projection is therefore safe: no workload
        // can observe or mutate the tree before both Memory and initial Files
        // have converged under this exact physical-effect fence.
        memory::project_snapshots(runtime, pod_name, plan, effect_fence).await?;
        live_inputs::project_manifest(runtime, pod_name, plan).await?;
        memory::mark_projection_complete(runtime, pod_name, effect_fence).await
    } else {
        live_inputs::project_manifest(runtime, pod_name, plan).await
    }
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
) -> Result<crate::RuntimeRestoreTarget, RuntimeError> {
    if crate::restoration_plan_fingerprint(plan) != plan_fingerprint {
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
                restore::verify_claim_projection,
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

    let cms = runtime.configmaps();
    for (index, bind) in content_binds(plan).iter().enumerate() {
        if crate::live_inputs::manages(bind) {
            continue;
        }
        let mut configmap = build_configmap(
            &runtime_id,
            index,
            bind.content.as_deref(),
            bind.content_bytes.as_deref(),
            &runtime.owner,
        );
        stamp_realization(&mut configmap)?;
        create_or_verify(&cms, &configmap).await?;
    }
    let secrets = runtime.secrets();
    for (index, bind) in credential_binds(plan).iter().enumerate() {
        let mut secret = build_credential_secret(
            &runtime_id,
            index,
            credential_key(bind),
            bind.secret_content
                .as_ref()
                .expect("credential bind has secret bytes")
                .expose(),
            &runtime.owner,
        );
        stamp_realization(&mut secret)?;
        create_or_verify(&secrets, &secret).await?;
    }

    let mut pod = runtime.pod(&runtime_id, plan);
    stamp_sandbox_release_annotations(&mut pod, id, &plan.image);
    if let Some(uid) = claim_uid.as_deref() {
        continuation::bind_claim_uid(&mut pod, uid);
    }
    realization::stamp_restoration(&mut pod, evidence);
    realization::stamp_restoration_plan(&mut pod, plan_fingerprint);
    stamp_pod_realization(&mut pod)?;
    let outcome = realization::create_or_verify_with_status_exact(
        &runtime.pods(),
        &pod,
        restore::verify_pod_projection,
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
    realization::transfer_runtime_owner(
        &runtime.pods(),
        outcome.object,
        &pod_uid,
        &runtime.owner_id,
    )
    .await?;
    realization::await_pod_ready(&runtime.pods(), &name).await?;
    live_inputs::project_manifest(runtime, &name, plan).await?;
    Ok(crate::RuntimeRestoreTarget {
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

    fn package_plan(image: &str) -> ContainerPlan {
        ContainerPlan {
            image: image.into(),
            command: vec!["sleep".into(), "30".into()],
            env: vec![("PRIVATE_RUNTIME_INPUT".into(), "never-annotate-me".into())],
            control_services: Default::default(),
            packages: Default::default(),
            binds: Vec::new(),
            outputs_volume: "/mnt/session/outputs".into(),
            network: crate::NetworkMode::Open,
            egress_identity: Default::default(),
            requests: pc::ResourceRequests::default(),
            limits: pc::ResourceLimits::default(),
            filesystem_continuity: pc::FilesystemContinuity::Retained,
            memory_mounts: Vec::new(),
            rootfs: crate::RootfsPlan::Image(image.into()),
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
        let scope = "sesn_fnv1a64:mission-call-of-duty";
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
            Some(crate::k8s_package_realization::K8S_PACKAGE_REALIZATION_CONTRACT_VERSION),
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
