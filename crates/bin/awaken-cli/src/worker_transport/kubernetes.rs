// @formal entrypoint sandbox.k8s_process_observation_requires_complete_exit
//! Trusted Kubernetes facts lowered into existing Coordinator ports.

use awaken_container_contract::{K8sCreateBrokerEffects, K8sLedgerBinding};
use awaken_session_execution_contract::ApplicationError;
use std::sync::Arc;

use super::WorkerTransport;
#[cfg(test)]
use awaken_dispatch::classify_native_attempt_termination;
use awaken_dispatch::{
    AttemptExecutionBinding, AttemptStopStatus, ClosedAttemptExecution, ClosedWorkerExecution,
    WorkerCreateLedger, WorkerExecutionBindingSource, WorkerExecutionStopSource, WorkerIdentity,
    WorkerProcessStopStatus, WorkerRegistration,
};
use awaken_dispatch::{NativeAttemptFootprint, NativeAttemptTermination};
use awaken_sandbox_kubernetes::{
    K8sWorkerCreateLedger, PodProcessBinding, PodProcessObserver, PodProcessStopper,
    PodProcessTermination,
};
use awaken_worker_contract::{WorkerRelayTargetError, WorkerRelayTargetObserver};
use awaken_worker_transport_security::kubernetes::{
    KubernetesPodIdentity, KubernetesWorkerAuthenticator,
};

use awaken_coordinator::worker_create_broker::{K8sCreateBroker, seal_worker_requests};

pub(super) fn assemble(
    client: kube::Client,
    namespace: &str,
    service_account: &str,
    audience: &str,
    provider_id: &str,
    create_control_namespace: &str,
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
    let create_ledger = Arc::new(
        K8sWorkerCreateLedger::new(
            client.clone(),
            create_control_namespace,
            namespace,
            provider_id,
        )
        .map_err(|error| error.to_string())?,
    );
    let create_broker = Arc::new(K8sCreateBroker::new(
        create_ledger.clone(),
        namespace,
        provider_id,
    ));
    let stopper = PodProcessStopper::new(
        PodProcessObserver::new(client, namespace).map_err(|error| error.to_string())?,
    );
    let binding_source = Arc::new(KubernetesWorkerBindings {
        observer,
        create_ledger: create_ledger.clone(),
        namespace: namespace.to_owned(),
        provider_id: provider_id.to_owned(),
    });
    Ok(WorkerTransport {
        authenticator: Arc::new(authenticator),
        bindings: Some(binding_source.clone()),
        stops: Some(Arc::new(KubernetesWorkerStops {
            stopper,
            create_ledger,
            namespace: namespace.to_owned(),
            provider_id: provider_id.to_owned(),
        })),
        relay_targets: Some(binding_source),
        create_broker: Some(create_broker),
    })
}

struct KubernetesWorkerBindings {
    observer: PodProcessObserver,
    create_ledger: Arc<K8sWorkerCreateLedger>,
    namespace: String,
    provider_id: String,
}

#[async_trait::async_trait]
impl WorkerExecutionBindingSource for KubernetesWorkerBindings {
    async fn seal_unattempted_worker(
        &self,
        worker_id: &str,
        expected: &WorkerCreateLedger,
    ) -> Result<WorkerCreateLedger, ApplicationError> {
        let pod =
            KubernetesPodIdentity::from_worker_id(worker_id).map_err(ApplicationError::invalid)?;
        if pod.namespace() != self.namespace || pod.worker_id() != worker_id {
            return Err(ApplicationError::invalid(
                "Worker bootstrap names another Pod namespace",
            ));
        }
        let (name, uid) = self
            .create_ledger
            .existing_worker_anchor(pod.namespace(), pod.name(), pod.uid())
            .await
            .map_err(|_| {
                ApplicationError::unavailable("original Worker bootstrap ledger unavailable")
            })?;
        let anchor = WorkerCreateLedger::new(
            &self.provider_id,
            self.create_ledger.control_namespace(),
            name,
            uid,
            self.create_ledger.configuration_fingerprint(),
        )
        .map_err(|error| ApplicationError::internal(error.to_string()))?;
        let binding = K8sLedgerBinding {
            worker_namespace: pod.namespace(),
            worker_name: pod.name(),
            worker_uid: pod.uid(),
            ledger_name: anchor.name(),
            ledger_uid: anchor.uid(),
            configuration_fingerprint: anchor.configuration_fingerprint(),
        };
        if &anchor != expected {
            return Err(ApplicationError::unavailable(
                "original Worker bootstrap anchor changed",
            ));
        }
        self.create_ledger
            .seal_unattempted_worker(binding)
            .await
            .map_err(|_| {
                ApplicationError::unavailable("Worker bootstrap is bound, incomplete or changed")
            })?;
        Ok(anchor)
    }

