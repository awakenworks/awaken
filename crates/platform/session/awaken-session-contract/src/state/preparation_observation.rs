// @formal surface exclude=read_model_contract
// @formal reason: Secret-free, transient projection of the existing Session root; no mutation, lease or external-effect authority.
//! One preparation observation for in-process and transport consumers.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::{PersistedSession, SessionEnvironmentState, SessionExecutionState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPreparationStatus {
    Pending,
    Ready,
    Failed(SessionPreparationFailure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPreparationFailure {
    UndeclaredMcpServer,
    EnvironmentFailed,
    McpFailed,
    SessionTerminal,
    SandboxMcpWithoutEnvironment,
}

/// A view of one root, never an instruction or a retained readiness record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPreparationObservation {
    session_id: awaken_session_types::SessionId,
    configured_mcp_servers: Vec<String>,
    realized_mcp_servers: Vec<String>,
    status: SessionPreparationStatus,
}

impl SessionPreparationObservation {
    #[must_use]
    pub fn session_id(&self) -> &awaken_session_types::SessionId {
        &self.session_id
    }
    #[must_use]
    pub fn configured_mcp_servers(&self) -> &[String] {
        &self.configured_mcp_servers
    }
    #[must_use]
    pub fn realized_mcp_servers(&self) -> &[String] {
        &self.realized_mcp_servers
    }
    #[must_use]
    pub fn status(&self) -> SessionPreparationStatus {
        self.status
    }

    #[must_use]
    pub fn from_session(session: &PersistedSession) -> Self {
        let configured = session
            .configured_mcp_servers()
            .into_iter()
            .map(|server| server.name)
            .collect::<BTreeSet<_>>();
        let realized = session
            .visible_mcp_servers()
            .into_iter()
            .map(|server| server.name)
            .collect::<BTreeSet<_>>();
        let resident = matches!(
            &session.environment,
            SessionEnvironmentState::Resident { .. }
        );
        let deferred = session.frozen_baseline().is_some_and(|baseline| {
            baseline.environment.sandbox_provisioning
                == awaken_provisioning_contract::SandboxProvisioning::OnToolUse
        }) && matches!(
            &session.environment,
            SessionEnvironmentState::Unmaterialized
        );
        let sandbox_mcp = session
            .mcp
            .desired_attachments()
            .iter()
            .any(|attachment| attachment.target.sandbox_stdio_target().is_some());
        let failure = if !realized.is_subset(&configured) {
            Some(SessionPreparationFailure::UndeclaredMcpServer)
        } else if session.realization_progress().failed_at_unix_ms().is_some() {
            Some(SessionPreparationFailure::EnvironmentFailed)
        } else if session.mcp.desired_attachments().iter().any(|attachment| {
            attachment.state
                == awaken_session_configuration_contract::mcp::McpAttachmentState::Failed
        }) {
            Some(SessionPreparationFailure::McpFailed)
        } else if session.is_terminal() {
            Some(SessionPreparationFailure::SessionTerminal)
        } else if deferred && sandbox_mcp && realized == configured {
            Some(SessionPreparationFailure::SandboxMcpWithoutEnvironment)
        } else {
            None
        };
        let status = preparation_status(
            failure,
            session.execution == SessionExecutionState::Idle,
            realized == configured,
            resident,
            deferred,
            sandbox_mcp,
        );
        Self {
            session_id: session.session_id.clone(),
            configured_mcp_servers: configured.into_iter().collect(),
            realized_mcp_servers: realized.into_iter().collect(),
            status,
        }
    }
}

fn preparation_status(
    failure: Option<SessionPreparationFailure>,
    idle: bool,
    exact_mcp: bool,
    resident: bool,
    deferred: bool,
    sandbox_mcp: bool,
) -> SessionPreparationStatus {
    if let Some(reason) = failure {
        SessionPreparationStatus::Failed(reason)
    } else if idle && exact_mcp && (resident || (deferred && !sandbox_mcp)) {
        SessionPreparationStatus::Ready
    } else {
        SessionPreparationStatus::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preparation_observation_exhausts_the_readiness_decision_table() {
        // Causes F=failure, I=idle, M=exact realized MCP, R=resident,
        // D=deferred/unmaterialized, S=sandbox MCP. Effects: E1 F -> failed;
        // E2 !F+I+M+(R | D+!S) -> ready; E3 every remaining row -> pending.
        // Exhaust all64 combinations, including malformed R+D: this projection
        // grants no permission, so the root's separate transition laws remain
        // authoritative. A ready bit is never inferred from transport success.
        for bits in 0u8..64 {
            let b = |index: u32| bits & (1u8 << index) != 0;
            let actual = preparation_status(
                b(0).then_some(SessionPreparationFailure::EnvironmentFailed),
                b(1),
                b(2),
                b(3),
                b(4),
                b(5),
            );
            // Independently enumerated ready rows from E2's decision table:
            // resident14,30,46,62; deferred without sandbox22 (30 overlaps).
            let expected = if bits % 2 == 1 {
                SessionPreparationStatus::Failed(SessionPreparationFailure::EnvironmentFailed)
            } else if [14, 22, 30, 46, 62].contains(&bits) {
                SessionPreparationStatus::Ready
            } else {
                SessionPreparationStatus::Pending
            };
            assert_eq!(actual, expected, "decision row {bits}");
        }
    }
}
