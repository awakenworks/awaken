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
        let native = self.kind == "native";
        let awaken_tool_bridge = if native {
            "unavailable"
        } else {
            match self.local.as_ref() {
                Some(local) if !local.detected => "unavailable",
                Some(local) => match local.negotiated.as_ref() {
                    Some(negotiated)
                        if negotiated
                            .get("mcp_http")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                            || negotiated
                                .get("mcp_sse")
                                .and_then(Value::as_bool)
                                .unwrap_or(false) =>
                    {
                        "supported"
                    }
                    Some(_) => "unavailable",
                    None => "conditional",
                },
                None => "conditional",
            }
        };
        let provider_server_tools = if native {
            "supported"
        } else if self.cli.as_deref() == Some("codex") {
            "conditional"
        } else {
            "unavailable"
        };
        json!({
            "id": self.id,
            "label": self.label,
            "kind": self.kind,
            "cli": self.cli,
            "description": self.description,
            "supported": true,
            "features": {
                "environment_session": "supported",
                "context_projection": "supported",
                "awaken_tool_bridge": awaken_tool_bridge,
                "state_machine": if native { "supported" } else { "unavailable" },
                "background_tools": if native { "supported" } else { "unavailable" },
                "working_directory": if native { "unavailable" } else { "supported" },
                "provider_server_tools": provider_server_tools,
            },
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
    runtimes: Arc<dyn RuntimeCapabilitySource>,
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
    runtimes: Vec<RuntimeCapability>,
) -> Router {
    capabilities_router_with_source(
        tools,
        plugins,
        Arc::new(StaticRuntimeCapabilities(runtimes)),
    )
}

pub fn capabilities_router_with_source(
    tools: Vec<ToolDescriptor>,
    plugins: Vec<PluginCapability>,
    runtimes: Arc<dyn RuntimeCapabilitySource>,
) -> Router {
    Router::new()
        .route("/v1/capabilities", get(get_capabilities))
        .with_state(Arc::new(CapabilityState {
            tools,
            plugins,
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
        "toolsets": managed_toolset_catalog(&state.tools),
        "plugins": plugin_catalog(&state.plugins),
        "runtimes": runtime_caps,
        "resource_inputs": resource_input_capability(),
        "sandbox_execution_policy": sandbox_execution_policy_capability(),
        "dreams": dream_capability(),
    }))
}

/// Read-only authoring projection of the Provisioning layout authority. This
/// endpoint owns no mutable fallback catalog.
fn resource_input_capability() -> Value {
    json!({
        "default_mounts": awaken_provisioning_contract::resource_input_default_mounts(),
    })
}

/// Managed Agents tool-family authoring metadata. Closed membership comes from
/// the wire contract while descriptions and input schemas come from the live
/// runtime catalog, so authoring clients do not duplicate either authority.
fn managed_toolset_catalog(tools: &[ToolDescriptor]) -> Vec<Value> {
    let agent_default_config = json!({
        "enabled": true,
        "permission_policy": { "type": "always_allow" }
    });
    let members = awaken_session_contract::AGENT_TOOLSET_TOOL_IDS
        .iter()
        .map(|name| {
            let descriptor = tools.iter().find(|tool| tool.id == *name);
            let configurable_fields = match *name {
                "web_fetch" => json!([
                    "enabled",
                    "permission_policy",
                    "allowed_domains",
                    "blocked_domains",
                    "max_content_tokens"
                ]),
                "web_search" => json!([
                    "enabled",
                    "permission_policy",
                    "allowed_domains",
                    "blocked_domains",
                    "user_location"
                ]),
                _ => json!(["enabled", "permission_policy"]),
            };
            json!({
                "name": name,
                "description": descriptor.map(|tool| tool.description.as_str()),
                "input_schema": descriptor.map(|tool| &tool.parameters),
                "available": descriptor.is_some(),
                "controlled_modification": awaken_session_contract::is_controlled_modification_member(name),
                "configurable_fields": configurable_fields,
            })
        })
        .collect::<Vec<_>>();
    vec![
        json!({
            "type": "agent_toolset_20260401",
            "source_kind": "agent",
            "dynamic_members": false,
            "default_config": agent_default_config,
            "members": members,
        }),
        json!({
            "type": "mcp_toolset",
            "source_kind": "mcp",
            "dynamic_members": true,
            "default_config": {
                "enabled": true,
                "permission_policy": { "type": "always_ask" }
            },
            "member_configurable_fields": ["enabled", "permission_policy"],
        }),
    ]
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
    fn managed_toolset_catalog_uses_contract_members_and_source_specific_defaults() {
        // Capability cause/effect table: C1 Agent Toolset default is allow; C2 a
        // member is/is not controlled by the separate transient preset; C3 its
        // descriptor is present/absent. Effects: E1 the one Toolset default owns
        // ordinary picker permission; E2 C2 is only classification metadata; E3
        // C3 projects exact availability/schema. R1 Bash=C1+controlled+present
        // => allow default+flagged+available; R2 read=C1+ordinary+absent => allow
        // default+unflagged+unavailable. No member-level default is emitted. The
        // server preset transform is tested by
        // its Session-contract owner and is not rebuilt by this capability view.
        let tools = vec![ToolDescriptor::pinned(
            "builtin:test",
            "bash",
            "Run a shell command.",
            json!({ "type": "object", "properties": {} }),
        )];
        let catalog = managed_toolset_catalog(&tools);
        assert_eq!(catalog.len(), 2);
        assert_eq!(catalog[0]["type"], "agent_toolset_20260401");
        assert_eq!(
            catalog[0]["default_config"]["permission_policy"]["type"],
            "always_allow"
        );
        assert_eq!(catalog[1]["type"], "mcp_toolset");
        assert_eq!(catalog[1]["dynamic_members"], true);
        assert_eq!(
            catalog[1]["default_config"]["permission_policy"]["type"],
            "always_ask"
        );
        let members = catalog[0]["members"].as_array().unwrap();
        assert_eq!(
            members.len(),
            awaken_session_contract::AGENT_TOOLSET_TOOL_IDS.len()
        );
        let bash = members
            .iter()
            .find(|member| member["name"] == "bash")
            .unwrap();
        assert_eq!(bash["available"], true);
        assert_eq!(bash["description"], "Run a shell command.");
        assert_eq!(bash["controlled_modification"], true);
        assert!(bash.get("default_permission_policy").is_none());
        let read = members
            .iter()
            .find(|member| member["name"] == "read")
            .unwrap();
        assert_eq!(read["controlled_modification"], false);
        assert!(read.get("default_permission_policy").is_none());
        let web_search = members
            .iter()
            .find(|member| member["name"] == "web_search")
            .unwrap();
        assert_eq!(web_search["available"], false);
        assert!(
            web_search["configurable_fields"]
                .as_array()
                .unwrap()
                .iter()
                .any(|field| field == "user_location")
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

    #[tokio::test]
    async fn resource_input_capability_projects_the_provisioning_defaults_exactly() {
        // Read-model cause/effect table: C1 the provisioning authority exposes
        // all three closed input kinds -> E1 `/v1/capabilities` projects those
        // exact values under `resource_inputs.default_mounts`; C2 no mutable
        // config/store input exists -> E2 the projection cannot author a second
        // default catalog. Rule R1=C1+C2 -> E1+E2.
        let Json(projected) = get_capabilities(State(Arc::new(CapabilityState {
            tools: Vec::new(),
            plugins: Vec::new(),
            runtimes: Arc::new(StaticRuntimeCapabilities(Vec::new())),
        })))
        .await;
        let authoritative = awaken_provisioning_contract::resource_input_default_mounts();
        assert_eq!(
            projected["resource_inputs"]["default_mounts"],
            serde_json::to_value(authoritative).unwrap(),
            "R1"
        );
        assert_eq!(
            projected["resource_inputs"]["default_mounts"]["repository"],
            "/workspace/repo"
        );
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
        assert_eq!(json["features"]["awaken_tool_bridge"], "conditional", "C2");
        assert_eq!(json["features"]["state_machine"], "unavailable", "C2");
        assert_eq!(
            native.to_json()["features"]["state_machine"],
            "supported",
            "C1"
        );
        assert_eq!(
            native.to_json()["features"]["provider_server_tools"],
            "supported",
            "C1 Native owns the provider request and can project exact hosted tools"
        );
        assert_eq!(json["local"]["detected"], true, "C2");
        assert_eq!(json["local"]["login_state"], "available", "C2");

        let supported = RuntimeCapability::acp("codex", "Codex", "description").with_local(
            LocalRuntimeCapability {
                detected: true,
                version: Some("1".into()),
                login_state: Some("available".into()),
                reason_code: None,
                remediation: None,
                negotiated: Some(json!({"mcp_http": true, "mcp_sse": false})),
            },
        );
        assert_eq!(
            supported.to_json()["features"]["awaken_tool_bridge"],
            "supported",
            "C3 negotiated MCP transport makes the bridge executable"
        );
        let unsupported = RuntimeCapability::acp("codex", "Codex", "description").with_local(
            LocalRuntimeCapability {
                detected: true,
                version: Some("1".into()),
                login_state: Some("available".into()),
                reason_code: None,
                remediation: None,
                negotiated: Some(json!({"mcp_http": false, "mcp_sse": false})),
            },
        );
        assert_eq!(
            unsupported.to_json()["features"]["awaken_tool_bridge"],
            "unavailable",
            "C4 a probed Harness without MCP must not advertise Awaken tools"
        );
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
