//! Trusted Kubernetes facts lowered into existing Coordinator ports.

use super::*;
use awaken_dispatch::{
    ApplicationError, AttemptExecutionBinding, AttemptStopStatus, ClosedAttemptExecution,
    ClosedWorkerExecution, WorkerExecutionBindingSource, WorkerExecutionStopSource, WorkerIdentity,
    WorkerProcessStopStatus,
};
use awaken_runtime_contract::attempt_termination::{
    NativeAttemptTermination, classify_native_attempt_termination,
};
use awaken_sandbox_container::k8s::{
    PodProcessBinding, PodProcessObserver, PodProcessStopper, PodProcessTermination,
};
use awaken_worker_transport_security::kubernetes::{
    KubernetesPodIdentity, KubernetesWorkerAuthenticator,
};

pub(super) fn assemble(
    policy: &crate::config::KubernetesWorkerTransport,
    client: kube::Client,
) -> Result<WorkerTransport, String> {
    let authenticator = KubernetesWorkerAuthenticator::new(
        client.clone(),
        &policy.namespace,
        &policy.service_account,
        &policy.audience,
        std::time::Duration::from_secs(5),
    )?;
    let observer = PodProcessObserver::new(client.clone(), &policy.namespace)
        .map_err(|error| error.to_string())?;
    let stopper = PodProcessStopper::new(
        PodProcessObserver::new(client, &policy.namespace).map_err(|error| error.to_string())?,
    );
    Ok(WorkerTransport {
        authenticator: Arc::new(authenticator),
        bindings: Some(Arc::new(KubernetesWorkerBindings {
            observer,
            namespace: policy.namespace.clone(),
            provider_id: policy.provider_id.clone(),
        })),
        stops: Some(Arc::new(KubernetesWorkerStops {
            stopper,
            namespace: policy.namespace.clone(),
            provider_id: policy.provider_id.clone(),
        })),
    })
}

struct KubernetesWorkerBindings {
    observer: PodProcessObserver,
    namespace: String,
    provider_id: String,
}

#[async_trait::async_trait]
impl WorkerExecutionBindingSource for KubernetesWorkerBindings {
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
        if !binding.evidence_was_retained() {
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

    async fn observe_attempt_termination(
        &self,
        execution: &ClosedAttemptExecution,
        recovery: &awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
    ) -> Result<AttemptStopStatus, ApplicationError> {
        match classify_native_attempt_termination(recovery)
            .map_err(|error| ApplicationError::internal(error.to_string()))?
        {
            NativeAttemptTermination::Unknown => Ok(AttemptStopStatus::Unknown),
            NativeAttemptTermination::AllExecutionStopped => execution
                .observed_all_execution_stopped(format!(
                    "{};thread-commit:{}",
                    execution.process_exit().evidence_ref(),
                    recovery.store_cursor
                )),
        }
    }
}

#[cfg(test)]
mod tests;
