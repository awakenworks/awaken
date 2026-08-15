//! Transactional realization of a Kubernetes Sandbox and retained claim.

use super::*;

pub(super) async fn create(
    runtime: &K8sRuntime,
    id: &str,
    plan: &ContainerPlan,
) -> Result<String, RuntimeError> {
    admit_network(&plan.network, runtime.restricted_egress_policy)?;
    if let Some(limit) = unenforceable_k8s_limit(&plan.limits) {
        return Err(RuntimeError::Backend(format!(
            "k8s cannot enforce a per-Pod `{limit}` limit (it is a node/kubelet \
             setting, not a Pod-spec field); refusing to place a `{limit}`-limited \
             spec on the k8s tier rather than silently dropping the cap"
        )));
    }
    let runtime_id = k8s_runtime_id(id)?;
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
    let pods = runtime.pods();
    let managed_pod_name = pod_name(&runtime_id);
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
            let _ = pods
                .delete(
                    &managed_pod_name,
                    &DeleteParams::default().preconditions(kube::api::Preconditions {
                        uid: Some(expected_pod_uid.to_owned()),
                        resource_version: observed.metadata.resource_version,
                    }),
                )
                .await;
            let _ = await_pod_deleted(&pods, &managed_pod_name).await;
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
