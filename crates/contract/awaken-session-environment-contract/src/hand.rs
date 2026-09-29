// @formal surface obligation=worker.sandbox_tool_recovery_is_a_hard_claim_axis
// @formal reason: Owns the container Hand residency vocabulary and its exhaustive mapping to the tool
// @formal reason: recovery capability that Worker registration advertises and the Session environment
// @formal reason: executor honours; the claim axis itself is proved where the manifest is matched.
//! The Session Hand: how a sandbox-resident tool channel is bound.

use std::sync::Arc;

use awaken_agent_channel::AgentChannel;
use awaken_tool_contract::tool::{ToolExecutor, ToolRecoveryCapability};

/// Binds a live Hand channel to the runtime's tool executor. The framing
/// implementation belongs to an outer startup crate; the environment owns only
/// the Session lifecycle and never imports the relay adapter. Consumer: the
/// Session environment implementation, which the Worker composition hands one.
pub trait HandExecutorFactory: Send + Sync {
    fn bind(
        &self,
        channel: Box<dyn AgentChannel>,
        operation_scope: &str,
        recovery: ToolRecoveryCapability,
    ) -> Arc<dyn ToolExecutor>;
}

/// Lifetime of the Session container's Hand role. One enum drives both the Pod
/// process and the Runtime Host binding, preventing parallel Hand owners.
/// Consumers: the Deployment that selects it, Worker registration, and the
/// container environment that runs it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ContainerHandResidency {
    /// Launch a rebuildable `hand --stdio` process for the current Worker.
    #[default]
    AttachedExec,
    /// Run the Hand as the Session container process and reopen its private
    /// provider channel after Worker replacement.
    Resident,
}

impl ContainerHandResidency {
    /// Recovery evidence shared by Worker registration and the realized
    /// Session environment executor. Keeping this exhaustive mapping here makes
    /// a new residency mode a compile error until both paths agree.
    #[must_use]
    pub const fn recovery_capability(self) -> ToolRecoveryCapability {
        match self {
            Self::AttachedExec => ToolRecoveryCapability::NonRecoverable,
            Self::Resident => ToolRecoveryCapability::DurableRequest,
        }
    }
}

impl std::str::FromStr for ContainerHandResidency {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "attached_exec" | "attached" => Ok(Self::AttachedExec),
            "resident" => Ok(Self::Resident),
            other => Err(format!(
                "invalid container_hand_residency={other:?}: expected attached_exec or resident"
            )),
        }
    }
}
