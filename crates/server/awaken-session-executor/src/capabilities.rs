//! The capability snapshot (`GET /v1/capabilities`): the host-level facts the
//! management console needs to author agents data-driven — the advertised tool
//! descriptors and the installable plugins with their config JSON-Schema. These
//! are deployment-level (the same for every workspace), so the handler is
//! scope-free; it is merged into the flat management router and thus reachable
//! both flat and via `/v1/workspaces/{ws}/capabilities` (the addressing is
//! uniform even though the data is scope-invariant).
//!
//! Everything *scoped* the editor needs — models (`/v1/config/catalog`), skills
//! (`/v1/skills`), delegate agents (`/v1/config/agents`), MCP servers
//! (`/v1/config/mcp-servers`) — already has its own endpoint; this router
//! deliberately does not duplicate them.

use std::sync::Arc;

use awaken_runtime_contract::resolved::ToolDescriptor;
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

/// Mount `GET /v1/capabilities` over the host's advertised tool descriptors.
pub fn capabilities_router(tools: Vec<ToolDescriptor>) -> Router {
    Router::new()
        .route("/v1/capabilities", get(get_capabilities))
        .with_state(Arc::new(tools))
}

async fn get_capabilities(State(tools): State<Arc<Vec<ToolDescriptor>>>) -> Json<Value> {
    let tool_caps: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({
                "id": t.id,
                "description": t.description,
                "parameters": t.parameters,
            })
        })
        .collect();
    Json(json!({
        "runtime_version": env!("CARGO_PKG_VERSION"),
        "tools": tool_caps,
        "plugins": plugin_catalog(),
    }))
}

/// The installable plugins whose per-plugin `plugin_config` section the editor can
/// render from `config_schema`. Only plugins that expose a config schema are
/// listed (the permission engine is a gate policy, not a config-section plugin).
fn plugin_catalog() -> Vec<Value> {
    vec![
        plugin_cap(
            awaken_ext_state_machine::STATE_MACHINE_PLUGIN_ID,
            awaken_ext_state_machine::config_schema(),
        ),
        plugin_cap(
            awaken_ext_compact::COMPACT_PLUGIN_ID,
            awaken_ext_compact::compact_config_schema(),
        ),
        plugin_cap(
            awaken_ext_memory::MEMORY_PLUGIN_ID,
            awaken_ext_memory::memory_config_schema(),
        ),
    ]
}

fn plugin_cap(id: &str, config_schema: Value) -> Value {
    json!({ "id": id, "config_sections": [id], "config_schema": config_schema })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_catalog_lists_schema_carrying_plugins() {
        let plugins = plugin_catalog();
        // The editor's plugin picker + schema-driven config forms consume this.
        assert!(plugins.iter().any(|p| p["id"] == "state_machine"));
        assert!(plugins.iter().any(|p| p["id"] == "compact"));
        assert!(plugins.iter().any(|p| p["id"] == "memory"));
        // Every listed plugin carries an object config_schema (never null), so the
        // editor renders a form rather than a raw JSON textarea.
        assert!(
            plugins
                .iter()
                .all(|p| p["config_schema"].is_object() && p["config_sections"].is_array()),
            "every plugin needs an object config_schema: {plugins:?}"
        );
    }
}
