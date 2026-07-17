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

use serde::{Deserialize, Serialize};

use crate::{
    AgentMcpConfig, AgentResourceConfig, InferenceProfile, McpServerDef, WebhookEndpointDef,
};

/// The **identity** of an authored memory store (ADR-0038 MemoryStore family): its id,
/// name, description, free-form metadata, and an archived flag. This is a control-plane
/// aggregate — the durable metadata that mirrors the `McpServerDef` pattern — kept
/// distinct from the store's *content* (the path-addressed memories, which live in the
/// data-plane `MemoryFs`). Persisting this def is what lets a store's identity survive a
/// restart and be enumerated from the control plane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryStoreDef {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub metadata: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub archived: bool,
}

/// A registry of authored [`MemoryStoreDef`]s (by store id) — the memory-store identity
/// aggregate, mirroring [`McpStore`]. Sync + in-memory by default; the durable admin
/// backend implements the same port over the `admin` migration bundle, so a memory
/// store's identity/metadata survives a restart and the admin assistant can enumerate
/// stores. Content lives elsewhere (the data-plane `MemoryFs`).
pub trait MemoryStoreRegistry: Send + Sync {
    fn put_memory_store(&self, def: MemoryStoreDef);
    fn get_memory_store(&self, id: &str) -> Option<MemoryStoreDef>;
    /// Non-archived stores, sorted by id (the enumeration contract).
    fn list_memory_stores(&self) -> Vec<MemoryStoreDef>;
}

/// The default in-memory [`MemoryStoreRegistry`], keyed by store id.
#[derive(Default)]
pub struct InMemoryMemoryStoreRegistry(
    std::sync::Mutex<std::collections::BTreeMap<String, MemoryStoreDef>>,
);

impl InMemoryMemoryStoreRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl MemoryStoreRegistry for InMemoryMemoryStoreRegistry {
    fn put_memory_store(&self, def: MemoryStoreDef) {
        self.0
            .lock()
            .expect("memory stores")
            .insert(def.id.clone(), def);
    }
    fn get_memory_store(&self, id: &str) -> Option<MemoryStoreDef> {
        self.0.lock().expect("memory stores").get(id).cloned()
    }
    fn list_memory_stores(&self) -> Vec<MemoryStoreDef> {
        // Non-archived, sorted by id (the BTreeMap already orders by key).
        self.0
            .lock()
            .expect("memory stores")
            .values()
            .filter(|d| !d.archived)
            .cloned()
            .collect()
    }
}

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

/// A store for authored [`WebhookEndpointDef`]s (an admin-plane aggregate,
/// ADR-0048). Sync + in-memory by default; the durable admin backend implements
/// the same port. Unlike [`McpStore`]/[`InferenceProfileStore`] it enumerates by
/// workspace (dispatch fan-out) and supports delete (unsubscribe).
pub trait WebhookStore: Send + Sync {
    fn put(&self, def: WebhookEndpointDef);
    fn get(&self, id: &str) -> Option<WebhookEndpointDef>;
    /// Every endpoint owned by `workspace_id` (including disabled), for CRUD list
    /// and dispatch matching.
    fn list(&self, workspace_id: &str) -> Vec<WebhookEndpointDef>;
    /// Remove by id; `true` if a row was removed (idempotent unsubscribe).
    fn delete(&self, id: &str) -> bool;
}

/// The default in-memory [`WebhookStore`], keyed by endpoint id.
#[derive(Default)]
pub struct InMemoryWebhookStore(std::sync::Mutex<HashMap<String, WebhookEndpointDef>>);

