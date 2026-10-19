// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: This source implements the repository-controlled mcp product boundary and is classified
// @formal reason: against its product requirement; this link is not a formal-proof claim.
//! MCP as a runtime plugin.
//!
//! [`McpServer`] connects to a server, holds its live tool registry, and starts
//! a background task that re-lists tools and bumps the registry version whenever
//! the server fires `tools/list_changed`. [`McpPlugin`] projects that registry
//! into the runtime as dynamic tools: `resolve()` returns the current tool
//! snapshot and `live_version()` returns the registry version, so the kernel
//! re-resolves the tool face at a step boundary after a change (P5b).
//!
//! The plugin depends only on the runtime contract; the kernel is never
//! imported. Its `CapabilityBound` is a namespace (`mcp__{server}__`) since the
//! exact ids are not known at composition time.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use awaken_mcp_wire::McpToolDefinition;
use awaken_runtime_contract::plugin::{
    CapabilityBound, Contributions, DynamicTool, IdBound, Plugin, PluginManifest,
};
use tokio::sync::broadcast;

use crate::error::McpError;
use crate::id_mapping::tool_namespace;
use crate::sensitive::mark_sensitive;
use crate::stdio::StdioTransport;
use crate::tool::{McpRawTool, mcp_tool_descriptor_for_transport, validate_tools_task_negotiation};
use crate::transport::{ListChangedKind, McpToolTransport};

/// Host-declared sensitive fields, keyed by the tool's wire name: dotted
/// property paths marked `x-sensitive` in the tool's schema at registry load
/// (see [`crate::sensitive`]). Complements the server's own `writeOnly` /
/// `format: "password"` markers.
pub type SensitiveFields = HashMap<String, Vec<String>>;

/// Mark host-declared sensitive paths on each tool's input schema, so the
/// projected descriptors carry them (and redaction can key off them).
fn apply_sensitive(tools: &mut [McpToolDefinition], sensitive: &SensitiveFields) {
    for def in tools {
        if let Some(paths) = sensitive.get(&def.name) {
            mark_sensitive(&mut def.input_schema, paths);
        }
    }
}

/// The plugin id for a server.
fn plugin_id(server_name: &str) -> String {
    format!("mcp:{server_name}")
}

fn manifest(server_name: &str, namespace: &str) -> PluginManifest {
    PluginManifest {
        id: plugin_id(server_name),
        requires: Vec::new(),
        config_sections: vec![plugin_id(server_name)],
        bound: CapabilityBound {
            tools: IdBound::Namespace(namespace.to_owned()),
            ..Default::default()
        },
    }
}

/// A server's live tool registry: the current tool set and a version that
/// advances on every `tools/list_changed`.
struct McpRegistry {
    version: u64,
    tools: Vec<McpToolDefinition>,
}

/// One catalog observer shared only by explicit server/plugin owners. The
/// observer itself never owns this guard, so it cannot keep itself alive.
struct RegistryRefresh(tokio::task::AbortHandle);

impl Drop for RegistryRefresh {
    fn drop(&mut self) {
        // Stop the original local catalog read task. This requests task cleanup;
        // it is not a receipt for remote tool completion or process termination.
        self.0.abort();
    }
}

/// A connected MCP server with a self-refreshing tool registry.
pub struct McpServer {
    server_name: String,
    namespace: String,
    transport: Arc<dyn McpToolTransport>,
    registry: Arc<Mutex<McpRegistry>>,
    _refresh: Arc<RegistryRefresh>,
}

impl McpServer {
    /// Discover `transport`'s tools and start refreshing the registry whenever a
    /// tool `list_changed` arrives on `list_changed`.
    pub async fn start(
        server_name: impl Into<String>,
        transport: Arc<dyn McpToolTransport>,
        list_changed: broadcast::Receiver<ListChangedKind>,
    ) -> Result<Self, McpError> {
        Self::start_with_sensitive(server_name, transport, list_changed, SensitiveFields::new())
            .await
    }

