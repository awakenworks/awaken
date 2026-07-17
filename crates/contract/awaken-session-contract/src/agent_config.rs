//! The config-plane agent-projection port (ADR-0043).

/// The config-plane projection of an agent: the runtime-authoritative fields the
/// managed wire shows. A neutral view so `/v1/agents` presents an agent authored on
/// the config plane (`/v1/config/agents`) as a *projection* of that single truth
/// rather than a second copy — the "retreat to projection" direction (ADR-0043).
pub struct AgentConfigView {
    pub model: Option<String>,
    pub system: Option<String>,
    pub tool_ids: Vec<String>,
}

/// A source of config-plane agent projections. A **port**: the host implements it
/// over its `ConfigService` (the managed crate cannot depend on the host), so the
/// managed adapter reads the neutral config truth without naming it.
pub trait AgentConfigSource: Send + Sync {
    /// The config-plane view of `agent_id`, if it is published there.
    fn agent_view(&self, agent_id: &str) -> Option<AgentConfigView>;
}
