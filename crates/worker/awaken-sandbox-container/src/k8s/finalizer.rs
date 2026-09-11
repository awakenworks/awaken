//! One Kubernetes CAS loop for removing an exact finalizer.

use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use kube::api::PostParams;

use super::error::{api_conflict, api_not_found};
use super::{RuntimeError, backend};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ExactFinalizerReleaseAdmission {
    Remove,
    Complete,
}

/// Remove one finalizer through the exact Pod's resourceVersion. Domain owners
/// supply admission and metadata cleanup; this function alone owns GET/replace,
/// conflict retry and post-write verification.
pub(super) async fn release_exact_pod_finalizer(
    pods: &Api<Pod>,
    name: &str,
    finalizer: &str,
    checkpoint: &(dyn Fn() -> Result<(), RuntimeError> + Sync),
    admit: &(dyn Fn(&Pod) -> Result<ExactFinalizerReleaseAdmission, RuntimeError> + Sync),
    prepare: &(dyn Fn(&mut Pod) + Sync),
) -> Result<(), RuntimeError> {
    loop {
        checkpoint()?;
        let observed = match pods.get(name).await {
            Ok(pod) => pod,
            Err(error) if api_not_found(&error) => return Ok(()),
            Err(error) => return Err(backend(error)),
        };
        checkpoint()?;
        if admit(&observed)? == ExactFinalizerReleaseAdmission::Complete {
            return Ok(());
        }
        if observed.metadata.resource_version.is_none() {
            return Err(backend(
                "Kubernetes Pod has no resourceVersion for finalizer release",
            ));
        }
        let finalizers = observed
            .metadata
            .finalizers
            .clone()
            .unwrap_or_default()
            .into_iter()
            .filter(|candidate| candidate != finalizer)
            .collect::<Vec<_>>();
        let mut replacement = observed;
        replacement.metadata.finalizers = Some(finalizers);
        prepare(&mut replacement);
        let replaced = pods
            .replace(name, &PostParams::default(), &replacement)
            .await;
        checkpoint()?;
        match replaced {
            Ok(patched) if admit(&patched)? == ExactFinalizerReleaseAdmission::Complete => {
                return Ok(());
            }
            Ok(_) => return Err(backend("Kubernetes Pod finalizer release was not retained")),
            Err(error) if api_conflict(&error) => continue,
            Err(error) if api_not_found(&error) => return Ok(()),
            Err(error) => return Err(backend(error)),
        }
    }
}