    async fn prepare_registration_physical_evidence(
        &self,
        registration: &WorkerRegistration,
    ) -> Result<Option<WorkerCreateLedger>, ApplicationError> {
        let pod = KubernetesPodIdentity::from_worker_id(&registration.worker_id)
            .map_err(ApplicationError::invalid)?;
        if pod.namespace() != self.namespace
            || registration.worker_id != pod.worker_id()
            || registration.incarnation_id != pod.uid()
        {
            return Err(ApplicationError::invalid(
                "Create ledger registration names another Worker Pod",
            ));
        }
        let (name, uid) = self
            .create_ledger
            .initialize(pod.namespace(), pod.name(), pod.uid())
            .await
            .map_err(|_| ApplicationError::unavailable("Worker Create ledger unavailable"))?;
        let anchor = WorkerCreateLedger::new(
            &self.provider_id,
            self.create_ledger.control_namespace(),
            name,
            uid,
            self.create_ledger.configuration_fingerprint(),
        )
        .map_err(|error| ApplicationError::internal(error.to_string()))?;
        // Retain the original process before Registry can accept the Worker.
        // A crash after provider retention but before registration leaves no
        // Ready authority; a retry rejoins the immutable ledger and binding.
        let provisional =
            WorkerIdentity::new(&registration.worker_id, &registration.incarnation_id, 1);
        self.retain_registered_worker_evidence(&provisional, Some(&anchor))
            .await?;
        Ok(Some(anchor))
    }
    async fn retain_registered_worker_evidence(
        &self,
        worker: &WorkerIdentity,
        create_ledger: Option<&WorkerCreateLedger>,
    ) -> Result<(), ApplicationError> {
        let pod = KubernetesPodIdentity::from_worker_id(&worker.worker_id)
            .map_err(ApplicationError::invalid)?;
        if pod.namespace() != self.namespace || !pod.matches_worker(worker) {
            return Err(ApplicationError::invalid(
                "Worker retention domain mismatch",
            ));
        }
        let anchor = create_ledger.ok_or_else(|| {
            ApplicationError::unavailable(
                "registered Kubernetes Worker has no frozen Create ledger",
            )
        })?;
        let exact = K8sLedgerBinding {
            worker_namespace: pod.namespace(),
            worker_name: pod.name(),
            worker_uid: pod.uid(),
            ledger_name: anchor.name(),
            ledger_uid: anchor.uid(),
            configuration_fingerprint: anchor.configuration_fingerprint(),
        };
        if anchor.provider_id() != self.provider_id
            || anchor.namespace() != self.create_ledger.control_namespace()
        {
            return Err(ApplicationError::unavailable(
                "registered Kubernetes Worker Create ledger provider changed",
            ));
        }
        self.observer
            .retain_pod_process_evidence(pod.name(), pod.uid())
            .await
            .map_err(|_| ApplicationError::unavailable("exact Worker retention unavailable"))?;
        let process = self
            .observer
            .bind_pod_process(pod.name(), pod.uid())
            .await
            .map_err(|_| {
                ApplicationError::unavailable("retained Worker process binding unavailable")
            })?;
        if !process.worker_evidence_was_retained() {
            return Err(ApplicationError::unavailable(
                "Worker process evidence was not retained",
            ));
        }
        self.create_ledger
            .retain_process_binding(exact, &process)
            .await
            .map_err(|_| {
                ApplicationError::unavailable("Worker process binding could not be retained")
            })
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
    create_ledger: Arc<K8sWorkerCreateLedger>,
    namespace: String,
    provider_id: String,
}

impl KubernetesWorkerStops {
    fn process_for(
        &self,
        execution: &ClosedWorkerExecution,
    ) -> Result<(PodProcessBinding, KubernetesPodIdentity), ApplicationError> {
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
        Ok((process, pod))
    }

