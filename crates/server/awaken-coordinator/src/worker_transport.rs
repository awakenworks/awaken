//! Shared assembly of the existing Worker authentication and execution sources.
//! Process roles retain configuration ownership; Dispatch retains admission.

use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum WorkerRelayTargetError {
    #[error("Worker Pod identity is invalid or foreign")]
    InvalidIdentity,
    #[error("exact Worker Pod route evidence is unavailable")]
    Unavailable,
}

/// Observe the address of an already selected Worker. The provider must bind
/// its identity, Pod UID and IP in one observation; it cannot select a Worker
/// or grant an MCP route.
#[async_trait::async_trait]
pub trait WorkerRelayTargetObserver: Send + Sync {
    async fn ip_for(
        &self,
        worker: &awaken_worker_contract::WorkerIdentity,
    ) -> Result<std::net::IpAddr, WorkerRelayTargetError>;
}

#[cfg(feature = "container-k8s")]
mod kubernetes;
#[cfg(target_os = "linux")]
mod systemd;

/// Private fields keep authentication and physical binding selected together.
/// An injected authenticator alone is deliberately an unbound transport.
pub struct WorkerTransport {
    authenticator: Arc<dyn awaken_worker_transport_security::WorkerRequestAuthenticator>,
    bindings: Option<Arc<dyn awaken_dispatch::WorkerExecutionBindingSource>>,
    stops: Option<Arc<dyn awaken_dispatch::WorkerExecutionStopSource>>,
    relay_targets: Option<Arc<dyn WorkerRelayTargetObserver>>,
}

impl WorkerTransport {
    pub fn unbound(
        authenticator: Arc<dyn awaken_worker_transport_security::WorkerRequestAuthenticator>,
    ) -> Self {
        Self {
            authenticator,
            bindings: None,
            stops: None,
            relay_targets: None,
        }
    }

    pub fn authenticator(
        &self,
    ) -> Arc<dyn awaken_worker_transport_security::WorkerRequestAuthenticator> {
        self.authenticator.clone()
    }

    pub fn binding_source(
        &self,
    ) -> Option<Arc<dyn awaken_dispatch::WorkerExecutionBindingSource>> {
        self.bindings.clone()
    }

    pub fn stop_source(&self) -> Option<Arc<dyn awaken_dispatch::WorkerExecutionStopSource>> {
        self.stops.clone()
    }

    pub fn relay_target_source(&self) -> Option<Arc<dyn WorkerRelayTargetObserver>> {
        self.relay_targets.clone()
    }
}

#[cfg(target_os = "linux")]
impl WorkerTransport {
    /// Probe a user manager and the same trusted executables used by the
    /// binding/stop adapter before selecting invisible local placement.
    pub async fn systemd_user_available() -> bool {
        systemd::available().await
    }

    pub fn systemd_user_run_executable() -> Option<std::path::PathBuf> {
        systemd::run_executable()
    }

    pub fn systemd_user_env_executable() -> Option<std::path::PathBuf> {
        systemd::env_executable()
    }

    pub fn systemd_user_launch_environment() -> Result<Vec<std::ffi::OsString>, String> {
        systemd::launch_environment()
    }

    pub async fn systemd_user_unit_running(incarnation_id: &str) -> Result<bool, String> {
        systemd::unit_running(incarnation_id).await
    }

    pub async fn systemd_user_request_drain(incarnation_id: &str) -> Result<(), String> {
        systemd::request_drain(incarnation_id).await
    }

    /// The single exact unit-name projection shared by a product launcher and
    /// this transport's physical binding/stop evidence.
    pub fn systemd_user_unit_name(incarnation_id: &str) -> Result<String, String> {
        systemd::unit_name_for_incarnation(incarnation_id)
    }

    /// Bind one local independent Worker to its retained systemd user service.
    /// Authentication and physical evidence are selected together, while the
    /// existing Dispatch and Registry remain the only attempt authorities.
    pub fn systemd_user(
        authenticator: Arc<dyn awaken_worker_transport_security::WorkerRequestAuthenticator>,
        provider_id: &str,
        evidence_dir: &std::path::Path,
    ) -> Result<Self, String> {
        let adapter = Arc::new(systemd::SystemdWorkerEvidence::new(
            provider_id,
            evidence_dir,
        )?);
        Ok(Self {
            authenticator,
            bindings: Some(adapter.clone()),
            stops: Some(adapter),
        })
    }
}

#[cfg(feature = "container-k8s")]
impl WorkerTransport {
    /// Validate the existing Kubernetes trust domain before provider I/O.
    pub fn validate_kubernetes_policy(
        namespace: &str,
        service_account: &str,
        audience: &str,
        provider_id: &str,
    ) -> Result<(), String> {
        if provider_id.trim().is_empty() || provider_id.trim() != provider_id {
            return Err("worker_kubernetes requires an exact nonempty provider_id".into());
        }
        awaken_worker_transport_security::kubernetes::KubernetesWorkerAuthenticator::validate_policy(
            namespace,
            service_account,
            audience,
            std::time::Duration::from_secs(5),
        )
    }

    /// Select one authenticated provider domain for identity, binding and stop.
    pub fn kubernetes(
        client: kube::Client,
        namespace: &str,
        service_account: &str,
        audience: &str,
        provider_id: &str,
    ) -> Result<Self, String> {
        Self::validate_kubernetes_policy(namespace, service_account, audience, provider_id)?;
        kubernetes::assemble(client, namespace, service_account, audience, provider_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unbound_transport_preserves_authentication_without_claiming_process_evidence() {
        // Decision U1: supplied authenticator + no selected process provider ->
        // preserve the exact authentication object; both evidence ports absent.
        // U2 Kubernetes construction owns the paired provider ports in its
        // adjacent identity/replacement/stop tests; no independent injection.
        let auth: Arc<dyn awaken_worker_transport_security::WorkerRequestAuthenticator> =
            Arc::new(awaken_worker_transport_security::HeaderWorkerAuthenticator);
        let transport = WorkerTransport::unbound(auth.clone());
        assert!(Arc::ptr_eq(&auth, &transport.authenticator()));
        assert!(transport.binding_source().is_none());
        assert!(transport.stop_source().is_none());
        assert!(transport.relay_target_source().is_none());
    }
}
