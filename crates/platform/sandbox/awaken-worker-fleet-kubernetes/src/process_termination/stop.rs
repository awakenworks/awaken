// @formal entrypoint sandbox.k8s_process_observation_requires_complete_exit
//! Exact Worker Pod deletion and evidence release under trusted caller admission.
//! Retained stop observes exits; closed-idle retirement grants no exit or replay.

use super::{
    ContainerFate, PodProcessBinding, PodProcessObserver, PodProcessTermination,
    WORKER_PROCESS_EVIDENCE_FINALIZER, api_not_found, backend, bind, binding_retention_is_present,
    container_incarnation, coordinate, matching_statuses, observe, restartable_fates,
    retention_finalizer, runs_newer_container,
};
use crate::pod_metadata::{ExactFinalizerAdmission, update_exact_pod_finalizer};
use awaken_container_contract::RuntimeError;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{DeleteParams, ListParams, Preconditions};

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

    /// Retire one closed-idle Worker incarnation. If its restartable Pod already
    /// runs a newer container instance, that newer incarnation owns the Pod
    /// and its retention: nothing is deleted or released here. Otherwise this
    /// is the Pod-level closed-idle retirement above.
    pub async fn retire_closed_idle_incarnation(
        &self,
        binding: &PodProcessBinding,
    ) -> Result<(), RuntimeError> {
        if binding.namespace != self.observer.namespace {
            return Err(backend("Pod process binding belongs to another namespace"));
        }
        let pod = match self.observer.pods.get(&binding.name).await {
            Ok(pod) => pod,
            Err(error) if api_not_found(&error) => return Ok(()),
            Err(error) => return Err(backend(error)),
        };
        if runs_newer_container(binding, &pod) {
            return Ok(());
        }
        self.retire_closed_idle_pod(&binding.name, &binding.uid)
            .await
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
    /// Until the API accepts deletion intent, retain the original Pod set.
    pub async fn release_closed_idle_pod_evidence(
        &self,
        name: &str,
        uid: &str,
    ) -> Result<(), RuntimeError> {
        self.release_pod_evidence(name, uid, ReleaseAdmission::DeletionIntent)
            .await
    }

    /// This namespace's ended Pods that still carry the Worker process-evidence
    /// finalizer, in name order. Ended means a terminal phase, or deletion
    /// intent with every container terminated (an Always Pod being deleted).
    /// Listing grants nothing; the caller decides which may release.
    pub async fn ended_retained_worker_pods(
        &self,
    ) -> Result<Vec<EndedRetainedWorkerPod>, RuntimeError> {
        let pods = self
            .observer
            .pods
            .list(&ListParams::default())
            .await
            .map_err(backend)?;
        let mut ended: Vec<_> = pods
            .items
            .iter()
            .filter_map(ended_retained_worker_pod)
            .collect();
        ended.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(ended)
    }

    /// Release the Worker finalizer of an ended Pod whose process never reached
    /// the Registry. The caller must first prove that: no Registry record and an
    /// absent or sealed unbound Create ledger. Here the fresh read must still be
    /// the same UID, still ended (an absorbing phase: no process can start or
    /// bind again) and still retained; the CAS is fenced by resourceVersion.
    /// No DELETE is issued and no exit receipt is produced.
    pub async fn release_ended_unregistered_pod_evidence(
        &self,
        name: &str,
        uid: &str,
    ) -> Result<(), RuntimeError> {
        self.release_pod_evidence(name, uid, ReleaseAdmission::Ended)
            .await
    }

    // One finalizer CAS for every release authority: a durable exact process
    // exit may release immediately; an unbound/closed-idle coordinate alone
    // must wait for API-owned deletion intent; a never-registered domain needs
    // its Pod ended. None issues DELETE.
    async fn release_pod_evidence(
        &self,
        name: &str,
        uid: &str,
        admission: ReleaseAdmission<'_>,
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
                let admitted = match admission {
                    ReleaseAdmission::ExactExit(binding) => !runs_newer_container(binding, pod),
                    ReleaseAdmission::DeletionIntent => pod.metadata.deletion_timestamp.is_some(),
                    ReleaseAdmission::Ended => pod_has_ended(pod),
                };
                if pod.metadata.uid.as_deref() != Some(uid)
                    || retention_finalizer(pod) != Some(WORKER_PROCESS_EVIDENCE_FINALIZER)
                    || !admitted
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
        self.request_stop_observing_loss(binding)
            .await
            .map(|(termination, _)| termination)
    }

    /// [`Self::request_stop`], also saying whether the bound Pod's evidence is
    /// lost for good: the Pod is gone, or another UID holds its name. The
    /// observation is then Unknown and can never become an exit or a
    /// supersession; the caller owns the operator quarantine signal.
    pub async fn request_stop_observing_loss(
        &self,
        binding: &PodProcessBinding,
    ) -> Result<(PodProcessTermination, ProcessEvidence), RuntimeError> {
        let lost = (PodProcessTermination::Unknown, ProcessEvidence::Lost);
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
            Err(error) if api_not_found(&error) => return Ok(lost),
            Err(error) => return Err(backend(error)),
        };
        if pod.metadata.uid.as_deref() != Some(binding.uid.as_str()) {
            return Ok(lost);
        }
        let retained = |termination| (termination, ProcessEvidence::Retained);
        if binding.restartable {
            return self
                .request_restartable_stop(binding, &pod)
                .await
                .map(retained);
        }
        if matching_statuses(binding, &pod).is_none() {
            return Ok(retained(PodProcessTermination::Unknown));
        }
        if !binding_retention_is_present(binding, &pod) {
            return Err(backend("Worker process evidence retention was lost"));
        }
        if pod.metadata.deletion_timestamp.is_some() {
            return Ok(retained(observe(binding, &pod)));
        }
        // Even an already-exited Pod needs the API-owned, irreversible deletion
        // intent. Never-restart policy and point-in-time exit alone do not close
        // its launch lifecycle. Accepted DELETE is still only Unknown: a fresh
        // read must establish both closure and the same exact process exits.
        self.request_exact_delete(&binding.name, &binding.uid, &pod)
            .await?;
        Ok(retained(PodProcessTermination::Unknown))
    }

    /// Container-instance stop for a restartable binding:
    /// | bound containers                   | effect                                   |
    /// | every one exactly exited           | local exit receipt; the Pod is untouched |
    /// | one still running                  | one exact UID/RV Pod DELETE, Unknown     |
    /// | none running or lost, some superseded | Superseded receipt; Pod untouched     |
    /// | none running, one lost             | Unknown (indeterminate), no mutation     |
    /// A foreign Pod (node, policy, names) is Unknown; a missing Pod or another
    /// UID is Unknown with lost evidence (see `request_stop_observing_loss`);
    /// lost retention is an error. A Pod that already runs a newer incarnation is never deleted:
    /// the bound instance is then exited, superseded or lost, not running.
    async fn request_restartable_stop(
        &self,
        binding: &PodProcessBinding,
        pod: &Pod,
    ) -> Result<PodProcessTermination, RuntimeError> {
        let Some(fates) = restartable_fates(binding, pod) else {
            return Ok(PodProcessTermination::Unknown);
        };
        if !binding_retention_is_present(binding, pod) {
            return Err(backend("Worker process evidence retention was lost"));
        }
        if fates.contains(&ContainerFate::Running) {
            if pod.metadata.deletion_timestamp.is_none() {
                self.request_exact_delete(&binding.name, &binding.uid, pod)
                    .await?;
            }
            return Ok(PodProcessTermination::Unknown);
        }
        Ok(observe(binding, pod))
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
        // Sandbox Create and continuation retention belong to their Resource
        // lifecycle. This observer may consume them but must not release a
        // Create-result, PVC or harvest fence. Worker release stays unchanged.
        if binding.retention_finalizer != WORKER_PROCESS_EVIDENCE_FINALIZER {
            return Ok(());
        }
        self.release_pod_evidence(
            &binding.name,
            &binding.uid,
            ReleaseAdmission::ExactExit(binding),
        )
        .await
    }
}

