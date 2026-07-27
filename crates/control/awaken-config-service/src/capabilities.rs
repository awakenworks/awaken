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

/// One execution backend projected by the composition root from its authoritative
/// runtime catalog. The config plane renders this value but never authors another
/// adapter list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCapability {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub cli: Option<String>,
    pub description: String,
    pub local: Option<LocalRuntimeCapability>,
}

/// One startup observation of a supported runtime on this host. This is a
/// read-model projection, not another persisted capability inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalRuntimeCapability {
    pub detected: bool,
    pub version: Option<String>,
    pub login_state: Option<String>,
    pub reason_code: Option<String>,
    pub remediation: Option<String>,
}

impl RuntimeCapability {
    #[must_use]
    pub fn native() -> Self {
        Self {
            id: "awaken".into(),
            label: "Native".into(),
            kind: "native".into(),
            cli: None,
            description: "Runs in-process on the awaken runtime — no external CLI, no sandbox."
                .into(),
            local: None,
        }
    }

    #[must_use]
    pub fn acp(
        cli: impl Into<String>,
        label: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        let cli = cli.into();
        Self {
            id: format!("acp:{cli}"),
            label: label.into(),
            kind: "acp".into(),
            cli: Some(cli),
            description: description.into(),
            local: None,
        }
    }

    #[must_use]
    pub fn with_local(mut self, local: LocalRuntimeCapability) -> Self {
        self.local = Some(local);
        self
    }

    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "label": self.label,
            "kind": self.kind,
            "cli": self.cli,
            "description": self.description,
            "supported": true,
            "local": self.local.as_ref().map(|local| json!({
                "detected": local.detected,
                "version": local.version,
                "login_state": local.login_state,
                "reason_code": local.reason_code,
                "remediation": local.remediation,
            })),
        })
    }
}

struct CapabilityState {
    tools: Vec<ToolDescriptor>,
    runtimes: Vec<RuntimeCapability>,
}

/// Mount `GET /v1/capabilities` over the host's advertised tool descriptors and
/// the runtime catalog projection supplied by the composition root.
pub fn capabilities_router(tools: Vec<ToolDescriptor>, runtimes: Vec<RuntimeCapability>) -> Router {
    Router::new()
        .route("/v1/capabilities", get(get_capabilities))
        .with_state(Arc::new(CapabilityState { tools, runtimes }))
}

async fn get_capabilities(State(state): State<Arc<CapabilityState>>) -> Json<Value> {
    let tool_caps: Vec<Value> = state
        .tools
        .iter()
        .map(|t| {
            json!({
                "id": t.id,
                "description": t.description,
                "parameters": t.parameters,
            })
        })
        .collect();
    let runtime_caps: Vec<Value> = state
        .runtimes
        .iter()
        .map(RuntimeCapability::to_json)
        .collect();
    Json(json!({
        "runtime_version": env!("CARGO_PKG_VERSION"),
        "tools": tool_caps,
        "plugins": plugin_catalog(),
        "policies": policy_catalog(),
        "runtimes": runtime_caps,
        "sandbox_execution_policy": sandbox_execution_policy_capability(),
    }))
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
    fn runtime_capability_constructor_preserves_the_exact_adapter_identity() {
        // Cause graph:
        // catalog row -> exact `acp:<id>` capability; absent row -> no capability
        // can be fabricated by this config-plane renderer.
        //
        // Decision table:
        // | Rule | input kind | CLI id | result id | CLI field |
        // | C1 | Native | - | awaken | absent |
        // | C2 | ACP | codex | acp:codex | codex |
        let native = RuntimeCapability::native();
        let acp = RuntimeCapability::acp("codex", "Codex", "description");
        assert_eq!(native.id, "awaken", "C1");
        assert_eq!(native.cli, None, "C1");
        assert_eq!(acp.id, "acp:codex", "C2");
        assert_eq!(acp.cli.as_deref(), Some("codex"), "C2");
        let observed = acp.with_local(LocalRuntimeCapability {
            detected: true,
            version: Some("1".into()),
            login_state: Some("available".into()),
            reason_code: Some("acp_login_available".into()),
            remediation: None,
        });
        let json = observed.to_json();
        assert_eq!(json["supported"], true, "C2");
        assert_eq!(json["local"]["detected"], true, "C2");
        assert_eq!(json["local"]["login_state"], "available", "C2");
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
