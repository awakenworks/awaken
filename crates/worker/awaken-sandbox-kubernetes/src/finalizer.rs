// @formal entrypoint sandbox.k8s_process_observation_requires_complete_exit
//! One Pod metadata CAS loop; finalizer owners retain their own list/admission law.

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
/// supply admission and metadata cleanup; the shared Pod loop alone owns
/// GET/replace, conflict retry and post-write verification.
pub(super) async fn update_exact_pod_finalizer(
    pods: &Api<Pod>,
    name: &str,
    finalizer: &str,
    require_present: bool,
    checkpoint: &(dyn Fn() -> Result<(), RuntimeError> + Sync),
    admit: &(dyn Fn(&Pod) -> Result<ExactFinalizerAdmission, RuntimeError> + Sync),
    prepare: &(dyn Fn(&mut Pod) + Sync),
) -> Result<(), RuntimeError> {
    let replacement = |observed: &Pod| {
        let admission = admit(observed)?;
        if admission == ExactFinalizerAdmission::Complete {
            return Ok(None);
        }
        if observed.metadata.resource_version.is_none() {
            return Err(backend(
                "Kubernetes Pod has no resourceVersion for finalizer update",
            ));
        }
        let mut replacement = observed.clone();
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
        Ok(Some(replacement))
    };
    let verify = |patched: &Pod| {
        if admit(patched)? == ExactFinalizerAdmission::Complete {
            Ok(())
        } else {
            Err(backend("Kubernetes Pod finalizer update was not retained"))
        }
    };
    update_pod_metadata(
        pods,
        name,
        None,
        require_present,
        Some(checkpoint),
        &replacement,
        &verify,
    )
    .await
    .map(|_| ())
}

/// The original cancellable Pod GET/replace/409 loop, shared without changing
/// any domain admission. `initial` is an already-observed Pod, not a fresh UID
/// sample. A 409 alone never authorizes a mutation: `replacement` must recheck
/// the caller's frozen identity and merge its one change into the fresh object.
/// None preserves the old owner-transfer interface's absence of an effect
/// callback; it proves no expiry check. Existing finalizer/publish callbacks
/// remain Some and run on the same pre/post-I/O sides as before.
pub(super) async fn update_pod_metadata(
    pods: &Api<Pod>,
    name: &str,
    mut initial: Option<Pod>,
    require_present: bool,
    checkpoint: Option<&(dyn Fn() -> Result<(), RuntimeError> + Sync)>,
    replacement: &(dyn Fn(&Pod) -> Result<Option<Pod>, RuntimeError> + Sync),
    verify: &(dyn Fn(&Pod) -> Result<(), RuntimeError> + Sync),
) -> Result<Option<Pod>, RuntimeError> {
    loop {
        if let Some(checkpoint) = checkpoint {
            checkpoint()?;
        }
        let observed = if let Some(pod) = initial.take() {
            pod
        } else {
            let pod = match pods.get(name).await {
                Ok(pod) => pod,
                Err(error) if api_not_found(&error) && !require_present => return Ok(None),
                Err(error) => return Err(backend(error)),
            };
            if let Some(checkpoint) = checkpoint {
                checkpoint()?;
            }
            pod
        };
        let Some(replacement) = replacement(&observed)? else {
            return Ok(Some(observed));
        };
        let replaced = pods
            .replace(name, &PostParams::default(), &replacement)
            .await;
        if let Some(checkpoint) = checkpoint {
            checkpoint()?;
        }
        match replaced {
            Ok(patched) => {
                verify(&patched)?;
                return Ok(Some(patched));
            }
            Err(error) if api_conflict(&error) => continue,
            Err(error) if api_not_found(&error) && !require_present => return Ok(None),
            Err(error) => return Err(backend(error)),
        }
    }
}