    /// [`start`](Self::start), with host-declared sensitive fields marked on
    /// each tool's schema at every registry load (initial and on refresh).
    pub async fn start_with_sensitive(
        server_name: impl Into<String>,
        transport: Arc<dyn McpToolTransport>,
        mut list_changed: broadcast::Receiver<ListChangedKind>,
        sensitive: SensitiveFields,
    ) -> Result<Self, McpError> {
        let server_name = server_name.into();
        let namespace = tool_namespace(&server_name)?;
        let mut tools = transport.list_tools().await?;
        validate_tools_task_negotiation(&server_name, &tools, transport.as_ref())?;
        apply_sensitive(&mut tools, &sensitive);
        let registry = Arc::new(Mutex::new(McpRegistry { version: 1, tools }));

        let refresh_registry = Arc::clone(&registry);
        let refresh_transport = Arc::clone(&transport);
        let refresh_server_name = server_name.clone();
        let refresh_task = tokio::spawn(async move {
            while let Ok(kind) = list_changed.recv().await {
                if kind == ListChangedKind::Tools
                    && let Ok(mut tools) = refresh_transport.list_tools().await
                    && validate_tools_task_negotiation(
                        &refresh_server_name,
                        &tools,
                        refresh_transport.as_ref(),
                    )
                    .is_ok()
                {
                    apply_sensitive(&mut tools, &sensitive);
                    let mut registry = refresh_registry.lock().unwrap();
                    registry.tools = tools;
                    registry.version += 1;
                }
            }
        });

        Ok(Self {
            server_name,
            namespace,
            transport,
            registry,
            _refresh: Arc::new(RegistryRefresh(refresh_task.abort_handle())),
        })
    }

    /// Connect over stdio and wire the server's own `list_changed` stream.
    pub async fn connect_stdio(
        server_name: impl Into<String>,
        transport: StdioTransport,
    ) -> Result<Self, McpError> {
        let list_changed = transport.subscribe_list_changed();
        Self::start(server_name, Arc::new(transport), list_changed).await
    }

    /// Connect over streaming HTTP and wire its `list_changed` stream. The
    /// `transport` must have been built with
    /// [`HttpTransport::connect_streaming`](crate::http::HttpTransport::connect_streaming)
    /// so the background SSE listener is running.
    pub async fn connect_http(
        server_name: impl Into<String>,
        transport: crate::http::HttpTransport,
    ) -> Result<Self, McpError> {
        let list_changed = transport.subscribe_list_changed();
        Self::start(server_name, Arc::new(transport), list_changed).await
    }

    /// The runtime plugin projecting this server's tools.
    pub fn plugin(&self) -> McpPlugin {
        McpPlugin {
            server_name: self.server_name.clone(),
            namespace: self.namespace.clone(),
            transport: Arc::clone(&self.transport),
            registry: Arc::clone(&self.registry),
            _refresh: Arc::clone(&self._refresh),
        }
    }

    /// The current registry version (advances on `tools/list_changed`).
    pub fn version(&self) -> u64 {
        self.registry.lock().unwrap().version
    }

    /// The server's name.
    pub fn name(&self) -> &str {
        &self.server_name
    }

    /// The encoded tool namespace (`mcp__{encoded}__`) this server's tools are
    /// projected under. The encoding is injective, so distinct raw names always
    /// yield distinct namespaces; the manager still dedups on this as a
    /// structural backstop.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Force a namespace, so the manager's collision backstop can be exercised
    /// even though the injective encoding makes a real collision unreachable.
    #[cfg(test)]
    pub(crate) fn set_namespace_for_test(&mut self, namespace: String) {
        self.namespace = namespace;
    }

    /// Whether the underlying transport is still usable.
    pub fn is_alive(&self) -> bool {
        self.transport.is_alive()
    }
}

/// The plugin projecting an [`McpServer`]'s live tools into the runtime.
pub struct McpPlugin {
    server_name: String,
    namespace: String,
    transport: Arc<dyn McpToolTransport>,
    registry: Arc<Mutex<McpRegistry>>,
    _refresh: Arc<RegistryRefresh>,
}

impl McpPlugin {
    /// The same pure metadata as a connected server, with the canonical namespace
    /// derived without connecting, discovering tools or granting execution rights.
    pub fn manifest_for(server_name: &str) -> Result<PluginManifest, McpError> {
        let namespace = tool_namespace(server_name)?;
        Ok(manifest(server_name, &namespace))
    }
}

