// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Installers of the Session environment providers the Session lifecycle realizes through;
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! ENV capability of [`SharedHost`]: the Session-owned environment provider,
//! the trusted-host BackendOwned provider, deliberately fresh housekeeping
//! sandboxes, checkpoint custody, CacheVolume preparation, and the Memory
//! mount adapter every provider shares.

use crate::host::*;

/// Sandbox-provider wiring. Consumer: Session environment realization and
/// lifecycle, ACP backend selection, warm capacity, and helper sub-agents.
pub(crate) struct EnvironmentProviders {
    /// Deliberately fresh sandboxes for auxiliary Agent Runs (compactor, Memory
    /// selection/extraction, isolated Skill forks), injected by the process
    /// composition. They never share the Session-owned environment.
    pub(crate) auxiliary_sandboxes:
        Arc<dyn awaken_session_environment_contract::AuxiliarySandboxFactory>,
    /// Provider for the Session-owned environment shared by Native/ACP/children.
    /// Kept separate from deliberately-fresh housekeeping sandboxes.
    pub(crate) session_provider: crate::session_environment::SessionEnvironmentProvider,
    /// Deployment-owned byte custody for full-Environment checkpoints. Desired
    /// lifecycle state remains in the Session aggregate; this port stores bytes
    /// only. Hosted startup injects the regional encrypted adapter.
    pub(crate) environment_checkpoint_store:
        Option<Arc<dyn awaken_provisioning_contract::SandboxCheckpointStore>>,
    /// Product-plane single-flight preparation for caller-owned CacheVolume
    /// paths. The selected Sandbox provider remains an opaque-path consumer.
    pub(crate) cache_volume_prewarmer: crate::cache_volume::CacheVolumePrewarmer,
    /// Trusted-host environment selected only for BackendOwned provisioning.
    /// It is a policy branch over the same Session owner, not a second executor.
    pub(crate) backend_owned_session_provider:
        Option<crate::session_environment::SessionEnvironmentProvider>,
    /// The process startup installed the authoritative Session provider. ACP
    /// ACP execution must reuse it instead of constructing a deployment-derived peer.
    pub(crate) session_provider_explicit: bool,
    /// Worker-side service for governed MemoryStore mounts. The Runtime Host
    /// stores only the neutral contract; the outer server process selects
    /// the FUSE/copy adapter with the Host's dependencies.
    pub(crate) memory_mounter: Option<Arc<dyn awaken_provisioning_contract::MemoryMounter>>,
}

impl EnvironmentProviders {
    /// The providers a Host starts with: the construction-time fallback
    /// Session provider (`crate::uninstalled_environment`), until startup
    /// installs one.
    pub(in crate::host) fn new(
        sandbox_root: PathBuf,
        auxiliary_sandboxes: Arc<dyn awaken_session_environment_contract::AuxiliarySandboxFactory>,
    ) -> Self {
        Self {
            auxiliary_sandboxes,
            session_provider: crate::uninstalled_environment::default_session_provider(
                sandbox_root,
            ),
            environment_checkpoint_store: None,
            cache_volume_prewarmer: crate::cache_volume::CacheVolumePrewarmer::default(),
            backend_owned_session_provider: None,
            session_provider_explicit: false,
            memory_mounter: None,
        }
    }
}
