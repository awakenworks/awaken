// @formal entrypoint sandbox.k8s_process_observation_requires_complete_exit
//! Independent stop command for an already bound, retained Worker Pod domain.
//! It never removes evidence or calls Dispatch/Session cleanup.

use super::*;
use crate::k8s::finalizer::{ExactFinalizerReleaseAdmission, release_exact_pod_finalizer};
use kube::api::{DeleteParams, Preconditions};

/// A trusted recovery application may request termination of a whole Worker
/// process domain. This is not a per-Run cancellation port: other Runs can share
/// that domain. The caller must close its Worker admission before using it.
/// The adapter remains usable after that Worker dies and retains all evidence.
pub struct PodProcessStopper {
    observer: PodProcessObserver,
}

impl PodProcessStopper {
    pub fn new(observer: PodProcessObserver) -> Self {
        Self { observer }
    }

    /// Retry the same exact bound domain, never a replacement with its name.
    /// Accepted DELETE and an already-deleting Pod remain Unknown until the
    /// existing observer establishes every exact runtime exit in a Pod with
    /// accepted deletion intent. Natural exit does not skip that closure.
    /// Response loss is retried by reading the same UID; no separate
    /// stop-operation store exists.
    /// Missing history, unretained legacy bindings and API failures never mint a
    /// stop receipt. No response here grants replay or Dispatch retirement.
    pub async fn request_stop(
        &self,
        binding: &PodProcessBinding,
    ) -> Result<PodProcessTermination, RuntimeError> {
        if binding.namespace != self.observer.namespace {
            return Err(backend("Pod process binding belongs to another namespace"));
        }
        if !binding.retained {
            return Err(backend(
                "Worker process evidence was not retained before binding",
            ));
        }
        let pod = match self.observer.pods.get(&binding.name).await {
            Ok(pod) => pod,
            Err(error) if api_not_found(&error) => return Ok(PodProcessTermination::Unknown),
            Err(error) => return Err(backend(error)),
        };
        if matching_statuses(binding, &pod).is_none() {
            return Ok(PodProcessTermination::Unknown);
        }
        if !evidence_is_retained(&pod) {
            return Err(backend("Worker process evidence retention was lost"));
        }
        if pod.metadata.deletion_timestamp.is_some() {
            return Ok(observe(binding, &pod));
        }
        // Even an already-exited Pod needs the API-owned, irreversible deletion
        // intent. Never-restart policy and point-in-time exit alone do not close
        // its launch lifecycle. Accepted DELETE is still only Unknown: a fresh
        // read must establish both closure and the same exact process exits.
        let version = coordinate(pod.metadata.resource_version.as_deref())
            .ok_or_else(|| backend("Worker Pod has no exact resource version"))?;
        // Do not force-remove a zero-grace Pod's API history. Even disposable
        // profiles receive a positive grace period; the held finalizer and exit
        // observer, not this duration or DELETE response, establish the fact.
        let grace = pod
            .spec
            .as_ref()
            .and_then(|spec| spec.termination_grace_period_seconds);
        let mut params = DeleteParams::default().preconditions(Preconditions {
            uid: Some(binding.uid.clone()),
            resource_version: Some(version),
        });
        if grace == Some(0) {
            params = params.grace_period(1);
        }
        self.observer
            .pods
            .delete(&binding.name, &params)
            .await
            .map_err(backend)?;
        Ok(PodProcessTermination::Unknown)
    }

    /// Release only the evidence finalizer for the exact bound process domain.
    /// The trusted application calls this after its durable Registry readback.
    /// Missing or replacement Pods mean the old UID can no longer be retained;
    /// a matching Pod is updated through the shared resourceVersion CAS loop.
    pub async fn release_evidence(&self, binding: &PodProcessBinding) -> Result<(), RuntimeError> {
        if binding.namespace != self.observer.namespace {
            return Err(backend("Pod process binding belongs to another namespace"));
        }
        if !binding.retained {
            return Err(backend(
                "Worker process evidence was not retained before binding",
            ));
        }
        release_exact_pod_finalizer(
            &self.observer.pods,
            &binding.name,
            WORKER_PROCESS_EVIDENCE_FINALIZER,
            &|| Ok(()),
            &|pod| {
                if pod.metadata.uid.as_deref() != Some(binding.uid.as_str())
                    || !evidence_is_retained(pod)
                {
                    Ok(ExactFinalizerReleaseAdmission::Complete)
                } else {
                    Ok(ExactFinalizerReleaseAdmission::Remove)
                }
            },
            &|_| {},
        )
        .await
    }
}

#[cfg(test)]
mod tests;