impl Plugin for McpPlugin {
    fn manifest(&self) -> PluginManifest {
        manifest(&self.server_name, &self.namespace)
    }

    fn resolve(&self) -> Contributions {
        let mut contributions = Contributions::new(plugin_id(&self.server_name));
        let registry = self.registry.lock().unwrap();
        for def in &registry.tools {
            // A tool whose name has no encodable identity is skipped rather
            // than failing the whole resolution.
            if let (Ok(descriptor), Ok(tool)) = (
                mcp_tool_descriptor_for_transport(&self.server_name, def, self.transport.as_ref()),
                McpRawTool::from_definition(&self.server_name, def, Arc::clone(&self.transport)),
            ) && let Ok(dynamic) = DynamicTool::try_new(descriptor, Arc::new(tool))
            {
                contributions.register_dynamic_tool(dynamic);
            }
        }
        contributions
    }

    fn live_version(&self) -> Option<u64> {
        Some(self.registry.lock().unwrap().version)
    }

    fn resolve_configured(
        &self,
        config: Option<&serde_json::Value>,
    ) -> Result<Contributions, awaken_runtime_contract::plugin::PluginConfigError> {
        crate::notification::McpNotificationConfig::parse(config).map_err(|e| {
            awaken_runtime_contract::plugin::PluginConfigError::new(plugin_id(&self.server_name), e)
        })?;
        Ok(self.resolve())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awaken_mcp_wire::McpTransportError;
    use awaken_mcp_wire::{CallToolResult, ToolContent};
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A transport whose tool list is swapped on the second `list_tools` call, so
    /// a refresh can be observed.
    struct SwappingTransport {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl McpToolTransport for SwappingTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let name = if n == 0 { "alpha" } else { "beta" };
            Ok(vec![
                serde_json::from_value(serde_json::json!({
                    "name": name,
                    "inputSchema": {
                        "type": "object",
                        "properties": { "token": { "type": "string" } },
                    },
                }))
                .unwrap(),
            ])
        }
        async fn call_tool(
            &self,
            _tool_name: &str,
            _arguments: Value,
        ) -> Result<CallToolResult, McpTransportError> {
            Ok(CallToolResult {
                content: vec![ToolContent::Text {
                    text: "ok".to_string(),
                    annotations: None,
                    meta: None,
                }],
                structured_content: None,
                is_error: Some(false),
            })
        }
    }

    #[test]
    fn pure_server_manifest_uses_the_canonical_coordinate_without_a_connection() {
        // M3 empty names fail through the existing coordinate owner; M4 ASCII
        // and Unicode keep exact plugin ids and injective canonical bounds.
        // No McpServer/transport exists in these metadata-only rows.
        assert!(McpPlugin::manifest_for("").is_err());
        for server in ["phone", "智客/phone"] {
            let projected = McpPlugin::manifest_for(server).unwrap();
            assert_eq!(projected.id, format!("mcp:{server}"));
            assert_eq!(projected.config_sections, vec![projected.id.clone()]);
            assert!(projected.requires.is_empty());
            assert_eq!(
                projected.bound,
                CapabilityBound {
                    tools: IdBound::Namespace(tool_namespace(server).unwrap()),
                    ..Default::default()
                }
            );
        }
    }

    #[tokio::test]
    async fn plugin_projects_the_registry_as_dynamic_tools() {
        let transport = Arc::new(SwappingTransport {
            calls: AtomicUsize::new(0),
        });
        let (_tx, rx) = broadcast::channel(4);
        let mut server = McpServer::start("srv", transport, rx)
            .await
            .expect("starts");
        let plugin = server.plugin();

        // Manifest declares the server's namespace bound.
        assert_eq!(
            plugin.manifest().bound.tools,
            IdBound::Namespace("mcp__srv__".to_string())
        );
        assert_eq!(plugin.live_version(), Some(1));

        let contributions = plugin.resolve();
        assert_eq!(contributions.dynamic_tools.len(), 1);
        assert_eq!(
            contributions.dynamic_tools[0].descriptor().id,
            "mcp__srv__alpha"
        );
        // M1 the pure entry equals a connected instance; M2 an existing
        // forced namespace stays instance-owned rather than recalculated.
        assert_eq!(McpPlugin::manifest_for("srv").unwrap(), plugin.manifest());
        server.set_namespace_for_test("forced-existing-namespace".into());
        assert_eq!(
            server.plugin().manifest().bound.tools,
            IdBound::Namespace("forced-existing-namespace".into())
        );
    }

