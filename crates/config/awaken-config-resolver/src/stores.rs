//! Read ports for the authored control-plane aggregates — the store traits the
//! resolver and the runtime host consult at bind time, plus in-memory reference
//! impls.
//!
//! These live here, with the aggregate types they carry, so the **read** path
//! stays in the open resolver crate and never reaches back up into the authoring
//! HTTP surface. `awaken-admin-config-api` (the authoring plane) writes *through*
//! these same ports and provides the durable SQLite backend for them; the
//! runtime host reads *through* them at session-prepare time.

use std::collections::HashMap;

use crate::{AgentMcpConfig, AgentResourceConfig, InferenceProfile, McpServerDef};

/// A store for authored [`InferenceProfile`]s (an admin-plane aggregate). Sync +
/// in-memory by default; a durable backend can implement the same port.
pub trait InferenceProfileStore: Send + Sync {
    fn put(&self, id: String, profile: InferenceProfile);
    fn get(&self, id: &str) -> Option<InferenceProfile>;
}

/// The default in-memory [`InferenceProfileStore`].
#[derive(Default)]
pub struct InMemoryProfileStore(std::sync::Mutex<HashMap<String, InferenceProfile>>);

impl InMemoryProfileStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl InferenceProfileStore for InMemoryProfileStore {
    fn put(&self, id: String, profile: InferenceProfile) {
        self.0.lock().expect("profiles").insert(id, profile);
    }
    fn get(&self, id: &str) -> Option<InferenceProfile> {
        self.0.lock().expect("profiles").get(id).cloned()
    }
}

/// A store for authored [`McpServerDef`]s (by server id) and per-agent
/// [`AgentMcpConfig`] bindings (by agent id) — the admin-plane MCP aggregates.
/// Sync + in-memory by default; a durable backend can implement the same port.
pub trait McpStore: Send + Sync {
    fn put_server(&self, def: McpServerDef);
    fn get_server(&self, id: &str) -> Option<McpServerDef>;
    fn list_servers(&self) -> Vec<McpServerDef>;
    fn put_agent_config(&self, config: AgentMcpConfig);
    fn get_agent_config(&self, agent_id: &str) -> Option<AgentMcpConfig>;
}

/// The default in-memory [`McpStore`].
#[derive(Default)]
pub struct InMemoryMcpStore {
    servers: std::sync::Mutex<HashMap<String, McpServerDef>>,
    agents: std::sync::Mutex<HashMap<String, AgentMcpConfig>>,
}

impl InMemoryMcpStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl McpStore for InMemoryMcpStore {
    fn put_server(&self, def: McpServerDef) {
        self.servers
            .lock()
            .expect("mcp servers")
            .insert(def.id.0.clone(), def);
    }
    fn get_server(&self, id: &str) -> Option<McpServerDef> {
        self.servers.lock().expect("mcp servers").get(id).cloned()
    }
    fn list_servers(&self) -> Vec<McpServerDef> {
        let mut servers: Vec<McpServerDef> = self
            .servers
            .lock()
            .expect("mcp servers")
            .values()
            .cloned()
            .collect();
        servers.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        servers
    }
    fn put_agent_config(&self, config: AgentMcpConfig) {
        self.agents
            .lock()
            .expect("agent mcp configs")
            .insert(config.agent_id.clone(), config);
    }
    fn get_agent_config(&self, agent_id: &str) -> Option<AgentMcpConfig> {
        self.agents
            .lock()
            .expect("agent mcp configs")
            .get(agent_id)
            .cloned()
    }
}

/// A store for per-agent [`AgentResourceConfig`] bindings (ADR-0038) — which
/// resources an agent mounts. Sync + in-memory by default; the SQLite backend
/// implements the same port over a scoped-migration table.
pub trait ResourceStore: Send + Sync {
    fn put_agent_resource(&self, config: AgentResourceConfig);
    fn get_agent_resource(&self, agent_id: &str) -> Option<AgentResourceConfig>;
}

/// The default in-memory [`ResourceStore`], keyed by agent id.
#[derive(Default)]
pub struct InMemoryResourceStore(std::sync::Mutex<HashMap<String, AgentResourceConfig>>);

impl InMemoryResourceStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl ResourceStore for InMemoryResourceStore {
    fn put_agent_resource(&self, config: AgentResourceConfig) {
        self.0
            .lock()
            .expect("agent resource configs")
            .insert(config.agent_id.clone(), config);
    }
    fn get_agent_resource(&self, agent_id: &str) -> Option<AgentResourceConfig> {
        self.0
            .lock()
            .expect("agent resource configs")
            .get(agent_id)
            .cloned()
    }
}
