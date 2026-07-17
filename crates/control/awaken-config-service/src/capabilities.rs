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
        "policies": policy_catalog(),
        "runtimes": runtime_catalog(),
        "sandbox": sandbox_capability(),
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
    ]
}

fn runtime(id: &str, label: &str, kind: &str, cli: Option<&str>, description: &str) -> Value {
    json!({ "id": id, "label": label, "kind": kind, "cli": cli, "description": description })
}

/// The sandbox the environment realizes for a run — a UI-facing projection of the
/// backend `SandboxSpec` (`awaken_provisioning_contract`). Carries a JSON Schema the
/// console renders as a form AND a set of named presets so the common cases are one
/// click; the raw schema stays the escape hatch. The assistant authors from the same
/// schema (the "feed the schema" lever), so natural-language sandbox authoring is free.
pub fn sandbox_capability() -> Value {
    json!({
        "config_schema": sandbox_config_schema(),
        "presets": sandbox_presets(),
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

/// Grammar the field schema can't convey (isolation ranking, egress modes, the
/// mount shape), so an author — human form or LLM — doesn't guess. Field names mirror
/// `awaken_provisioning_contract::{SandboxSpec, vocab}` so the console value maps 1:1
/// onto the backend spec at provisioning time.
const SANDBOX_AUTHORING_GUIDE: &str = "\
How a run is isolated (a projection of the backend SandboxSpec). Authoring rules:\n\
- `isolation`: `workdir` (cwd only, no OS isolation — trusted/dev), `namespace` \
(OS-namespace isolation via bubblewrap/sandbox-exec — the default for an ACP CLI), or \
`container` (full container/VM). A provider must meet or exceed what you ask for.\n\
- `network.mode`: `unrestricted` (full egress — still needed to reach the model), \
`allowlist` (deny-by-default; only `network.hosts` are reachable, via the egress \
gateway), or `none` (no egress). More restrictive is always safe to ask for.\n\
- `mounts[]`: each is `{mount_path, access}` where `access` is `read_only` or \
`read_write`; `mount_path` is sandbox-absolute (e.g. `/work`, `/repo`).\n\
- `limits`: best-effort caps `cpu_millis` (1000 = 1 core) and `memory_bytes`; omit for \
provider defaults. A backend that can't enforce a set limit fails closed, never ignores it.";

/// The `SandboxSpec` projection an environment persists under `config.sandbox`.
fn sandbox_config_schema() -> Value {
    json!({
        "type": "object",
        "title": "Sandbox",
        "description": SANDBOX_AUTHORING_GUIDE,
        "examples": [sandbox_preset_spec_network_isolated()],
        "properties": {
            "isolation": {
                "type": "string", "enum": ["workdir", "namespace", "container"],
                "default": "namespace",
                "description": "Isolation class; the provider must meet or exceed it."
            },
            "mounts": {
                "type": "array",
                "description": "Filesystem made visible inside the sandbox.",
                "items": {
                    "type": "object",
                    "properties": {
                        "mount_path": { "type": "string", "description": "Sandbox-absolute path, e.g. /work." },
                        "access": { "type": "string", "enum": ["read_only", "read_write"] }
                    },
                    "required": ["mount_path", "access"]
                }
            },
            "network": {
                "type": "object",
                "description": "Egress policy.",
                "properties": {
                    "mode": { "type": "string", "enum": ["unrestricted", "allowlist", "none"], "default": "unrestricted" },
                    "hosts": {
                        "type": "array", "items": { "type": "string" },
                        "description": "Reachable hosts when mode is allowlist (globs allowed)."
                    }
                },
                "required": ["mode"]
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

/// Named starting points, from most-open to locked-down. Each `spec` validates against
/// [`sandbox_config_schema`]; the console shows them as one-click presets over the form.
fn sandbox_presets() -> Vec<Value> {
    vec![
        json!({
            "id": "standard", "label": "Standard",
            "description": "bwrap isolation, full egress. A scratch /work the agent can write.",
            "spec": {
                "isolation": "namespace",
                "mounts": [{ "mount_path": "/work", "access": "read_write" }],
                "network": { "mode": "unrestricted" }
            }
        }),
        json!({
            "id": "network-isolated", "label": "Network-isolated",
            "description": "bwrap isolation, egress denied except an allowlist. For untrusted work that still needs a few APIs.",
            "spec": sandbox_preset_spec_network_isolated()
        }),
        json!({
            "id": "locked-down", "label": "Locked-down",
            "description": "bwrap isolation, no egress, read-only inputs, tight caps. Maximum containment.",
            "spec": {
                "isolation": "namespace",
                "mounts": [{ "mount_path": "/repo", "access": "read_only" }],
                "network": { "mode": "none" },
                "limits": { "cpu_millis": 2000, "memory_bytes": 2147483648u64 }
            }
        }),
    ]
}

fn sandbox_preset_spec_network_isolated() -> Value {
    json!({
        "isolation": "namespace",
        "mounts": [
            { "mount_path": "/work", "access": "read_write" },
            { "mount_path": "/repo", "access": "read_only" }
        ],
        "network": { "mode": "allowlist", "hosts": ["api.github.com"] },
        "limits": { "cpu_millis": 2000, "memory_bytes": 4294967296u64 }
    })
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
                "acp:codex",
                "acp:gemini",
                "acp:opencode"
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
    fn sandbox_capability_carries_schema_and_presets() {
        let sb = sandbox_capability();
        let schema = &sb["config_schema"];
        assert!(schema.is_object());
        // The grammar (isolation ranking, egress modes) rides on the schema description.
        let desc = schema["description"].as_str().unwrap();
        assert!(desc.contains("namespace") && desc.contains("allowlist"));
        // Three presets, most-open → locked-down, each with a spec.
        let presets = sb["presets"].as_array().unwrap();
        let preset_ids: Vec<&str> = presets.iter().map(|p| p["id"].as_str().unwrap()).collect();
        assert_eq!(preset_ids, ["standard", "network-isolated", "locked-down"]);
        // The locked-down preset denies egress — the guard that it's actually the strict one.
        let locked = presets.iter().find(|p| p["id"] == "locked-down").unwrap();
        assert_eq!(locked["spec"]["network"]["mode"], "none");
    }
}
