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
        Option<Arc<dyn awaken_session_realization_contract::ClaimedSessionRealizationControl>>,
    pub(crate) session_coordination_control: Option<Arc<crate::coordination::ClaimedCoordination>>,
    /// Sole adapter for opaque credentials owned by this Worker process. Control
    /// and Session code retain only exact non-secret references.
    pub(crate) worker_credential_resolver:
        Option<Arc<dyn awaken_credential_contract::WorkerLocalCredentialResolver>>,
}
