// @formal entrypoint sandbox.k8s_process_observation_requires_complete_exit
//! Trusted Kubernetes facts lowered into existing Coordinator ports.

use super::*;
#[cfg(test)]
use awaken_dispatch::classify_native_attempt_termination;
use awaken_dispatch::{
    ApplicationError, AttemptExecutionBinding, AttemptStopStatus, ClosedAttemptExecution,
    ClosedWorkerExecution, WorkerExecutionBindingSource, WorkerExecutionStopSource, WorkerIdentity,
    WorkerProcessStopStatus,
};
use awaken_dispatch::{NativeAttemptFootprint, NativeAttemptTermination};
use awaken_sandbox_container::k8s::{
    PodProcessBinding, PodProcessObserver, PodProcessStopper, PodProcessTermination,
};
use awaken_worker_transport_security::kubernetes::{
    KubernetesPodIdentity, KubernetesWorkerAuthenticator,
};

pub(super) fn assemble(
    client: kube::Client,
    namespace: &str,
    service_account: &str,
    audience: &str,
    provider_id: &str,
) -> Result<WorkerTransport, String> {
    let authenticator = KubernetesWorkerAuthenticator::new(
        client.clone(),
        namespace,
        service_account,
        audience,
        std::time::Duration::from_secs(5),
    )?;
    let observer =
        PodProcessObserver::new(client.clone(), namespace).map_err(|error| error.to_string())?;
    let stopper = PodProcessStopper::new(
        PodProcessObserver::new(client, namespace).map_err(|error| error.to_string())?,
    );
    let binding_source = Arc::new(KubernetesWorkerBindings {
        observer,
        namespace: namespace.to_owned(),
        provider_id: provider_id.to_owned(),
    });
    Ok(WorkerTransport {
        authenticator: Arc::new(authenticator),
        bindings: Some(binding_source.clone()),
        stops: Some(Arc::new(KubernetesWorkerStops {
            stopper,
            namespace: namespace.to_owned(),
            provider_id: provider_id.to_owned(),
        })),
        relay_targets: Some(binding_source),
    })
}

struct KubernetesWorkerBindings {
    observer: PodProcessObserver,
    namespace: String,
    provider_id: String,
}

#[async_trait::async_trait]
impl WorkerExecutionBindingSource for KubernetesWorkerBindings {
    async fn retain_registered_worker_evidence(
        &self,
        worker: &WorkerIdentity,
    ) -> Result<(), ApplicationError> {
        let pod = KubernetesPodIdentity::from_worker_id(&worker.worker_id)
            .map_err(ApplicationError::invalid)?;
        if pod.namespace() != self.namespace || !pod.matches_worker(worker) {
            return Err(ApplicationError::invalid(
                "Worker retention domain mismatch",
            ));
        }
        self.observer
            .retain_pod_process_evidence(pod.name(), pod.uid())
            .await
            .map_err(|_| ApplicationError::unavailable("exact Worker retention unavailable"))
    }
    async fn binding_for(
        &self,
        worker: &WorkerIdentity,
    ) -> Result<AttemptExecutionBinding, ApplicationError> {
        let pod = KubernetesPodIdentity::from_worker_id(&worker.worker_id)
            .map_err(ApplicationError::invalid)?;
        if pod.namespace() != self.namespace || !pod.matches_worker(worker) {
            return Err(ApplicationError::invalid(
                "Worker does not match its immutable Kubernetes process domain",
            ));
        }
        // The request identity has already passed TokenReview and Registry checks.
        // A replacement between that observation and this read must fail closed.
        let binding = self
            .observer
            .bind_pod_process(pod.name(), pod.uid())
            .await
            .map_err(|_| {
                ApplicationError::unavailable("exact Kubernetes Worker process is unavailable")
            })?;
        if !binding.worker_evidence_was_retained() {
            return Err(ApplicationError::unavailable(
                "Kubernetes Worker process evidence must be retained before admission",
            ));
        }
        let reference = serde_json::to_string(&binding)
            .map_err(|_| ApplicationError::internal("Worker process binding encoding failed"))?;
        AttemptExecutionBinding::new(worker.clone(), &self.provider_id, reference)
            .map_err(|error| ApplicationError::invalid(error.to_string()))
    }
}

#[async_trait::async_trait]
impl WorkerRelayTargetObserver for KubernetesWorkerBindings {
    async fn ip_for(
        &self,
        worker: &WorkerIdentity,
    ) -> Result<std::net::IpAddr, WorkerRelayTargetError> {
        let pod = KubernetesPodIdentity::from_worker_id(&worker.worker_id)
            .map_err(|_| WorkerRelayTargetError::InvalidIdentity)?;
        if pod.namespace() != self.namespace || !pod.matches_worker(worker) {
            return Err(WorkerRelayTargetError::InvalidIdentity);
        }
        self.observer
            .relay_ip_for(pod.name(), pod.uid())
            .await
            .map_err(|_| WorkerRelayTargetError::Unavailable)
    }
}

struct KubernetesWorkerStops {
    stopper: PodProcessStopper,
    namespace: String,
    provider_id: String,
}

