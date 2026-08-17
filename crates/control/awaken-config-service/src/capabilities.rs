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

use async_trait::async_trait;
use awaken_runtime_contract::capability::PluginCapability;
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
    /// Protocol-neutral negotiated descriptor projection. Its authoritative
    /// structure is owned by the Worker ACP contract.
    pub negotiated: Option<Value>,
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
                "negotiated": local.negotiated,
            })),
        })
    }
}

struct CapabilityState {
    tools: Vec<ToolDescriptor>,
    plugins: Vec<PluginCapability>,
    policies: Vec<PolicyCapability>,
    runtimes: Arc<dyn RuntimeCapabilitySource>,
}

/// One extension-owned policy schema projected by the config API.
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyCapability {
    pub id: String,
    pub config_schema: Value,
}

impl PolicyCapability {
    #[must_use]
    pub fn new(id: impl Into<String>, config_schema: Value) -> Self {
        Self {
            id: id.into(),
            config_schema,
        }
    }
}

/// Read port for current runtime observations. Discovery remains owned by the
/// composition adapter; this config-plane projection queries it per request.
#[async_trait]
pub trait RuntimeCapabilitySource: Send + Sync {
    async fn current(&self) -> Vec<RuntimeCapability>;
}

struct StaticRuntimeCapabilities(Vec<RuntimeCapability>);

#[async_trait]
impl RuntimeCapabilitySource for StaticRuntimeCapabilities {
    async fn current(&self) -> Vec<RuntimeCapability> {
        self.0.clone()
    }
}

#[must_use]
pub fn static_runtime_capabilities(
    runtimes: Vec<RuntimeCapability>,
) -> Arc<dyn RuntimeCapabilitySource> {
    Arc::new(StaticRuntimeCapabilities(runtimes))
}

/// Mount `GET /v1/capabilities` over the host's advertised tool descriptors and
/// the runtime catalog projection supplied by the composition root.
pub fn capabilities_router(
    tools: Vec<ToolDescriptor>,
    plugins: Vec<PluginCapability>,
    policies: Vec<PolicyCapability>,
    runtimes: Vec<RuntimeCapability>,
) -> Router {
    capabilities_router_with_source(
        tools,
        plugins,
        policies,
        Arc::new(StaticRuntimeCapabilities(runtimes)),
    )
}

pub fn capabilities_router_with_source(
    tools: Vec<ToolDescriptor>,
    plugins: Vec<PluginCapability>,
    policies: Vec<PolicyCapability>,
    runtimes: Arc<dyn RuntimeCapabilitySource>,
) -> Router {
    Router::new()
        .route("/v1/capabilities", get(get_capabilities))
        .with_state(Arc::new(CapabilityState {
            tools,
            plugins,
            policies,
            runtimes,
        }))
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
        .current()
        .await
        .iter()
        .map(RuntimeCapability::to_json)
        .collect();
    Json(json!({
        "runtime_version": env!("CARGO_PKG_VERSION"),
        "tools": tool_caps,
        "plugins": plugin_catalog(&state.plugins),
        "policies": policy_catalog(&state.policies),
        "runtimes": runtime_caps,
        "sandbox_execution_policy": sandbox_execution_policy_capability(),
        "dreams": dream_capability(),
    }))
}

/// Stable, deployment-independent Dream authoring limits. Runtime model
/// readiness remains workspace-scoped and is intersected with `/v1/config/catalog`.
pub fn dream_capability() -> Value {
    json!({
        "enabled": true,
        "research_preview": true,
        "model_reference_format": "managed",
        "supports_connected_models": true,
        "supported_executors": ["native", "acp"],
        "model_reference_example": "qwen/qwen3-235b;provider=anyrouter;api=open_ai_responses;endpoint=primary;executor=acp:codex",
        "supported_models": awaken_session_contract::DREAM_SUPPORTED_MODELS,
        "supported_speeds": ["standard"],
        "max_sessions": awaken_session_contract::DREAM_MAX_SESSIONS,
        "max_instructions_chars": awaken_session_contract::DREAM_MAX_INSTRUCTIONS_CHARS,
        "policy_available": true,
        "collection_path": "/v1/dreams",
        "policy_path_template": "/v1/awaken/memory-stores/{memory_store_id}/dream-policy",
    })
}

/// Authoring contract for the independent, versioned SandboxExecutionPolicy.
/// Environment networking and Resource mounts deliberately do not appear here.
pub fn sandbox_execution_policy_capability() -> Value {
    json!({
        "config_schema": sandbox_config_schema(),
        "provisioning_schema": {
            "type": "string",
            "enum": ["eager", "on_tool_use"],
            "default": "eager",
            "description": "Create during Session preparation, or block the first sandbox tool until Native Awaken creates it. on_tool_use is rejected for ACP runtimes."
        },
        "presets": sandbox_presets(),
        "collection_path": "/v1/awaken/sandbox-execution-policies",
        "version_path_template": "/v1/awaken/sandbox-execution-policies/{policy_id}/versions/{version}",
    })
}

