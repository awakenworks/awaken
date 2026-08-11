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

/// Exact Session-facing defaults frozen from one executable publication.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExecutableAgentSessionProfile {
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
    pub delegate_ids: Vec<String>,
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

    fn unavailable_delegate_in(&self, workspace_id: &str, agent_id: &str) -> Option<String> {
        self.session_profile_in(workspace_id, agent_id)?
            .delegate_ids
            .into_iter()
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
                    delegate_ids: vec!["live".into(), "archived".into()],
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
