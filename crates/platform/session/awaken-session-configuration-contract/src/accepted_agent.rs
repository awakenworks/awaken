//! Immutable effective Agent configuration accepted by one Session.

use awaken_runtime_contract::agent_bindings::InferenceOptions;

/// Where the frozen model choice came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionModelSource {
    /// The request's own override.
    Override,
    /// The Agent publication's model and inference controls.
    Publication,
    /// The actually selected process default, never a publication receipt.
    Default,
}

/// The model a new Session freezes, when one was selected.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionModelChoice {
    pub model_id: String,
    pub inference: InferenceOptions,
    pub source: SessionModelSource,
}

/// Secret-free active MCP projection consumed by protocol adapters. It is
/// derived from this aggregate's typed attachment state without a JSON hop.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VisibleMcpServer {
    pub name: String,
    pub target: awaken_agent_contract::McpTarget,
    pub prompts_as_skills: bool,
}

/// Complete accepted Session configuration retained for deterministic history.
/// This passive value is not an Agent authoring model or an execution authority.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionAgentConfiguration {
    pub agent_id: String,
    pub version: u64,
    pub model: SessionModelChoice,
    pub name: String,
    pub description: Option<String>,
    pub system: Option<String>,
    pub tools: crate::SessionToolConfiguration,
    pub mcp_servers: Vec<VisibleMcpServer>,
    pub skills: Vec<awaken_agent_contract::AgentSkillBinding>,
    pub multiagent: Option<SessionAgentRoster>,
}

impl SessionAgentConfiguration {
    /// Prepare presentation from already selected creation values, before Root compilation.
    /// This performs no catalog lookup, admission, or execution selection. MCP is
    /// unrecorded here: the existing compiled Root supplies its canonical desired
    /// projection in the one pre-insert recording operation. This seed is never
    /// a history fact or an execution/authorization input.
    #[must_use]
    pub fn creation_presentation_seed(
        control: &crate::ControlSessionCreationInputs,
        version: u64,
        model: Option<SessionModelChoice>,
        presentation: (Option<&str>, Option<&str>),
        inherited_system: Option<String>,
        tools: &crate::SessionToolConfiguration,
        skills: Vec<awaken_agent_contract::AgentSkillBinding>,
    ) -> Self {
        Self {
            agent_id: control.agent_id.clone(),
            version,
            model: model.unwrap_or_else(|| SessionModelChoice {
                model_id: control.model.clone(),
                inference: Default::default(),
                source: SessionModelSource::Default,
            }),
            name: presentation.0.unwrap_or(&control.agent_id).to_owned(),
            description: presentation.1.map(str::to_owned),
            system: control.system_prompt.resolve(inherited_system),
            tools: tools.clone(),
            mcp_servers: Vec::new(),
            skills,
            multiagent: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionAgentRoster {
    pub members: Vec<SessionAgentRosterMember>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "configuration", rename_all = "snake_case")]
pub enum SessionAgentRosterMember {
    Agent(Box<SessionAgentConfiguration>),
    Advisor(String),
}