/// The always-on policies whose `plugin_config` section shapes a run without being
/// an installable plugin. The permission gate is the one: an agent's `permission`
/// section (default behavior + ordered rules) drives the thread's authorization
/// gate (see runtime-host `config::config_permission_ruleset`). Kept separate from
/// `plugins` so the console renders a dedicated policy editor, not an enable toggle.
fn policy_catalog(policies: &[PolicyCapability]) -> Vec<Value> {
    policies
        .iter()
        .map(|policy| policy_cap(&policy.id, policy.config_schema.clone()))
        .collect()
}

fn policy_cap(id: &str, config_schema: Value) -> Value {
    json!({ "id": id, "config_section": id, "config_schema": config_schema })
}

/// Public projection of the exact plugin capability rows supplied by the runtime
/// composition root. The config plane owns no plugin inventory of its own.
fn plugin_catalog(plugins: &[PluginCapability]) -> Vec<Value> {
    plugins
        .iter()
        .map(|plugin| {
            json!({
                "id": plugin.id,
                "config_sections": plugin.schema_keys,
                "config_schema": plugin.config_schema,
                "bound": plugin.bound,
            })
        })
        .collect()
}

/// Grammar the field schema cannot convey (isolation ranking and enforceability).
const SANDBOX_AUTHORING_GUIDE: &str = "\
How a run is isolated (a versioned SandboxExecutionPolicy). Authoring rules:\n\
- `isolation`: `workdir` (cwd only, no OS isolation — trusted/dev), `namespace` \
(OS-namespace isolation via bubblewrap/sandbox-exec — the default for an ACP CLI), or \
`container` (full container/VM). A provider must meet or exceed what you ask for.\n\
- `requests`: scheduler reservations for `cpu_millis` (1000 = 1 core), `memory_bytes`, and \
`disk_bytes`; they are placement demand, not runtime caps or billing units.\n\
- `limits`: enforceable caps for `cpu_millis`, `memory_bytes`, `disk_bytes`, and `pids`; omit \
for provider defaults. A backend that can't enforce a set limit fails closed, never ignores it.\n\
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
                "description": "Enforceable resource caps; omit for provider defaults.",
                "additionalProperties": false,
                "properties": {
                    "cpu_millis": { "type": ["integer", "null"], "minimum": 1, "description": "CPU cap in millicores (1000 = 1 core)." },
                    "memory_bytes": { "type": ["integer", "null"], "minimum": 1, "description": "Memory cap in bytes." },
                    "disk_bytes": { "type": ["integer", "null"], "minimum": 1, "description": "Ephemeral-disk cap in bytes." },
                    "pids": { "type": ["integer", "null"], "minimum": 1, "description": "Process-count cap." }
                }
            },
            "requests": {
                "type": "object",
                "description": "Scheduler reservations; independent of limits and billing.",
                "additionalProperties": false,
                "properties": {
                    "cpu_millis": { "type": ["integer", "null"], "minimum": 1, "description": "Reserved CPU in millicores (1000 = 1 core)." },
                    "memory_bytes": { "type": ["integer", "null"], "minimum": 1, "description": "Reserved memory in bytes." },
                    "disk_bytes": { "type": ["integer", "null"], "minimum": 1, "description": "Reserved ephemeral disk in bytes." }
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ChangingRuntimeSource(AtomicUsize);

    #[async_trait]
    impl RuntimeCapabilitySource for ChangingRuntimeSource {
        async fn current(&self) -> Vec<RuntimeCapability> {
            let state = self.0.fetch_add(1, Ordering::SeqCst);
            vec![RuntimeCapability::acp("codex", "Codex", "test").with_local(
                LocalRuntimeCapability {
                    detected: true,
                    version: Some("1".into()),
                    login_state: Some(if state == 0 {
                        "login_required".into()
                    } else {
                        "available".into()
                    }),
                    reason_code: None,
                    remediation: None,
                    negotiated: None,
                },
            )]
        }
    }

    #[tokio::test]
    async fn runtime_source_is_queried_for_each_capability_projection() {
        // Cause/effect decision table:
        // R1 first observation login_required -> first projection unavailable;
        // R2 later observation available      -> next projection available.
        // No startup snapshot is retained by the config service.
        let source = ChangingRuntimeSource(AtomicUsize::new(0));
        assert_eq!(
            source.current().await[0]
                .local
                .as_ref()
                .unwrap()
                .login_state
                .as_deref(),
            Some("login_required"),
            "R1"
        );
        assert_eq!(
            source.current().await[0]
                .local
                .as_ref()
                .unwrap()
                .login_state
                .as_deref(),
            Some("available"),
            "R2"
        );
    }

    #[test]
    fn plugin_catalog_projects_only_the_injected_runtime_facts() {
        // Cause/effect decision table:
        // R1 injected capability -> exact id/keys/schema/bound public projection;
        // R2 absent capability   -> no Config Service fallback row is fabricated.
        let source = vec![PluginCapability {
            id: "external.example".into(),
            schema_keys: vec!["external_config".into()],
            config_schema: Some(json!({"type": "object"})),
            bound: Default::default(),
        }];
        let plugins = plugin_catalog(&source);
        assert_eq!(plugins.len(), 1, "R1/R2");
        assert_eq!(plugins[0]["id"], "external.example", "R1");
        assert_eq!(
            plugins[0]["config_sections"],
            json!(["external_config"]),
            "R1"
        );
        assert!(plugins[0]["config_schema"].is_object(), "R1");
        assert!(plugins[0]["bound"].is_object(), "R1");
        assert!(plugins.iter().all(|p| p["id"] != "state_machine"), "R2");
    }

    #[test]
    fn policy_catalog_projects_only_injected_policy_schemas() {
        // Causes: C1 one injected policy; C2 no injected policies. Effects:
        // E1 exact id/schema projection; E2 no application-owned fallback row.
        // R1 C1 -> E1; R2 C2 -> E2.
        let policies = policy_catalog(&[PolicyCapability::new(
            "permission",
            json!({"type": "object"}),
        )]);
        let perm = policies
            .iter()
            .find(|p| p["id"] == "permission")
            .expect("permission policy is advertised");
        assert!(
            perm["config_schema"].is_object(),
            "carries an object schema"
        );
        assert_eq!(perm["config_section"], "permission");
        assert!(policy_catalog(&[]).is_empty(), "R2");
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
            negotiated: None,
        });
        let json = observed.to_json();
        assert_eq!(json["supported"], true, "C2");
        assert_eq!(json["local"]["detected"], true, "C2");
        assert_eq!(json["local"]["login_state"], "available", "C2");
    }

    #[test]
    fn sandbox_execution_policy_capability_has_no_resource_or_network_overlap() {
        // Cause/effect decision table: R1 the exact SandboxOverride scheduling
        // axes are authorable -> requests(cpu,memory,disk) and
        // limits(cpu,memory,disk,pids) appear with positive-or-null grammar;
        // R2 Environment/Resource-owned network and mounts stay absent; R3
        // nested unknown fields fail schema admission; R4 requests remain
        // explicitly distinct from caps and billing in the authoring guide.
        let sb = sandbox_execution_policy_capability();
        let schema = &sb["config_schema"];
        assert!(schema.is_object());
        // The grammar and ownership split ride on the schema description.
        let desc = schema["description"].as_str().unwrap();
        assert!(desc.contains("namespace") && desc.contains("Networking belongs"));
        assert!(schema["properties"].get("network").is_none());
        assert!(schema["properties"].get("mounts").is_none());
        let requests = &schema["properties"]["requests"];
        assert_eq!(requests["additionalProperties"], false, "R3");
        assert_eq!(
            requests["properties"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            ["cpu_millis", "disk_bytes", "memory_bytes"]
                .into_iter()
                .collect(),
            "R1"
        );
        let limits = &schema["properties"]["limits"];
        assert_eq!(limits["additionalProperties"], false, "R3");
        assert_eq!(
            limits["properties"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            ["cpu_millis", "disk_bytes", "memory_bytes", "pids"]
                .into_iter()
                .collect(),
            "R1"
        );
        for fields in [requests, limits] {
            for field in fields["properties"].as_object().unwrap().values() {
                assert_eq!(field["minimum"], 1, "R1");
                assert_eq!(field["type"], json!(["integer", "null"]), "R1");
            }
        }
        assert!(desc.contains("not runtime caps or billing units"), "R4");
        assert_eq!(sb["provisioning_schema"]["default"], "eager");
        assert_eq!(
            sb["provisioning_schema"]["enum"],
            json!(["eager", "on_tool_use"])
        );
        assert_eq!(
            sb["version_path_template"],
            "/v1/awaken/sandbox-execution-policies/{policy_id}/versions/{version}"
        );
        let presets = sb["presets"].as_array().unwrap();
        let preset_ids: Vec<&str> = presets.iter().map(|p| p["id"].as_str().unwrap()).collect();
        assert_eq!(preset_ids, ["standard", "locked-down"]);
        let locked = presets.iter().find(|p| p["id"] == "locked-down").unwrap();
        assert_eq!(locked["spec"]["isolation"], "container");
    }

    #[test]
    fn dream_capability_projects_the_shared_contract_limits() {
        let dream = dream_capability();
        assert_eq!(dream["enabled"], true);
        assert_eq!(dream["research_preview"], true);
        assert_eq!(dream["max_sessions"], 100);
        assert_eq!(dream["max_instructions_chars"], 4096);
        assert_eq!(dream["supported_speeds"], json!(["standard"]));
        assert_eq!(dream["model_reference_format"], "managed");
        assert_eq!(dream["supports_connected_models"], true);
        assert_eq!(dream["supported_executors"], json!(["native", "acp"]));
        assert!(
            dream["supported_models"]
                .as_array()
                .unwrap()
                .iter()
                .any(|model| model == "claude-sonnet-5")
        );
    }
}