impl KubernetesWorkerStops {
    fn process_for(
        &self,
        execution: &ClosedWorkerExecution,
    ) -> Result<PodProcessBinding, ApplicationError> {
        let binding = execution.binding();
        let pod = KubernetesPodIdentity::from_worker_id(&binding.worker().worker_id)
            .map_err(ApplicationError::invalid)?;
        let process: PodProcessBinding = serde_json::from_str(binding.execution_ref())
            .map_err(|_| ApplicationError::invalid("invalid persisted Worker process binding"))?;
        if binding.provider_id() != self.provider_id
            || pod.namespace() != self.namespace
            || !pod.matches_worker(binding.worker())
            || !process.matches_pod(pod.namespace(), pod.name(), pod.uid())
        {
            return Err(ApplicationError::invalid(
                "closed Worker binding does not belong to this Kubernetes execution domain",
            ));
        }
        Ok(process)
    }
}

#[async_trait::async_trait]
impl WorkerExecutionStopSource for KubernetesWorkerStops {
    fn owns_worker_evidence(&self, worker: &WorkerIdentity) -> bool {
        KubernetesPodIdentity::from_worker_id(&worker.worker_id)
            .is_ok_and(|pod| pod.namespace() == self.namespace && pod.matches_worker(worker))
    }
    async fn release_closed_idle_worker_evidence(
        &self,
        worker: &WorkerIdentity,
    ) -> Result<(), ApplicationError> {
        let pod = KubernetesPodIdentity::from_worker_id(&worker.worker_id)
            .map_err(ApplicationError::invalid)?;
        if pod.namespace() != self.namespace || !pod.matches_worker(worker) {
            return Err(ApplicationError::invalid(
                "idle Worker evidence belongs to another Kubernetes domain",
            ));
        }
        self.stopper
            .release_closed_idle_pod_evidence(pod.name(), pod.uid())
            .await
            .map_err(|_| {
                ApplicationError::unavailable("idle Worker process evidence release is unavailable")
            })
    }
    async fn stop_closed_worker(
        &self,
        execution: &ClosedWorkerExecution,
    ) -> Result<WorkerProcessStopStatus, ApplicationError> {
        let process = self.process_for(execution)?;
        // Uses Coordinator credentials, not the dead Pod's now-revoked token.
        // Exact UID/resourceVersion CAS and retention are owned by the provider.
        match self.stopper.request_stop(&process).await.map_err(|_| {
            ApplicationError::unavailable("exact Kubernetes Worker stop observation is unavailable")
        })? {
            PodProcessTermination::Unknown => Ok(WorkerProcessStopStatus::Unknown),
            PodProcessTermination::Observed(receipt) => {
                execution.observed_exit(receipt.resource_version())
            }
        }
    }

    async fn release_closed_worker_evidence(
        &self,
        execution: &ClosedWorkerExecution,
    ) -> Result<(), ApplicationError> {
        let process = self.process_for(execution)?;
        self.stopper.release_evidence(&process).await.map_err(|_| {
            ApplicationError::unavailable(
                "exact Kubernetes Worker process evidence release is unavailable",
            )
        })
    }

    async fn close_attempt_execution(
        &self,
        execution: &ClosedAttemptExecution,
        recovery: &awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
    ) -> Result<AttemptStopStatus, ApplicationError> {
        if !execution.attempt().closing {
            return Err(ApplicationError::internal(
                "attempt entry has not durably closed",
            ));
        }
        let footprint = NativeAttemptFootprint::decode(recovery)
            .map_err(|error| ApplicationError::internal(error.to_string()))?;
        // Check whether exact process closure could resolve this footprint
        // before requesting any destructive provider effect. Opaque/legacy
        // participants (including shared delegation) cannot be proved stopped
        // by deleting a Session Sandbox, so leave that resource untouched.
        if footprint.classify(footprint.process_domains()) == NativeAttemptTermination::Unknown {
            return Ok(AttemptStopStatus::Unknown);
        }
        let mut closed = Vec::new();
        for domain in footprint.process_domains() {
            if let awaken_tool_contract::tool_batch::ToolExecutionDomain::ControlledProcess {
                provider_ref,
                execution_ref,
            } = domain
            {
                if closed.contains(domain) {
                    continue;
                }
                if provider_ref != "kubernetes-sandbox" {
                    continue;
                }
                let binding: PodProcessBinding =
                    serde_json::from_str(execution_ref).map_err(|_| {
                        ApplicationError::internal("invalid contained Sandbox process reference")
                    })?;
                match self
                    .stopper
                    .request_stop(&binding)
                    .await
                    .map_err(|error| ApplicationError::unavailable(error.to_string()))?
                {
                    PodProcessTermination::Unknown => {}
                    PodProcessTermination::Observed(_) => closed.push(domain.clone()),
                }
            }
        }
        match footprint.classify(&closed) {
            NativeAttemptTermination::Unknown => Ok(AttemptStopStatus::Unknown),
            NativeAttemptTermination::AllExecutionStopped => execution
                .observed_all_execution_stopped(format!(
                    "{};thread-commit:{}",
                    execution.local_evidence_ref(),
                    recovery.store_cursor
                )),
            NativeAttemptTermination::ControlledExecutionClosed => execution
                .observed_execution_domain_closed(format!(
                    "{};thread-commit:{}",
                    execution.local_evidence_ref(),
                    recovery.store_cursor
                )),
        }
    }
}

#[cfg(test)]
mod tests;
