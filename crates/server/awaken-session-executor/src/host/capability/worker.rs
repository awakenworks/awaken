//! WORKER capability of [`SharedHost`]: the role-neutral claim-fenced Control
//! clients a Host installs before it accepts claimed Session work. The
//! transports only a database-less Worker holds (claimed commit, live-progress
//! publisher, terminal cleanup, Memory reference encoder) belong to its
//! [`crate::UpstreamAuthority`] instead.

use crate::host::*;

/// Claim-fenced Control clients. Consumer: Session realization, coordination,
/// and credential materialization on a Host that executes claimed work.
#[derive(Default)]
pub(crate) struct WorkerTransports {
    /// Outbound claim-fenced Control client used to realize an already-frozen
    /// Session projection on this Worker.
    pub(crate) session_control:
        Option<Arc<dyn awaken_dispatch_contract::ClaimedSessionRealizationControl>>,
    pub(crate) session_coordination_control:
        Option<Arc<dyn awaken_dispatch_contract::ClaimedSessionCoordinationControl>>,
    /// Sole adapter for opaque credentials owned by this Worker process. Control
    /// and Session code retain only exact non-secret references.
    pub(crate) worker_credential_resolver:
        Option<Arc<dyn awaken_runtime_contract::WorkerLocalCredentialResolver>>,
}

impl SharedHost {
    /// Install the sole outbound claim-fenced client for frozen Session
    /// realization on a Worker.
    #[must_use]
    pub fn with_session_realization_control(
        mut self,
        control: Arc<dyn awaken_dispatch_contract::ClaimedSessionRealizationControl>,
    ) -> Self {
        self.worker.session_control = Some(control);
        self
    }

    /// Install both independent claim-fenced Session roles from the canonical
    /// Worker Control client.
    #[must_use]
    pub fn with_session_control<T>(mut self, control: Arc<T>) -> Self
    where
        T: awaken_dispatch_contract::ClaimedSessionRealizationControl
            + awaken_dispatch_contract::ClaimedSessionCoordinationControl
            + 'static,
    {
        self.worker.session_control = Some(control.clone());
        self.worker.session_coordination_control = Some(control);
        self
    }

    /// Install the Worker process's one opaque local-credential adapter.
    #[must_use]
    pub fn with_worker_credential_resolver(
        mut self,
        resolver: Arc<dyn awaken_runtime_contract::WorkerLocalCredentialResolver>,
    ) -> Self {
        self.worker.worker_credential_resolver = Some(resolver);
        self
    }
}
