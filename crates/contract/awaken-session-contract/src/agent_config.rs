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
    /// The config-plane view of `agent_id`, if it is published there.
    fn agent_view(&self, agent_id: &str) -> Option<AgentConfigView>;

    /// Workspace-scoped projection used when creating a Session. Implementors
    /// backed by a scoped repository must override this method; the default keeps
    /// scope-free registries source-compatible while they migrate.
    fn agent_view_in(&self, _workspace_id: &str, agent_id: &str) -> Option<AgentConfigView> {
        self.agent_view(agent_id)
    }
}
