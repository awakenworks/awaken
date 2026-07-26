//! The capability snapshot (`GET /v1/capabilities`): the host-level facts the
//! management console needs to author agents data-driven — the advertised tool
//! descriptors and the installable plugins with their config JSON-Schema. These
//! are deployment-level (the same for every workspace), so the handler is
//! scope-free; it is merged into the flat management router and thus reachable
//! both flat and via `/v1/workspaces/{ws}/capabilities` (the addressing is
//! uniform even though the data is scope-invariant).
//!
//! Everything *scoped* the editor needs — models (`/v1/config/catalog`), skills
//! (`/v1/skills`) and Agent definitions (`/v1/config/agents`, including their
//! typed MCP bindings and delegate rosters) — already has its own endpoint; this
//! router deliberately does not duplicate them.

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
        "policies": policy_catalog(),
        "runtimes": runtime_catalog(),
        "sandbox_execution_policy": sandbox_execution_policy_capability(),
    }))
}

/// The execution backends an environment can bind — the "where/how it runs" axis,
/// kept off the (protocol-neutral) agent. `awaken` is the native in-process runtime;
/// `acp:<cli>` routes the run to an ACP CLI adapter. The console renders this as the
/// environment's Runtime picker and, on session-start, writes the chosen id to the
/// `awaken.runtime` session-metadata key (see protocol-managed `ext::model_selection`).
///
/// Hand-curated deployment vocabulary (like the permission schema below), pinned to
/// the backend source of truth `awaken_run_executor_acp::known_acp_clis()` by
/// `runtime_catalog_matches_known_clis` — keep the two in sync.
pub fn runtime_catalog() -> Vec<Value> {
    vec![
        runtime(
            "awaken",
            "Native",
            "native",
            None,
            "Runs in-process on the awaken runtime — no external CLI, no sandbox.",
        ),
        runtime(
            "acp:claude",
            "Claude Code",
            "acp",
            Some("claude"),
            "Claude Code via the ACP adapter (npx @agentclientprotocol/claude-agent-acp). Reads CLAUDE.md.",
        ),
        runtime(
            "acp:kimi",
            "Kimi Code",
            "acp",
            Some("kimi"),
            "Kimi Code CLI via its native ACP server. Reads AGENTS.md.",
        ),
        runtime(
            "acp:codex",
            "Codex",
            "acp",
            Some("codex"),
            "OpenAI Codex via the Zed codex-acp adapter.",
        ),
        runtime(
            "acp:gemini",
            "Gemini CLI",
            "acp",
            Some("gemini"),
            "Gemini CLI via ACP. Reads GEMINI.md.",
        ),
        runtime(
            "acp:opencode",
            "OpenCode",
            "acp",
            Some("opencode"),
            "OpenCode via ACP.",
        ),
        runtime(
            "acp:hermes",
            "Hermes Agent",
            "acp",
            Some("hermes"),
            "Hermes Agent via its native ACP server; private MEMORY.md state stays isolated.",
        ),
    ]
}

fn runtime(id: &str, label: &str, kind: &str, cli: Option<&str>, description: &str) -> Value {
    json!({ "id": id, "label": label, "kind": kind, "cli": cli, "description": description })
}

/// Authoring contract for the independent, versioned SandboxExecutionPolicy.
/// Environment networking and Resource mounts deliberately do not appear here.
pub fn sandbox_execution_policy_capability() -> Value {
    json!({
        "config_schema": sandbox_config_schema(),
        "presets": sandbox_presets(),
        "collection_path": "/v1/awaken/sandbox-execution-policies",
    })
}

/// The always-on policies whose `plugin_config` section shapes a run without being
/// an installable plugin. The permission gate is the one: an agent's `permission`
/// section (default behavior + ordered rules) drives the thread's authorization
/// gate (see runtime-host `config::config_permission_ruleset`). Kept separate from
/// `plugins` so the console renders a dedicated policy editor, not an enable toggle.
fn policy_catalog() -> Vec<Value> {
    vec![policy_cap(
        "permission",
        awaken_ext_permission::permission_config_schema(),
    )]
}

