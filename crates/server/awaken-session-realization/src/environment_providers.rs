// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Installers of the Session environment providers the Session lifecycle realizes through;
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! The sandbox providers the realization executor realizes Sessions through: the
//! Session-owned environment provider, the trusted-host BackendOwned provider,
//! deliberately fresh housekeeping sandboxes, checkpoint custody, CacheVolume
//! preparation, and the Memory mount adapter every provider shares.

use std::path::PathBuf;
use std::sync::Arc;

/// Whether this process may realize an exact `trusted_host` Environment: the
/// boundary that preserves the local machine identity the author selected.
/// The process composition selects it once from its configuration. Consumer:
/// Session environment provider selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustedHostPolicy {
    /// `trusted_host` Environments fail closed on this process.
    Refused,
    /// The operator admitted `trusted_host` Environments on this process.
    Admitted,
}

/// Never-used warm Session capacity this process keeps ready. The process
/// composition selects it once from its configuration. Consumer: the Host's
/// warm Environment capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarmPool {
    /// No warm capacity is prepared.
    Disabled,
    /// Ready, never-used containers per exact mount-less Session shape, under a
    /// global cap across the default and every Environment shape.
    Target {
        per_shape: std::num::NonZeroUsize,
        total: usize,
    },
}

impl WarmPool {
    /// A per-shape target of zero disables warm capacity.
    #[must_use]
    pub fn new(per_shape: usize, total: usize) -> Self {
        match std::num::NonZeroUsize::new(per_shape) {
            Some(per_shape) => Self::Target { per_shape, total },
            None => Self::Disabled,
        }
    }

    /// The per-shape target and the global budget, when warm capacity is on.
    #[must_use]
    pub fn limits(self) -> Option<(usize, usize)> {
        match self {
            Self::Disabled => None,
            Self::Target { per_shape, total } => Some((per_shape.get(), total)),
        }
    }
}

/// Sandbox-provider wiring. Consumer: Session environment realization and
/// lifecycle, ACP backend selection, warm capacity, and helper sub-agents.
pub struct EnvironmentProviders {
    /// Deliberately fresh sandboxes for auxiliary Agent Runs (compactor, Memory
    /// selection/extraction, isolated Skill forks), injected by the process
    /// composition. They never share the Session-owned environment.
    pub auxiliary_sandboxes: Arc<dyn awaken_session_environment_contract::AuxiliarySandboxFactory>,
    /// Provider for the Session-owned environment shared by Native/ACP/children.
    /// Kept separate from deliberately-fresh housekeeping sandboxes.
    pub session_provider: crate::session_environment::SessionEnvironmentProvider,
    /// Deployment-owned byte custody for full-Environment checkpoints. Desired
    /// lifecycle state remains in the Session aggregate; this port stores bytes
    /// only. Hosted startup injects the regional encrypted adapter.
    pub environment_checkpoint_store:
        Option<Arc<dyn awaken_provisioning_contract::SandboxCheckpointStore>>,
    /// Product-plane single-flight preparation for caller-owned CacheVolume
    /// paths. The selected Sandbox provider remains an opaque-path consumer.
    pub cache_volume_prewarmer: crate::cache_volume::CacheVolumePrewarmer,
    /// Trusted-host environment selected only for BackendOwned provisioning.
    /// It is a policy branch over the same Session owner, not a second executor.
    pub backend_owned_session_provider:
        Option<crate::session_environment::SessionEnvironmentProvider>,
    /// The process startup installed the authoritative Session provider. ACP
    /// ACP execution must reuse it instead of constructing a deployment-derived peer.
    pub session_provider_explicit: bool,
    /// Worker-side service for governed MemoryStore mounts. The Runtime Host
    /// stores only the neutral contract; the outer server process selects
    /// the FUSE/copy adapter with the Host's dependencies.
    pub memory_mounter: Option<Arc<dyn awaken_provisioning_contract::MemoryMounter>>,
    /// Whether `trusted_host` Environments may be realized on this process.
    pub trusted_host: TrustedHostPolicy,
    /// The warm capacity this process keeps per Session shape.
    pub warm_pool: WarmPool,
    /// Final capacity participation selected once by canonical Host construction.
    pub runs_local_dispatch_pool: bool,
    /// The Session sandbox root the construction-time provider realizes under;
    /// this crate's fixtures pre-create physical sandboxes beneath it.
    #[cfg(any(test, feature = "test-support"))]
    pub sandbox_root: PathBuf,
}

impl EnvironmentProviders {
    /// The providers a Host starts with: the construction-time fallback
    /// Session provider the composition passes in, until startup installs one.
    pub fn new(
        #[cfg_attr(not(any(test, feature = "test-support")), allow(unused_variables))] sandbox_root: PathBuf,
        session_provider: crate::session_environment::SessionEnvironmentProvider,
        auxiliary_sandboxes: Arc<dyn awaken_session_environment_contract::AuxiliarySandboxFactory>,
        trusted_host: TrustedHostPolicy,
        warm_pool: WarmPool,
    ) -> Self {
        Self {
            auxiliary_sandboxes,
            #[cfg(any(test, feature = "test-support"))]
            sandbox_root,
            session_provider,
            environment_checkpoint_store: None,
            cache_volume_prewarmer: crate::cache_volume::CacheVolumePrewarmer::default(),
            backend_owned_session_provider: None,
            session_provider_explicit: false,
            memory_mounter: None,
            trusted_host,
            warm_pool,
            runs_local_dispatch_pool: false,
        }
    }
}
