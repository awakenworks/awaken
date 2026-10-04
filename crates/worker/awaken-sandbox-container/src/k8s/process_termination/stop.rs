// @formal entrypoint sandbox.k8s_process_observation_requires_complete_exit
//! Exact Worker Pod deletion and evidence release under trusted caller admission.
//! Retained stop observes exits; closed-idle retirement grants no exit or replay.

use super::*;
use crate::k8s::finalizer::{ExactFinalizerAdmission, update_exact_pod_finalizer};
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

    /// Retire only a Worker domain whose Registry admission is closed and whose
    /// complete Dispatch physical-slot scan is empty. Container restart cannot
    /// reopen this immutable UID. Deletion intent permits finalizer release here
    /// because no attempt needs its history; this operation returns no exit fact.
    pub async fn retire_closed_idle_pod(&self, name: &str, uid: &str) -> Result<(), RuntimeError> {
        if uid.is_empty() {
            return Err(backend("idle retirement requires an exact UID"));
        }
        let pod = match self.observer.pods.get(name).await {
            Ok(pod) => pod,
            Err(error) if api_not_found(&error) => return Ok(()),
            Err(error) => return Err(backend(error)),
        };
        if pod.metadata.uid.as_deref() != Some(uid) {
            return Ok(());
        }
        if pod.metadata.deletion_timestamp.is_none() {
            self.request_exact_delete(name, uid, &pod).await?;
        }
        self.release_closed_idle_pod_evidence(name, uid).await
    }

    /// A graceful Quiesced Worker may still be returning its deregistration
    /// RPC. Observe the exact running process without issuing DELETE; a Dead
    /// Worker uses the normal stop operation below instead.
    pub async fn exact_process_is_running(
        &self,
        binding: &PodProcessBinding,
    ) -> Result<bool, RuntimeError> {
        if binding.namespace != self.observer.namespace {
            return Err(backend("Pod process binding belongs to another namespace"));
        }
        let pod = self
            .observer
            .pods
            .get(&binding.name)
            .await
            .map_err(backend)?;
        if pod.metadata.uid.as_deref() != Some(binding.uid.as_str()) {
            return Err(backend("Pod process was replaced"));
        }
        Ok(
            bind(&self.observer.namespace, &binding.name, &binding.uid, &pod)
                .is_ok_and(|current| current == *binding),
        )
    }

    /// Drop retention metadata only after the trusted Coordinator has closed
    /// this incarnation and proved its Dispatch slots empty. No process binding
    /// or exit receipt is constructed, and a replacement UID is never touched.
    pub async fn release_closed_idle_pod_evidence(
        &self,
        name: &str,
        uid: &str,
    ) -> Result<(), RuntimeError> {
        if uid.is_empty() {
            return Err(backend("idle evidence release requires an exact UID"));
        }
        update_exact_pod_finalizer(
            &self.observer.pods,
            name,
            WORKER_PROCESS_EVIDENCE_FINALIZER,
            false,
            &|| Ok(()),
            &|pod| {
                if pod.metadata.uid.as_deref() != Some(uid)
                    || retention_finalizer(pod) != Some(WORKER_PROCESS_EVIDENCE_FINALIZER)
                {
                    Ok(ExactFinalizerAdmission::Complete)
                } else {
                    Ok(ExactFinalizerAdmission::Remove)
                }
            },
            &|_| {},
        )
        .await
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
        if !binding_retention_is_present(binding, &pod) {
            return Err(backend("Worker process evidence retention was lost"));
        }
        if pod.metadata.deletion_timestamp.is_some() {
            return Ok(observe(binding, &pod));
        }
        // Even an already-exited Pod needs the API-owned, irreversible deletion
        // intent. Never-restart policy and point-in-time exit alone do not close
        // its launch lifecycle. Accepted DELETE is still only Unknown: a fresh
        // read must establish both closure and the same exact process exits.
        self.request_exact_delete(&binding.name, &binding.uid, &pod)
            .await?;
        Ok(PodProcessTermination::Unknown)
    }

    // Retained stop and proven closed-idle retirement use one exact deletion
    // command. Their callers own different admission and evidence requirements.
    async fn request_exact_delete(
        &self,
        name: &str,
        uid: &str,
        pod: &Pod,
    ) -> Result<(), RuntimeError> {
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
            uid: Some(uid.to_owned()),
            resource_version: Some(version),
        });
        if grace == Some(0) {
            params = params.grace_period(1);
        }
        self.observer
            .pods
            .delete(name, &params)
            .await
            .map_err(backend)?;
        Ok(())
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
        // Session continuation retention belongs to its Resource lifecycle.
        // This process observer may consume it but must not release its PVC/
        // harvest fence. Worker-owned retention keeps its existing release.
        if binding.retention_finalizer != WORKER_PROCESS_EVIDENCE_FINALIZER {
            return Ok(());
        }
        self.release_closed_idle_pod_evidence(&binding.name, &binding.uid)
            .await
    }
}

#[cfg(test)]
mod tests;
