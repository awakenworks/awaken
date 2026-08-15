//! Session defaults projected from one immutable executable Agent publication.
//!
//! This belongs to the Control-to-Coordinator registration boundary: Control
//! authors it once from the same publication as the executable snapshot, and the
//! Coordinator catalog serves it when a future Session freezes its baseline.

use awaken_agent_contract::{AgentSkillBinding, ClientToolDescriptor, McpTarget, ToolsetPolicy};
use awaken_resource_contract::InputBinding;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutableAgentMcpServer {
    pub name: String,
    pub target: McpTarget,
    pub prompts_as_skills: bool,
    pub credential_source_id: Option<String>,
    pub credential_revision: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutableAgentEnvironment {
    pub environment_id: String,
    pub revision: u64,
}

/// One delegate edge frozen by the coordinator publication. The target revision
/// is part of the edge, not re-selected when a later Session or child Thread is
/// projected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutableAgentDelegate {
    pub agent_id: String,
    pub source_revision: Option<u64>,
}

/// Exact Session-facing defaults frozen from one executable publication.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExecutableAgentSessionProfile {
    /// Managed Agent identity is presentation rather than executable behavior,
    /// but it still belongs to this exact publication revision and must survive
    /// Session/Thread snapshot projection.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub source_revision: u64,
    pub model: Option<String>,
    /// Provider-neutral controls frozen from the same publication as `model`.
    pub inference: awaken_runtime_contract::agent_bindings::InferenceOptions,
    pub execution_model_ref: Option<String>,
    pub backend_ref: String,
    pub system: Option<String>,
    pub tool_ids: Vec<String>,
    pub toolsets: Vec<ToolsetPolicy>,
    pub client_tools: Vec<ClientToolDescriptor>,
    pub mcp_servers: Vec<ExecutableAgentMcpServer>,
    pub skills: Vec<AgentSkillBinding>,
    #[serde(default)]
    pub delegates: Vec<ExecutableAgentDelegate>,
    pub advisor_model: Option<String>,
    pub resources: Vec<InputBinding>,
    pub environment: Option<ExecutableAgentEnvironment>,
}

/// Coordinator read port for current executable Agent Session defaults.
pub trait ExecutableAgentProfileSource: Send + Sync {
    fn session_profile_in(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Option<ExecutableAgentSessionProfile>;

    fn agent_unavailable_in(&self, _workspace_id: &str, _agent_id: &str) -> bool {
        false
    }

    /// Resolve an exact publication. Sources that only support current profiles
    /// remain compatible, but may return it only when its revision matches.
    fn session_profile_at_revision_in(
        &self,
        workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Option<ExecutableAgentSessionProfile> {
        self.session_profile_in(workspace_id, agent_id)
            .filter(|profile| profile.source_revision == source_revision)
    }

    /// Resolve the complete immutable publication behind an exact Session
    /// profile. The Coordinator uses this only as rebuildable Worker input;
    /// Control's publication store remains authoritative.
    fn executable_snapshot_at_revision_in(
        &self,
        _workspace_id: &str,
        _agent_id: &str,
        _source_revision: u64,
    ) -> Option<awaken_runtime_contract::ExecutableAgentSnapshot> {
        None
    }

    fn unavailable_delegate_in(&self, workspace_id: &str, agent_id: &str) -> Option<String> {
        self.session_profile_in(workspace_id, agent_id)?
            .delegates
            .into_iter()
            .map(|delegate| delegate.agent_id)
            .find(|delegate| self.agent_unavailable_in(workspace_id, delegate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Source;

    impl ExecutableAgentProfileSource for Source {
        fn session_profile_in(
            &self,
            workspace: &str,
            agent: &str,
        ) -> Option<ExecutableAgentSessionProfile> {
            (workspace == "workspace" && agent == "coordinator").then(|| {
                ExecutableAgentSessionProfile {
                    backend_ref: "native".into(),
                    delegates: vec![
                        ExecutableAgentDelegate {
                            agent_id: "live".into(),
                            source_revision: Some(1),
                        },
                        ExecutableAgentDelegate {
                            agent_id: "archived".into(),
                            source_revision: Some(1),
                        },
                    ],
                    ..Default::default()
                }
            })
        }

        fn agent_unavailable_in(&self, workspace: &str, agent: &str) -> bool {
            workspace == "workspace" && agent == "archived"
        }
    }

    #[test]
    fn scheduled_delegate_admission_uses_the_current_catalog_lifecycle() {
        // Cause/effect decision table: R1 missing root profile -> no denial;
        // R2 live delegates are skipped; R3 the first unavailable direct delegate
        // is returned. Exact snapshot pinning remains a separate catalog read.
        assert_eq!(Source.unavailable_delegate_in("other", "coordinator"), None);
        assert_eq!(
            Source.unavailable_delegate_in("workspace", "coordinator"),
            Some("archived".into())
        );
    }
}
