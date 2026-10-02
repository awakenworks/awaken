//! Ports between the Runtime Host and the Session execution environments a
//! Worker realizes.
//!
//! The Runtime Host is shared by every process topology, so it names only
//! these ports; the process composition injects the concrete sandbox-backed
//! implementation (`awaken-session-environment`, over `awaken-sandbox-local`
//! and the container contract) behind them. The implementation depends only on
//! contracts and Worker adapters; the Runtime Host links it only as a
//! dev-dependency for its own tests.
//!
//! # Design: one port per consuming role
//!
//! A Session owns one realized environment, and one provider realizes it.
//! Neither is a single wide interface: each is split by *who calls it*, and
//! every Host module depends on the one role it uses. The Host carries a
//! realized environment as a record of handles — one `Arc<dyn Role>` per
//! role, all over the same adapter — so passing the environment around never
//! widens what a consumer may call. The provider is carried the same way.
//!
//! Realized-environment roles:
//!
//! | Role | Consumers |
//! |---|---|
//! | `EnvironmentLifecycle` — durable handle, stop/quiesce bound processes, unattached disposal, fenced prepare/dispose, checkpoint and terminal checkpoint cleanup, Memory reconciliation acknowledgement | Session environment lifecycle owner, realization, terminal preparation and cleanup, background-task ownership |
//! | `EnvironmentProjection` — capabilities, owned-path reservation, live mount validation and update fence, attach and adopted-mount reconciliation, projection-path removal, runtime workspace writes | managed Resource projection, realization, provisioning |
//! | `SkillFilesystem` — Skill directory interpretation, scan/register/refresh, read-only tree materialization | Skill delivery and discovery |
//! | `EnvironmentHarvest` — Artifact capture, frozen-mount read-back | Artifact harvest, terminal preparation |
//! | `AgentToolEnvironment` — bound tool executor, rooted tools, Agent filesystem view, host-identity support | Native Agent runs (Session context, root and child runners, child substrate, runtime build), Worker projection synchronizer |
//! | `AgentProcessEnvironment` — ACP process launch, config home, workspace cwd, inline materialization | ACP channel source, Session MCP attachment |
//! | `ScopedExecution` (Linux TrustedHost only) — execution-authority binding and refresh | execution-authority binding, realization renewal, revocation retirement |
//! | `RepositoryRealizer` (provisioning contract) | Repository activation |
//!
//! Provider roles:
//!
//! | Role | Consumers |
//! |---|---|
//! | `EnvironmentRealizer` — capabilities, host identity, effective spec, fenced create | Session realization, Worker projection synchronizer |
//! | `EnvironmentAdoption` — adoption layout, observation and closed-evidence validation, fenced adopt, retained rebind | environment lifecycle recovery, terminal preparation |
//! | `EnvironmentTerminalRealization` — terminal reconstruction, exact restore, restored-target disposal | terminal preparation and cleanup, environment continuation |
//! | `EnvironmentCapacity` — image prewarm, capacity prewarm/discard/ready/shutdown | warm capacity |
//! | `EnvironmentProviderInstallation` — Memory mounter and secret broker installation | Host startup |
//!
//! The provider that realized an environment may recover its own adapter from
//! the handles to rebind or observe it; the Host never does.
//!
//! Auxiliary Agent Runs (judge, compactor, Memory selection and extraction,
//! isolated Skill forks) use the separate [`AuxiliarySandboxFactory`] port:
//! they never share the Session-owned environment.

use std::sync::Arc;

mod acp;
mod agent_view;
mod files;
mod hand;
mod process;
mod proofs;
mod provider;
mod provider_effect;
mod realized;

pub use acp::{AcpSessionExecution, AcpSessionExecutorFactory};
pub use agent_view::{AgentFilesystemClient, AgentFilesystemView, AgentPathView};
pub use files::{CapturedArtifact, DiscoveredSkillFile};
pub use hand::{ContainerHandResidency, HandExecutorFactory};
pub use process::ProcessReaper;
pub use proofs::{EnvironmentQuiescenceProof, EnvironmentStopProof, HandStopProof};
pub use provider::{
    EnvironmentAdoption, EnvironmentCapacity, EnvironmentProviderInstallation,
    EnvironmentRealizationKind, EnvironmentRealizer, EnvironmentTerminalRealization,
    ObservationPurpose, SessionEnvironmentProvider,
};
pub use provider_effect::{EnvironmentAdoptionLayout, ProviderEffect};
pub use realized::{
    AgentProcessEnvironment, AgentToolEnvironment, EnvironmentHarvest, EnvironmentLifecycle,
    EnvironmentProjection, LiveProjectionUpdate, ScopedExecution, SessionEnvironment,
    SkillFilesystem,
};

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_tool_contract::tool::RawTool;

/// One sandbox realized for exactly one auxiliary Agent Run, exposing the
/// in-process tools rooted in it. Consumer: the Host's auxiliary Agent runner,
/// which registers these tools on the auxiliary Run's runtime.
pub trait RootedToolSandbox: Send + Sync {
    /// The sandbox's rooted in-process tools, jailed to its root.
    fn rooted_tools(&self) -> Vec<Arc<dyn RawTool>>;
}

/// Realizes deliberately fresh sandboxes for auxiliary Agent Runs (Outcome
/// judge, compactor, Memory selection and extraction, an isolated Skill fork).
/// These never share the Session-owned environment and carry no Session
/// effect fence. Consumer: the Host's auxiliary Agent runner.
#[async_trait]
pub trait AuxiliarySandboxProvider: Send + Sync {
    /// Realize one fresh sandbox for `spec`.
    async fn create_rooted_sandbox(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<Box<dyn RootedToolSandbox>, pc::SandboxError>;
}

/// Mints the auxiliary sandbox provider for one purpose, each at its own fresh
/// root, so auxiliary Runs of different purposes never share a filesystem.
/// Consumer: the Runtime Host, which the process composition hands one factory.
pub trait AuxiliarySandboxFactory: Send + Sync {
    /// A provider at a fresh root dedicated to `purpose` (for example
    /// `compact`, `mem-select`, `mem`, `skill-fork`).
    fn provider(&self, purpose: &str) -> Arc<dyn AuxiliarySandboxProvider>;
}

/// What the Workdir tier enforces: nothing beyond a lexical root, so the
/// neutral default. One source for the provider that realizes the tier and
/// for the Worker manifest projected before any provider exists.
#[must_use]
pub fn workdir_capabilities() -> pc::SandboxCapabilities {
    pc::SandboxCapabilities::default()
}

/// What the local Namespace tier enforces (bubblewrap on Linux, Seatbelt on
/// macOS). One source for the provider that realizes the tier and for the
/// Worker manifest projected before any provider exists.
#[must_use]
pub fn namespace_capabilities() -> pc::SandboxCapabilities {
    pc::SandboxCapabilities {
        process_tree_supervision: false,
        isolation: pc::IsolationClass::Namespace,
        tool_transparent: true,
        // Seatbelt is a policy boundary, not a mount namespace: it cannot make
        // host paths appear at Linux-style /workspace or /mnt paths. The launcher
        // translates cwd/known argv paths and exports real host paths instead.
        path_fidelity: cfg!(target_os = "linux"),
        enforced_readonly: true,
        network_isolation: true,
        enforced_network_allowlist: false,
        secret_egress_substitution: false,
        resource_limits: false,
        custom_rootfs: false,
        package_provisioning: false,
        control_services: if cfg!(target_os = "linux") {
            std::collections::BTreeSet::from([
                pc::SandboxControlServiceKind::RepositoryGitCredential,
            ])
        } else {
            Default::default()
        },
    }
}