fn policy_cap(id: &str, config_schema: Value) -> Value {
    json!({ "id": id, "config_section": id, "config_schema": config_schema })
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

/// Grammar the field schema cannot convey (isolation ranking and enforceability).
const SANDBOX_AUTHORING_GUIDE: &str = "\
How a run is isolated (a versioned SandboxExecutionPolicy). Authoring rules:\n\
- `isolation`: `workdir` (cwd only, no OS isolation — trusted/dev), `namespace` \
(OS-namespace isolation via bubblewrap/sandbox-exec — the default for an ACP CLI), or \
`container` (full container/VM). A provider must meet or exceed what you ask for.\n\
- `limits`: best-effort caps `cpu_millis` (1000 = 1 core) and `memory_bytes`; omit for \
provider defaults. A backend that can't enforce a set limit fails closed, never ignores it.\n\
Networking belongs to Environment; mounts belong to typed Resources.";

/// The exact config persisted in a SandboxExecutionPolicy version.
fn sandbox_config_schema() -> Value {
    json!({
        "type": "object",
        "title": "Sandbox",
        "description": SANDBOX_AUTHORING_GUIDE,
        "additionalProperties": false,
        "properties": {
            "isolation": {
                "type": "string", "enum": ["workdir", "namespace", "container"],
                "default": "namespace",
                "description": "Isolation class; the provider must meet or exceed it."
            },
            "limits": {
                "type": "object",
                "description": "Best-effort resource caps; omit for provider defaults.",
                "properties": {
                    "cpu_millis": { "type": ["integer", "null"], "minimum": 1, "description": "CPU cap in millicores (1000 = 1 core)." },
                    "memory_bytes": { "type": ["integer", "null"], "minimum": 1, "description": "Memory cap in bytes." }
                }
            }
        }
    })
}

/// Named starting points. Each `spec` validates against
/// [`sandbox_config_schema`]; the console shows them as one-click presets over the form.
fn sandbox_presets() -> Vec<Value> {
    vec![
        json!({
            "id": "standard", "label": "Standard",
            "description": "Namespace isolation with provider-default resource limits.",
            "spec": { "isolation": "namespace" }
        }),
        json!({
            "id": "locked-down", "label": "Locked-down",
            "description": "Container isolation with tight compute limits.",
            "spec": {
                "isolation": "container",
                "limits": { "cpu_millis": 2000, "memory_bytes": 2147483648u64 }
            }
        }),
    ]
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

    #[test]
    fn policy_catalog_exposes_the_permission_schema() {
        let policies = policy_catalog();
        let perm = policies
            .iter()
            .find(|p| p["id"] == "permission")
            .expect("permission policy is advertised");
        assert!(
            perm["config_schema"].is_object(),
            "carries an object schema"
        );
        assert_eq!(perm["config_section"], "permission");
    }

    #[test]
    fn runtime_catalog_matches_known_clis() {
        // Pinned to `awaken_run_executor_acp::known_acp_clis()` (native + these four).
        // If that catalog changes, update this list — the boundary keeps the executor
        // crate out of the config plane, so this is the deliberate sync point.
        let catalog = runtime_catalog();
        let ids: Vec<&str> = catalog.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(
            ids,
            [
                "awaken",
                "acp:claude",
                "acp:kimi",
                "acp:codex",
                "acp:gemini",
                "acp:opencode",
                "acp:hermes"
            ]
        );
        // Native has no cli; every acp:* names its cli so the host can look it up.
        for r in runtime_catalog() {
            if r["kind"] == "acp" {
                assert!(r["cli"].is_string(), "acp runtime names its cli: {r}");
            }
        }
    }

    #[test]
    fn sandbox_execution_policy_capability_has_no_resource_or_network_overlap() {
        let sb = sandbox_execution_policy_capability();
        let schema = &sb["config_schema"];
        assert!(schema.is_object());
        // The grammar and ownership split ride on the schema description.
        let desc = schema["description"].as_str().unwrap();
        assert!(desc.contains("namespace") && desc.contains("Networking belongs"));
        assert!(schema["properties"].get("network").is_none());
        assert!(schema["properties"].get("mounts").is_none());
        let presets = sb["presets"].as_array().unwrap();
        let preset_ids: Vec<&str> = presets.iter().map(|p| p["id"].as_str().unwrap()).collect();
        assert_eq!(preset_ids, ["standard", "locked-down"]);
        let locked = presets.iter().find(|p| p["id"] == "locked-down").unwrap();
        assert_eq!(locked["spec"]["isolation"], "container");
    }
}
