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
    /// Insert one logical lifecycle event if absent. The stable event id is the
    /// idempotency key across dispatcher retries and process restarts.
    fn enqueue_outbox(&self, event: WebhookOutboxEvent) -> bool;
    fn pending_outbox(&self) -> Vec<WebhookOutboxEvent>;
    fn complete_outbox(&self, event_id: &str) -> bool;
}

/// Secret-free durable webhook outbox row. Subscription secrets are resolved
/// only at dispatch time; this row is safe to persist in the admin database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookOutboxEvent {
    pub id: String,
    pub created_at: String,
    pub event_type: String,
    pub object_id: String,
    pub workspace_id: String,
    pub organization_id: Option<String>,
    pub timestamp: i64,
}

/// The default in-memory [`WebhookStore`], keyed by endpoint id.
#[derive(Default)]
pub struct InMemoryWebhookStore {
    endpoints: std::sync::Mutex<HashMap<String, WebhookEndpointDef>>,
    outbox: std::sync::Mutex<HashMap<String, WebhookOutboxEvent>>,
}

impl InMemoryWebhookStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl WebhookStore for InMemoryWebhookStore {
    fn put(&self, def: WebhookEndpointDef) {
        self.endpoints
            .lock()
            .expect("webhooks")
            .insert(def.id.clone(), def);
    }
    fn get(&self, id: &str) -> Option<WebhookEndpointDef> {
        self.endpoints.lock().expect("webhooks").get(id).cloned()
    }
    fn list(&self, workspace_id: &str) -> Vec<WebhookEndpointDef> {
        let mut rows: Vec<WebhookEndpointDef> = self
            .endpoints
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
        self.endpoints
            .lock()
            .expect("webhooks")
            .remove(id)
            .is_some()
    }
    fn enqueue_outbox(&self, event: WebhookOutboxEvent) -> bool {
        let mut rows = self.outbox.lock().expect("webhook outbox");
        if rows.contains_key(&event.id) {
            return false;
        }
        rows.insert(event.id.clone(), event);
        true
    }
    fn pending_outbox(&self) -> Vec<WebhookOutboxEvent> {
        let mut rows: Vec<_> = self
            .outbox
            .lock()
            .expect("webhook outbox")
            .values()
            .cloned()
            .collect();
        rows.sort_by(|a, b| a.id.cmp(&b.id));
        rows
    }
    fn complete_outbox(&self, event_id: &str) -> bool {
        self.outbox
            .lock()
            .expect("webhook outbox")
            .remove(event_id)
            .is_some()
    }
}

/// A store for per-agent [`AgentResourceConfig`] bindings (ADR-0038) — which
/// resources an agent mounts. Sync + in-memory by default; the SQLite backend
/// implements the same port over a scoped-migration table.
pub trait ResourceStore: Send + Sync {
    fn put_agent_resource(&self, config: AgentResourceConfig);
    fn get_agent_resource(&self, agent_id: &str) -> Option<AgentResourceConfig>;

    /// Workspace-scoped variants used by every platform edge. Legacy adapters
    /// inherit the old behavior, while durable/platform stores override these
    /// with a composite key so equal agent ids cannot collide across tenants.
    fn put_agent_resource_in(&self, _workspace: &str, config: AgentResourceConfig) {
        self.put_agent_resource(config);
    }
    fn get_agent_resource_in(
        &self,
        _workspace: &str,
        agent_id: &str,
    ) -> Option<AgentResourceConfig> {
        self.get_agent_resource(agent_id)
    }
}

/// The default in-memory [`ResourceStore`], keyed by agent id.
#[derive(Default)]
pub struct InMemoryResourceStore(std::sync::Mutex<HashMap<(String, String), AgentResourceConfig>>);

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
            .insert((String::new(), config.agent_id.clone()), config);
    }
    fn get_agent_resource(&self, agent_id: &str) -> Option<AgentResourceConfig> {
        self.0
            .lock()
            .expect("agent resource configs")
            .get(&(String::new(), agent_id.to_string()))
            .cloned()
    }
    fn put_agent_resource_in(&self, workspace: &str, config: AgentResourceConfig) {
        self.0
            .lock()
            .expect("agent resource configs")
            .insert((workspace.to_string(), config.agent_id.clone()), config);
    }
    fn get_agent_resource_in(
        &self,
        workspace: &str,
        agent_id: &str,
    ) -> Option<AgentResourceConfig> {
        self.0
            .lock()
            .expect("agent resource configs")
            .get(&(workspace.to_string(), agent_id.to_string()))
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