    async fn seal_original_create_admission(
        &self,
        pod: &KubernetesPodIdentity,
        worker: &WorkerIdentity,
        original: Option<&WorkerCreateLedger>,
    ) -> Result<(), ApplicationError> {
        let Some(anchor) = original else {
            return Ok(());
        };
        seal_worker_requests(
            self.create_ledger.as_ref(),
            anchor,
            pod,
            worker,
            &self.provider_id,
        )
        .await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl WorkerExecutionStopSource for KubernetesWorkerStops {
    fn owns_worker_evidence(&self, worker: &WorkerIdentity) -> bool {
        KubernetesPodIdentity::from_worker_id(&worker.worker_id)
            .is_ok_and(|pod| pod.namespace() == self.namespace && pod.matches_worker(worker))
    }
    async fn seal_closed_worker_admission(
        &self,
        worker: &WorkerIdentity,
        original_create_ledger: Option<&WorkerCreateLedger>,
    ) -> Result<(), ApplicationError> {
        let pod = KubernetesPodIdentity::from_worker_id(&worker.worker_id)
            .map_err(ApplicationError::invalid)?;
        if pod.namespace() != self.namespace || !pod.matches_worker(worker) {
            return Err(ApplicationError::invalid(
                "closed Worker admission belongs to another Kubernetes domain",
            ));
        }
        self.seal_original_create_admission(&pod, worker, original_create_ledger)
            .await
    }
    async fn observe_closed_idle_worker(
        &self,
        worker: &WorkerIdentity,
        may_stop_active: bool,
        create_ledger: Option<&WorkerCreateLedger>,
    ) -> Result<Option<awaken_dispatch::WorkerProcessExit>, ApplicationError> {
        let pod = KubernetesPodIdentity::from_worker_id(&worker.worker_id)
            .map_err(ApplicationError::invalid)?;
        if pod.namespace() != self.namespace || !pod.matches_worker(worker) {
            return Ok(None);
        }
        let Some(anchor) = create_ledger else {
            return Ok(None);
        };
        if anchor.provider_id() != self.provider_id
            || anchor.namespace() != self.create_ledger.control_namespace()
        {
            return Ok(None);
        }
        let binding = K8sLedgerBinding {
            worker_namespace: pod.namespace(),
            worker_name: pod.name(),
            worker_uid: pod.uid(),
            ledger_name: anchor.name(),
            ledger_uid: anchor.uid(),
            configuration_fingerprint: anchor.configuration_fingerprint(),
        };
        let Some(process) = self
            .create_ledger
            .read_process_binding(binding)
            .await
            .map_err(|_| {
                ApplicationError::unavailable("idle Worker process binding unavailable")
            })?
        else {
            return Ok(None);
        };
        if !may_stop_active
            && self
                .stopper
                .exact_process_is_running(&process)
                .await
                .map_err(|_| {
                    ApplicationError::unavailable("graceful Worker process observation unavailable")
                })?
        {
            return Ok(None);
        }
        let observed = self.stopper.request_stop(&process).await.map_err(|_| {
            ApplicationError::unavailable("idle Worker stop observation unavailable")
        })?;
        let PodProcessTermination::Observed(receipt) = observed else {
            return Ok(None);
        };
        let reference = serde_json::to_string(&process)
            .map_err(|_| ApplicationError::internal("idle Worker process encoding failed"))?;
        awaken_dispatch::WorkerProcessExit::new(
            &self.provider_id,
            reference,
            receipt.resource_version(),
        )
        .map(Some)
        .map_err(|error| ApplicationError::internal(error.to_string()))
    }
    async fn release_closed_idle_worker_evidence(
        &self,
        worker: &WorkerIdentity,
        original_create_ledger: Option<&WorkerCreateLedger>,
    ) -> Result<(), ApplicationError> {
        let pod = KubernetesPodIdentity::from_worker_id(&worker.worker_id)
            .map_err(ApplicationError::invalid)?;
        if pod.namespace() != self.namespace || !pod.matches_worker(worker) {
            return Err(ApplicationError::invalid(
                "idle Worker evidence belongs to another Kubernetes domain",
            ));
        }
        self.seal_original_create_admission(&pod, worker, original_create_ledger)
            .await?;
        self.stopper
            .retire_closed_idle_pod(pod.name(), pod.uid())
            .await
            .map_err(|_| {
                ApplicationError::unavailable("closed-idle Worker Pod retirement is unavailable")
            })
    }
    async fn stop_closed_worker(
        &self,
        execution: &ClosedWorkerExecution,
    ) -> Result<WorkerProcessStopStatus, ApplicationError> {
        let (process, _) = self.process_for(execution)?;
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
        let (process, pod) = self.process_for(execution)?;
        self.seal_original_create_admission(
            &pod,
            execution.binding().worker(),
            execution.original_create_ledger(),
        )
        .await?;
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
            if let Some((provider_ref, execution_ref)) = domain.controlled_process_binding() {
                if closed.iter().any(
                    |observed: &awaken_tool_contract::tool_batch::ToolExecutionDomain| {
                        observed.same_containment_as(domain)
                    },
                ) {
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