impl InMemoryWebhookStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl WebhookStore for InMemoryWebhookStore {
    fn put(&self, def: WebhookEndpointDef) {
        self.0.lock().expect("webhooks").insert(def.id.clone(), def);
    }
    fn get(&self, id: &str) -> Option<WebhookEndpointDef> {
        self.0.lock().expect("webhooks").get(id).cloned()
    }
    fn list(&self, workspace_id: &str) -> Vec<WebhookEndpointDef> {
        let mut rows: Vec<WebhookEndpointDef> = self
            .0
            .lock()
            .expect("webhooks")
            .values()
            .filter(|d| d.workspace_id == workspace_id)
            .cloned()
            .collect();
        rows.sort_by(|a, b| a.id.cmp(&b.id));
        rows
    }
    fn delete(&self, id: &str) -> bool {
        self.0.lock().expect("webhooks").remove(id).is_some()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn def(id: &str, archived: bool) -> MemoryStoreDef {
        MemoryStoreDef {
            id: id.to_string(),
            name: format!("{id} name"),
            description: String::new(),
            metadata: std::collections::BTreeMap::new(),
            archived,
        }
    }

    #[test]
    fn memory_registry_round_trips_and_lists_non_archived_sorted() {
        let reg = InMemoryMemoryStoreRegistry::new();
        assert!(reg.get_memory_store("mem-1").is_none());
        reg.put_memory_store(def("zeta", false));
        reg.put_memory_store(def("alpha", false));
        reg.put_memory_store(def("gamma", true)); // archived → excluded from list

        assert_eq!(reg.get_memory_store("zeta").unwrap().name, "zeta name");
        let ids: Vec<String> = reg.list_memory_stores().into_iter().map(|d| d.id).collect();
        assert_eq!(ids, vec!["alpha".to_string(), "zeta".to_string()]);

        // Upsert (archive) removes it from the listing but keeps it retrievable.
        reg.put_memory_store(def("zeta", true));
        let ids: Vec<String> = reg.list_memory_stores().into_iter().map(|d| d.id).collect();
        assert_eq!(ids, vec!["alpha".to_string()]);
        assert!(reg.get_memory_store("zeta").unwrap().archived);
    }

    // ---- SEC: WebhookStore tenant fence + delete/disabled semantics ----

    fn webhook(id: &str, workspace: &str, disabled: bool) -> WebhookEndpointDef {
        WebhookEndpointDef {
            id: id.to_string(),
            workspace_id: workspace.to_string(),
            url: format!("https://example.test/{id}"),
            event_types: Vec::new(),
            disabled,
            secret_ref: awaken_credential_vault::SecretRef("whsec".into()),
        }
    }

    #[test]
    fn webhook_store_lists_by_workspace_including_disabled_and_deletes_idempotently() {
        let store = InMemoryWebhookStore::new();
        store.put(webhook("wh-a1", "wrkspc_a", false));
        store.put(webhook("wh-a2", "wrkspc_a", true)); // disabled, still enumerated
        store.put(webhook("wh-b1", "wrkspc_b", false));

        // `list` is workspace-filtered (dispatch fan-out is per workspace), sorted
        // by id, and INCLUDES disabled rows (CRUD list surfaces suspended ones).
        let ids_a: Vec<String> = store.list("wrkspc_a").into_iter().map(|d| d.id).collect();
        assert_eq!(ids_a, vec!["wh-a1".to_string(), "wh-a2".to_string()]);
        // Workspace B's endpoint is never disclosed to A (the tenant fence).
        assert!(
            !ids_a.iter().any(|id| id == "wh-b1"),
            "workspace A must not see workspace B's webhook: {ids_a:?}"
        );
        // The disabled row is present with its flag intact (list ≠ dispatch filter).
        assert!(
            store
                .list("wrkspc_a")
                .iter()
                .find(|d| d.id == "wh-a2")
                .unwrap()
                .disabled
        );

        // `delete` is an idempotent unsubscribe: true once, false thereafter.
        assert!(store.delete("wh-a1"), "first delete removes a row");
        assert!(!store.delete("wh-a1"), "second delete is a no-op → false");
        assert!(store.get("wh-a1").is_none());
        // Deleting an unknown id is also false (never panics, never fabricates).
        assert!(!store.delete("ghost"));

        // The surviving (disabled) row is still listed and reachable.
        let remaining: Vec<String> = store.list("wrkspc_a").into_iter().map(|d| d.id).collect();
        assert_eq!(remaining, vec!["wh-a2".to_string()]);
        assert!(store.list("wrkspc_b").iter().any(|d| d.id == "wh-b1"));
    }
}