    #[tokio::test]
    async fn list_changed_refreshes_the_registry_and_bumps_the_version() {
        let transport = Arc::new(SwappingTransport {
            calls: AtomicUsize::new(0),
        });
        let (tx, rx) = broadcast::channel(4);
        let server = McpServer::start("srv", transport, rx)
            .await
            .expect("starts");
        assert_eq!(server.version(), 1);
        assert_eq!(
            server.plugin().resolve().dynamic_tools[0].descriptor().id,
            "mcp__srv__alpha"
        );

        // Fire a tools/list_changed; the background task re-lists and bumps.
        tx.send(ListChangedKind::Tools).expect("send");
        // Wait for the refresh to land.
        for _ in 0..50 {
            if server.version() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(server.version(), 2, "version advanced on list_changed");
        assert_eq!(
            server.plugin().resolve().dynamic_tools[0].descriptor().id,
            "mcp__srv__beta",
            "the refreshed tool set is projected"
        );
    }

    struct RequiredTaskOnRefreshTransport {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl McpToolTransport for RequiredTaskOnRefreshTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            let definition = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                serde_json::json!({"name": "stable"})
            } else {
                serde_json::json!({
                    "name": "requires_tasks",
                    "execution": {"taskSupport": "required"}
                })
            };
            Ok(vec![serde_json::from_value(definition).unwrap()])
        }

