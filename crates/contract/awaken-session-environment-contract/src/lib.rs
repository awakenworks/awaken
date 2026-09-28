//! Ports between the Runtime Host and the Session execution environments a
//! Worker realizes.
//!
//! The Runtime Host is shared by every process topology, so it names only
//! these ports; the process composition injects the concrete sandbox
//! implementation (`awaken-sandbox-local`) behind them.

use std::sync::Arc;

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
