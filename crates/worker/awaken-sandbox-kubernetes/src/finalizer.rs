// @formal entrypoint sandbox.k8s_process_observation_requires_complete_exit
//! One Kubernetes CAS loop for retaining or releasing an exact finalizer.

use awaken_container_contract::RuntimeError;
use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use kube::api::PostParams;

use super::backend;
use super::error::{api_conflict, api_not_found};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ExactFinalizerAdmission {
    Retain,
    Remove,
    Complete,
}

/// Update one finalizer through the exact Pod's resourceVersion. Domain owners
/// supply admission and metadata cleanup; this function alone owns GET/replace,
/// conflict retry and post-write verification.
pub(super) async fn update_exact_pod_finalizer(
    pods: &Api<Pod>,
    name: &str,
    finalizer: &str,
    require_present: bool,
    checkpoint: &(dyn Fn() -> Result<(), RuntimeError> + Sync),
    admit: &(dyn Fn(&Pod) -> Result<ExactFinalizerAdmission, RuntimeError> + Sync),
    prepare: &(dyn Fn(&mut Pod) + Sync),
) -> Result<(), RuntimeError> {
    loop {
        checkpoint()?;
        let observed = match pods.get(name).await {
            Ok(pod) => pod,
            Err(error) if api_not_found(&error) && !require_present => return Ok(()),
            Err(error) => return Err(backend(error)),
        };
        checkpoint()?;
        let admission = admit(&observed)?;
        if admission == ExactFinalizerAdmission::Complete {
            return Ok(());
        }
        if observed.metadata.resource_version.is_none() {
            return Err(backend(
                "Kubernetes Pod has no resourceVersion for finalizer update",
            ));
        }
        let mut replacement = observed;
        let mut finalizers = replacement.metadata.finalizers.take().unwrap_or_default();
        match admission {
            ExactFinalizerAdmission::Remove => {
                finalizers.retain(|candidate| candidate != finalizer)
            }
            ExactFinalizerAdmission::Retain => {
                if !finalizers.iter().any(|candidate| candidate == finalizer) {
                    finalizers.push(finalizer.into());
                }
            }
            ExactFinalizerAdmission::Complete => {
                unreachable!("complete admission returned before write")
            }
        }
        replacement.metadata.finalizers = Some(finalizers);
        prepare(&mut replacement);
        let replaced = pods
            .replace(name, &PostParams::default(), &replacement)
            .await;
        checkpoint()?;
        match replaced {
            Ok(patched) if admit(&patched)? == ExactFinalizerAdmission::Complete => {
                return Ok(());
            }
            Ok(_) => return Err(backend("Kubernetes Pod finalizer update was not retained")),
            Err(error) if api_conflict(&error) => continue,
            Err(error) if api_not_found(&error) && !require_present => return Ok(()),
            Err(error) => return Err(backend(error)),
        }
    }
}
