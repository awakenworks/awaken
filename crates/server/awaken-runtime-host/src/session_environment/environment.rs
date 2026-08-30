//! One realized Sandbox shared by every Run attempt in a Session.

use std::sync::Arc;

use awaken_provisioning_contract as pc;
use awaken_run_executor_acp::AgentChannelType;
use awaken_runtime_contract::tool::{RawTool, RawToolRegistry, ToolExecutor};
use awaken_sandbox_local::{LocalSandbox, NamespaceSandbox};

use super::HandExecutorFactory;
use super::container_skills::ContainerSkillCache;
use super::session_hand::{HandProjectionUpdate, HandStopProof, SessionHandExecutor};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnvironmentStopProof {
    NoBoundProcesses,
    Hand(HandStopProof),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EnvironmentQuiescenceProof;

/// One realized sandbox shared by every Run attempt in a Session.
pub(crate) enum SessionEnvironment {
    Workdir(Arc<LocalSandbox>),
    Namespace {
        sandbox: Arc<NamespaceSandbox>,
        hand: Arc<SessionHandExecutor>,
    },
    Container {
        sandbox: Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        hand: Arc<SessionHandExecutor>,
        skills: Arc<ContainerSkillCache>,
        capabilities: pc::SandboxCapabilities,
    },
}

impl SessionEnvironment {
    #[must_use]
    pub(crate) fn workdir(sandbox: LocalSandbox) -> Self {
        Self::Workdir(Arc::new(sandbox))
    }

    #[must_use]
    pub(crate) fn namespace(
        sandbox: NamespaceSandbox,
        hand_factory: Arc<dyn HandExecutorFactory>,
        hand_bin: impl Into<String>,
        hand_idle_after: std::time::Duration,
    ) -> Self {
        let sandbox = Arc::new(sandbox);
        let hand = Arc::new(SessionHandExecutor::namespace(
            sandbox.clone(),
            hand_factory,
            hand_bin,
            hand_idle_after,
        ));
        Self::Namespace { sandbox, hand }
    }

    pub(super) async fn container(
        sandbox: Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        hand_factory: Arc<dyn HandExecutorFactory>,
        hand_bin: &str,
        hand_idle_after: std::time::Duration,
        hand_residency: crate::deployment_config::ContainerHandResidency,
        capabilities: pc::SandboxCapabilities,
    ) -> Result<Self, pc::SandboxError> {
        let skills = Arc::new(ContainerSkillCache::default());
        let hand = Arc::new(
            SessionHandExecutor::container(
                sandbox.clone(),
                skills.clone(),
                hand_factory,
                hand_bin,
                hand_idle_after,
                hand_residency,
            )
            .await?,
        );
        Ok(Self::Container {
            sandbox,
            hand,
            skills,
            capabilities,
        })
    }

    /// Exact capability evidence of the provider that created this live
    /// environment. Resource hot-plug admission must inspect the resident
    /// environment rather than a process default that may select another tier.
    pub(crate) fn capabilities(&self) -> pc::SandboxCapabilities {
        match self {
            Self::Workdir(_) => awaken_sandbox_local::LocalProvider::capabilities(),
            Self::Namespace { .. } => awaken_sandbox_local::NamespaceProvider::capabilities(),
            Self::Container { capabilities, .. } => capabilities.clone(),
        }
    }

    /// Validate a complete replacement manifest against the resident backend's
    /// mount guarantees and hot-plug support before any projection is changed.
    pub(crate) fn validate_live_mount_replacement(
        &self,
        previous: &[pc::MountRequirement],
        next: &[pc::MountRequirement],
    ) -> Result<(), pc::SandboxError> {
        pc::validate_mount_requirements(next, &self.capabilities())
            .map_err(|error| pc::SandboxError::new(error.to_string()))?;
        if previous != next
            && let Self::Container { sandbox, .. } = self
            && !sandbox.supports_live_mount_replacement(previous, next)
        {
            return Err(pc::SandboxError::new(
                "late mount replacement is unsupported for this container input set",
            ));
        }
        Ok(())
    }

    /// Fence tool dispatch and hibernate the current sandbox-resident Hand before
    /// changing the live resource projection. Workdir has no process-level path
    /// fidelity and retains its existing host-side structured-tool behavior.
    pub(crate) async fn begin_live_projection_update(
        &self,
    ) -> Result<Option<HandProjectionUpdate<'_>>, pc::SandboxError> {
        match self {
            Self::Namespace { hand, .. } | Self::Container { hand, .. } => {
                hand.begin_projection_update().await.map(Some)
            }
            Self::Workdir(_) => Ok(None),
        }
    }

    /// Executable Hand for this realized environment. Namespace and Container
    /// execute through one sandbox-resident channel; only the explicitly weaker
    /// Workdir tier uses host-side rooted tools.
    pub(crate) fn tool_executor(&self) -> Arc<dyn ToolExecutor> {
        match self {
            Self::Namespace { hand, .. } | Self::Container { hand, .. } => hand.clone(),
            Self::Workdir(_) => Arc::new(RawToolRegistry::new(self.rooted_tools())),
        }
    }

    pub(crate) fn rooted_tools(&self) -> Vec<Arc<dyn RawTool>> {
        match self {
            Self::Workdir(sandbox) => sandbox.rooted_tools(),
            // Descriptors remain the canonical built-in set; execution is forced
            // through this environment's bound Hand in `SessionCtx`.
            Self::Namespace { .. } | Self::Container { .. } => {
                awaken_ext_builtin_tools::all_hand_tools()
            }
        }
    }

    /// Stop only the process bindings created while constructing this wrapper.
    /// Used when an adoption races a resident environment with the same handle;
    /// disposing here would incorrectly destroy the shared underlying container.
    pub(crate) async fn stop_bound_processes(
        &self,
    ) -> Result<EnvironmentStopProof, pc::SandboxError> {
        match self {
            Self::Namespace { hand, .. } | Self::Container { hand, .. } => {
                hand.stop().await.map(EnvironmentStopProof::Hand)
            }
            Self::Workdir(_) => Ok(EnvironmentStopProof::NoBoundProcesses),
        }
    }

    pub(crate) async fn spawn_agent(
        &self,
        command: pc::Command,
    ) -> Result<(Box<dyn pc::ProcessHandle>, Box<dyn AgentChannelType>), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => sandbox.spawn_agent(command).await,
            Self::Namespace { hand, .. } | Self::Container { hand, .. } => {
                hand.launcher().spawn_agent(command).await
            }
        }
    }
}
