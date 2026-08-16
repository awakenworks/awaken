//! Transactional realization of a Kubernetes Sandbox and retained claim.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExistingContinuationDecision {
    Preserve,
    ReapPod,
}

fn existing_continuation_decision(
    expected_claim: &str,
    pod_claim: Option<&str>,
    pod_claim_uid: Option<&str>,
    observed_claim_uid: Option<&str>,
) -> ExistingContinuationDecision {
    if pod_claim != Some(expected_claim) {
        return ExistingContinuationDecision::Preserve;
    }
    match (pod_claim_uid, observed_claim_uid) {
        (_, None) => ExistingContinuationDecision::ReapPod,
        (Some(expected), Some(observed)) if expected != observed => {
            ExistingContinuationDecision::ReapPod
        }
        _ => ExistingContinuationDecision::Preserve,
    }
}

async fn delete_exact_pod(pods: &Api<Pod>, observed: &Pod) -> Result<(), RuntimeError> {
    let name = observed
        .metadata
        .name
        .clone()
        .ok_or_else(|| backend("Kubernetes Sandbox Pod has no name"))?;
    let uid = observed
        .metadata
        .uid
        .clone()
        .ok_or_else(|| backend("Kubernetes Sandbox Pod has no UID"))?;
    let resource_version = observed
        .metadata
        .resource_version
        .clone()
        .ok_or_else(|| backend("Kubernetes Sandbox Pod has no resourceVersion"))?;
    match pods
        .delete(
            &name,
            &DeleteParams::default().preconditions(kube::api::Preconditions {
                uid: Some(uid),
                resource_version: Some(resource_version),
            }),
        )
        .await
    {
        Ok(_) => await_pod_deleted(pods, &name).await,
        Err(error) if api_not_found(&error) => Ok(()),
        Err(error) => Err(backend(error)),
    }
}

async fn reap_broken_continuation_pod(
    runtime: &K8sRuntime,
    pods: &Api<Pod>,
    pod_name: &str,
    claim_name: &str,
) -> Result<(), RuntimeError> {
    let Some(pod) = pods.get_opt(pod_name).await.map_err(backend)? else {
        return Ok(());
    };
    let pod_claim = pod
        .spec
        .as_ref()
        .and_then(|spec| spec.volumes.as_ref())
        .and_then(|volumes| {
            volumes.iter().find_map(|volume| {
                volume
                    .persistent_volume_claim
                    .as_ref()
                    .map(|claim| claim.claim_name.as_str())
            })
        });
    if pod_claim != Some(claim_name) {
        return Ok(());
    }
    let observed_claim_uid = match runtime.persistent_volume_claims().get(claim_name).await {
        Ok(claim) => Some(continuation::claim_uid(&claim)?),
        Err(error) if api_not_found(&error) => None,
        Err(error) => return Err(backend(error)),
    };
    if existing_continuation_decision(
        claim_name,
        pod_claim,
        continuation::bound_claim_uid(&pod),
        observed_claim_uid.as_deref(),
    ) == ExistingContinuationDecision::ReapPod
    {
        delete_exact_pod(pods, &pod).await?;
    }
    Ok(())
}