/// Whether a bound Pod can still report the bound processes' end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessEvidence {
    /// The exact Pod UID is still readable; Unknown may later resolve.
    Retained,
    /// The Pod is gone or another UID holds its name: no exit or supersession
    /// can ever be observed. The bound attempts stay quarantined with
    /// Indeterminate effects until an operator resolves them.
    Lost,
}

/// What, besides the exact UID and the Worker finalizer, admits a release.
#[derive(Clone, Copy)]
enum ReleaseAdmission<'a> {
    /// The Registry durably holds this binding's exact exit. A restartable Pod
    /// that already runs a newer container instance keeps its retention for
    /// that newer incarnation; the release is then a no-op.
    ExactExit(&'a PodProcessBinding),
    /// A closed-idle coordinate: the API must already have deletion intent.
    DeletionIntent,
    /// A never-registered domain: the Pod phase must be terminal.
    Ended,
}

fn pod_has_ended(pod: &Pod) -> bool {
    let Some(status) = pod.status.as_ref() else {
        return false;
    };
    matches!(status.phase.as_deref(), Some("Succeeded" | "Failed"))
        || (pod.metadata.deletion_timestamp.is_some()
            && status.container_statuses.as_ref().is_some_and(|statuses| {
                !statuses.is_empty()
                    && statuses.iter().all(|status| {
                        status
                            .state
                            .as_ref()
                            .is_some_and(|state| state.terminated.is_some())
                    })
            }))
}

/// An ended Pod still retaining Worker evidence: its exact coordinate and the
/// incarnation of its last container instance set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndedRetainedWorkerPod {
    pub name: String,
    pub uid: String,
    pub incarnation: String,
}

fn ended_retained_worker_pod(pod: &Pod) -> Option<EndedRetainedWorkerPod> {
    let retained = pod.metadata.finalizers.as_ref().is_some_and(|values| {
        values
            .iter()
            .any(|v| v == WORKER_PROCESS_EVIDENCE_FINALIZER)
    });
    if !retained || !pod_has_ended(pod) {
        return None;
    }
    let statuses = pod.status.as_ref()?.container_statuses.as_ref()?;
    let ids = statuses
        .iter()
        .map(|status| Some((status.name.as_str(), status.container_id.as_deref()?)))
        .collect::<Option<Vec<_>>>()?;
    Some(EndedRetainedWorkerPod {
        name: coordinate(pod.metadata.name.as_deref())?,
        uid: coordinate(pod.metadata.uid.as_deref())?,
        incarnation: container_incarnation(ids.into_iter()),
    })
}

#[cfg(test)]
mod tests;
