//! Trusted Kubernetes facts lowered into existing Coordinator ports.

use super::*;
use awaken_dispatch::{
    ApplicationError, AttemptExecutionBinding, WorkerExecutionBindingSource, WorkerIdentity,
};
use awaken_sandbox_container::k8s::PodProcessObserver;
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
    let observer =
        PodProcessObserver::new(client, &policy.namespace).map_err(|error| error.to_string())?;
    Ok(WorkerTransport {
        authenticator: Arc::new(authenticator),
        bindings: Some(Arc::new(KubernetesWorkerBindings {
            observer,
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
        let reference = serde_json::to_string(&binding)
            .map_err(|_| ApplicationError::internal("Worker process binding encoding failed"))?;
        AttemptExecutionBinding::new(worker.clone(), &self.provider_id, reference)
            .map_err(|error| ApplicationError::invalid(error.to_string()))
    }
}

#[cfg(test)]
mod tests;