pub(super) async fn create(
    runtime: &K8sRuntime,
    id: &str,
    plan: &ContainerPlan,
) -> Result<String, RuntimeError> {
    if let Some(limit) = unenforceable_k8s_limit(&plan.limits) {
        return Err(RuntimeError::Backend(format!(
            "k8s cannot enforce a per-Pod `{limit}` limit (it is a node/kubelet \
             setting, not a Pod-spec field); refusing to place a `{limit}`-limited \
             spec on the k8s tier rather than silently dropping the cap"
        )));
    }
    let runtime_id = k8s_runtime_id(id)?;
    let pods = runtime.pods();
    let managed_pod_name = pod_name(&runtime_id);
    if continuation::claim_required(plan, runtime.continuation_volume.is_some()) {
        let claim_name = continuation_claim_name(&runtime_id);
        reap_broken_continuation_pod(runtime, &pods, &managed_pod_name, &claim_name).await?;
    }
    let claim_outcome = if continuation::claim_required(plan, runtime.continuation_volume.is_some())
    {
        let config = runtime
            .continuation_volume
            .as_ref()
            .expect("claim selector requires configured continuation storage");
        let mut claim = continuation::build_claim(&runtime_id, config)?;
        stamp_realization(&mut claim)?;
        Some(create_or_verify_with_status(&runtime.persistent_volume_claims(), &claim).await?)
    } else {
        None
    };
    let claim_uid = claim_outcome
        .as_ref()
        .map(|outcome| continuation::claim_uid(&outcome.object))
        .transpose()?;
    let claim_created = claim_outcome
        .as_ref()
        .is_some_and(|outcome| outcome.created);
    let mut created_pod_uid = None::<String>;
    let result = async {
        reap_terminal_pod(&pods, &managed_pod_name).await?;
        let cms = runtime.configmaps();
        for (i, bind) in content_binds(plan).iter().enumerate() {
            if crate::live_inputs::manages(bind) {
                continue;
            }
            let mut cm = build_configmap(
                &runtime_id,
                i,
                bind.content.as_deref(),
                bind.content_bytes.as_deref(),
                &runtime.owner,
            );
            stamp_realization(&mut cm)?;
            create_or_verify(&cms, &cm).await?;
        }
        let secrets = runtime.secrets();
        for (i, bind) in credential_binds(plan).iter().enumerate() {
            let mut secret = build_credential_secret(
                &runtime_id,
                i,
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
        if let Some(uid) = claim_uid.as_deref() {
            continuation::bind_claim_uid(&mut pod, uid);
        }
        stamp_pod_realization(&mut pod)?;
        let outcome = create_or_verify_with_status(&pods, &pod).await?;
        let was_created = outcome.created;
        let mut created = outcome.object;
        if was_created {
            created_pod_uid = created.metadata.uid.clone();
        }
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
            pods.replace(&name, &PostParams::default(), &created)
                .await
                .map_err(backend)?;
        }
        realization::await_pod_ready(&pods, &name).await?;
        if was_created {
            memory::project_snapshots(runtime, &name, plan).await?;
        }
        live_inputs::project_manifest(runtime, &name, plan).await?;
        Ok(name)
    }
    .await;

    if result.is_err() && claim_created {
        if let Some(expected_pod_uid) = created_pod_uid.as_deref()
            && let Ok(observed) = pods.get(&managed_pod_name).await
            && observed.metadata.uid.as_deref() == Some(expected_pod_uid)
        {
            let _ = delete_exact_pod(&pods, &observed).await;
        }
        // Preserve a claim only when an exact concurrent Pod already binds it.
        if let Some(uid) = claim_uid.as_deref() {
            let claim_is_in_use = pods
                .get_opt(&managed_pod_name)
                .await
                .map_err(backend)?
                .as_ref()
                .and_then(continuation::bound_claim_uid)
                == Some(uid);
            if !claim_is_in_use {
                continuation::delete_claim(
                    &runtime.persistent_volume_claims(),
                    &managed_pod_name,
                    uid,
                )
                .await?;
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_continuation_reference_decision_table() {
        /* Existing-realization recovery cause/effect table.
         * Causes: C1 the deterministic Pod references this realization's PVC;
         * C2 that PVC is absent/present; C3 the Pod has no legacy incarnation,
         * the exact current UID, or a different UID. Effects: E1 preserve a Pod
         * which may still own live Session data; E2 reap only an impossible Pod
         * projection before recreating the PVC. Rules: R1 !C1=>E1;
         * R2 C1+absent(C2)=>E2; R3 C1+present(C2)+legacy(C3)=>E1;
         * R4 C1+present(C2)+exact(C3)=>E1;
         * R5 C1+present(C2)+different(C3)=>E2. The subsequent create transaction
         * remains the sole PVC/Pod owner and retains UID/resourceVersion fencing.
         */
        use ExistingContinuationDecision::{Preserve, ReapPod};

        assert_eq!(
            existing_continuation_decision("awc-s", Some("other"), None, None),
            Preserve,
            "R1"
        );
        assert_eq!(
            existing_continuation_decision("awc-s", Some("awc-s"), None, None),
            ReapPod,
            "R2"
        );
        assert_eq!(
            existing_continuation_decision("awc-s", Some("awc-s"), None, Some("uid-1")),
            Preserve,
            "R3"
        );
        assert_eq!(
            existing_continuation_decision("awc-s", Some("awc-s"), Some("uid-1"), Some("uid-1")),
            Preserve,
            "R4"
        );
        assert_eq!(
            existing_continuation_decision(
                "awc-s",
                Some("awc-s"),
                Some("uid-old"),
                Some("uid-new")
            ),
            ReapPod,
            "R5"
        );
    }
}