        async fn call_tool(
            &self,
            _tool_name: &str,
            _arguments: Value,
        ) -> Result<CallToolResult, McpTransportError> {
            unreachable!("projection test never invokes tools")
        }
    }

    #[tokio::test]
    async fn incompatible_required_task_refresh_preserves_the_committed_registry() {
        // Cause/effect graph: C1=current registry is valid; C2=list_changed
        // advertises a tool requiring tasks; C3=the initialized transport did
        // not negotiate task tools/call. Effects E1=the incompatible snapshot
        // is not published; E2=version and prior descriptors remain together.
        // Decision rule R1=C1+C2+C3=>E1+E2. Publishing either new tools or a
        // new version alone would split the dynamic catalog authority.
        let transport = Arc::new(RequiredTaskOnRefreshTransport {
            calls: AtomicUsize::new(0),
        });
        let (tx, rx) = broadcast::channel(4);
        let server = McpServer::start("srv", transport, rx)
            .await
            .expect("initial registry valid");
        tx.send(ListChangedKind::Tools).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(server.version(), 1, "R1/E2");
        let contributions = server.plugin().resolve();
        assert_eq!(contributions.dynamic_tools.len(), 1, "R1/E1");
        assert_eq!(
            contributions.dynamic_tools[0].descriptor().id,
            "mcp__srv__stable",
            "R1/E2"
        );
    }

    /// A transport whose tool set mixes a valid tool with one whose name has no
    /// encodable identity at all, to prove per-tool fault isolation.
    struct MixedTransport;

    #[async_trait]
    impl McpToolTransport for MixedTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            Ok(vec![
                serde_json::from_value(serde_json::json!({ "name": "good" })).unwrap(),
                // An empty name has no identity to encode ⇒ to_tool_id errors.
                serde_json::from_value(serde_json::json!({ "name": "" })).unwrap(),
            ])
        }
        async fn call_tool(
            &self,
            _tool_name: &str,
            _arguments: Value,
        ) -> Result<CallToolResult, McpTransportError> {
            Ok(CallToolResult {
                content: vec![ToolContent::Text {
                    text: "ok".to_string(),
                    annotations: None,
                    meta: None,
                }],
                structured_content: None,
                is_error: Some(false),
            })
        }
    }

    #[tokio::test]
    async fn resolve_skips_an_unmappable_tool_without_dropping_the_rest() {
        // A tool whose name cannot form a valid namespaced id is skipped, but the
        // projection of the *other* tools survives — one malformed tool must never
        // blank the whole server's tool face.
        let (_tx, rx) = broadcast::channel(4);
        let server = McpServer::start("srv", Arc::new(MixedTransport), rx)
            .await
            .expect("starts");
        let contributions = server.plugin().resolve();
        assert_eq!(
            contributions.dynamic_tools.len(),
            1,
            "the unmappable tool is skipped, the valid one kept"
        );
        assert_eq!(
            contributions.dynamic_tools[0].descriptor().id,
            "mcp__srv__good"
        );
    }

    /// A transport advertising several tools, to pin the registry projection.
    struct MultiToolTransport;

    #[async_trait]
    impl McpToolTransport for MultiToolTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            Ok(["read", "write", "delete"]
                .into_iter()
                .map(|name| serde_json::from_value(serde_json::json!({ "name": name })).unwrap())
                .collect())
        }
        async fn call_tool(
            &self,
            _tool_name: &str,
            _arguments: Value,
        ) -> Result<CallToolResult, McpTransportError> {
            Ok(CallToolResult {
                content: vec![ToolContent::Text {
                    text: "ok".to_string(),
                    annotations: None,
                    meta: None,
                }],
                structured_content: None,
                is_error: Some(false),
            })
        }
    }

    #[tokio::test]
    async fn resolve_exposes_exactly_every_advertised_tool_with_no_gating() {
        // Documents the current projection contract: the registry projection is 1:1
        // with the server's advertised tool list — every advertised tool becomes a
        // dynamic tool, in order. There is deliberately NO tool allow-list at this
        // layer (any per-agent tool gating is a policy concern applied above the
        // projection, not here); this asserts that documented behavior, not a defect.
        let (_tx, rx) = broadcast::channel(4);
        let server = McpServer::start("srv", Arc::new(MultiToolTransport), rx)
            .await
            .expect("starts");
        let ids: Vec<_> = server
            .plugin()
            .resolve()
            .dynamic_tools
            .iter()
            .map(|tool| tool.descriptor().id.clone())
            .collect();
        assert_eq!(
            ids,
            vec![
                "mcp__srv__read".to_string(),
                "mcp__srv__write".to_string(),
                "mcp__srv__delete".to_string(),
            ],
            "exactly the advertised tools are exposed, in order",
        );
    }

    #[tokio::test]
    async fn host_declared_sensitive_fields_mark_the_descriptor_across_refreshes() {
        let transport = Arc::new(SwappingTransport {
            calls: AtomicUsize::new(0),
        });
        let (tx, rx) = broadcast::channel(4);
        // Both tool generations declare `token` sensitive by wire name.
        let sensitive: SensitiveFields = [
            ("alpha".to_string(), vec!["token".to_string()]),
            ("beta".to_string(), vec!["token".to_string()]),
        ]
        .into_iter()
        .collect();
        let server = McpServer::start_with_sensitive("srv", transport, rx, sensitive)
            .await
            .expect("starts");

        let descriptor = server.plugin().resolve().dynamic_tools[0]
            .descriptor()
            .clone();
        assert_eq!(
            descriptor.parameters["properties"]["token"]["x-sensitive"],
            true
        );
        // Redaction keys off the projected descriptor alone.
        let redacted = crate::sensitive::redact_arguments(
            &descriptor.parameters,
            &serde_json::json!({ "token": "sk-live", "other": 1 }),
        );
        assert_eq!(redacted["token"], crate::sensitive::REDACTED);
        assert_eq!(redacted["other"], 1);

        // The refreshed tool set is marked too.
        tx.send(ListChangedKind::Tools).expect("send");
        for _ in 0..50 {
            if server.version() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let descriptor = server.plugin().resolve().dynamic_tools[0]
            .descriptor()
            .clone();
        assert_eq!(descriptor.id, "mcp__srv__beta");
        assert_eq!(
            descriptor.parameters["properties"]["token"]["x-sensitive"],
            true
        );
    }
}
