use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExecutableAgentSnapshotId(pub String);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutableAgentSnapshot {
    pub id: ExecutableAgentSnapshotId,
    pub root_agent_id: AgentId,
    pub resolved_spec: crate::resolved::ResolvedSpec,
    pub fingerprint: crate::resolved::CatalogFingerprint,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(pub String);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AgentSnapshotInput {
    /// The full snapshot inline. Boxed because it dwarfs the by-id variant, the
    /// common path, which should not carry the inline payload's stack size.
    Inline(Box<ExecutableAgentSnapshot>),
    ById(ExecutableAgentSnapshotId),
}
