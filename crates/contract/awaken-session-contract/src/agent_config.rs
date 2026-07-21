//! The config-plane agent-projection port (ADR-0043).

/// The config-plane projection of an agent: the runtime-authoritative fields the
/// managed wire shows. A neutral view so `/v1/agents` presents an agent authored on
/// the config plane (`/v1/config/agents`) as a *projection* of that single truth
/// rather than a second copy — the "retreat to projection" direction (ADR-0043).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMcpServerView {
    pub name: String,
    pub url: String,
}

pub struct AgentConfigView {
    pub model: Option<String>,
    pub system: Option<String>,
    pub tool_ids: Vec<String>,
    /// Direct MCP servers inherited by Sessions of this published Agent.
    pub mcp_servers: Vec<AgentMcpServerView>,
    /// The delivered Skills selected by this Agent. Empty is an intentional empty
    /// selection for newly published configs, not "all global skills".
    pub skill_ids: Vec<String>,
    /// Resources bound to the published Agent. The runtime mounts these at Session
    /// preparation; protocol projections expose the same effective inputs.
    pub resources: Vec<awaken_resource_contract::InputBinding>,
}

/// A source of config-plane agent projections. A **port**: the host implements it
/// over its `ConfigService` (the managed crate cannot depend on the host), so the
/// managed adapter reads the neutral config truth without naming it.
pub trait AgentConfigSource: Send + Sync {
    /// The config-plane view of `agent_id` installed in `workspace_id`. There is no
    /// scope-free fallback: a projection missing its trusted Workspace must fail
    /// closed instead of searching another tenant's installed catalog.
    fn agent_view_in(&self, workspace_id: &str, agent_id: &str) -> Option<AgentConfigView>;
}
