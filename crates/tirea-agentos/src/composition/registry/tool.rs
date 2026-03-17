use super::sorted_registry_ids;
use super::traits::{ToolRegistry, ToolRegistryError};
use crate::contracts::runtime::tool_call::Tool;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

#[derive(Clone, Default)]
pub struct InMemoryToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl std::fmt::Debug for InMemoryToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryToolRegistry")
            .field("len", &self.tools.len())
            .finish()
    }
}

impl InMemoryToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(id).cloned()
    }

    pub fn ids(&self) -> impl Iterator<Item = &String> {
        self.tools.keys()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<(), ToolRegistryError> {
        let id = tool.descriptor().id;
        if self.tools.contains_key(&id) {
            return Err(ToolRegistryError::ToolIdConflict(id));
        }
        self.tools.insert(id, tool);
        Ok(())
    }

    pub fn register_named(
        &mut self,
        id: impl Into<String>,
        tool: Arc<dyn Tool>,
    ) -> Result<(), ToolRegistryError> {
        let key = id.into();
        let descriptor_id = tool.descriptor().id;
        if key != descriptor_id {
            return Err(ToolRegistryError::ToolIdMismatch { key, descriptor_id });
        }
        if self.tools.contains_key(&key) {
            return Err(ToolRegistryError::ToolIdConflict(key));
        }
        self.tools.insert(key, tool);
        Ok(())
    }

    pub fn extend_named(
        &mut self,
        tools: HashMap<String, Arc<dyn Tool>>,
    ) -> Result<(), ToolRegistryError> {
        for (key, tool) in tools {
            self.register_named(key, tool)?;
        }
        Ok(())
    }

    pub fn extend_registry(&mut self, other: &dyn ToolRegistry) -> Result<(), ToolRegistryError> {
        self.extend_named(other.snapshot())
    }

    pub fn merge_many(
        regs: impl IntoIterator<Item = InMemoryToolRegistry>,
    ) -> Result<InMemoryToolRegistry, ToolRegistryError> {
        let mut out = InMemoryToolRegistry::new();
        for r in regs {
            out.extend_named(r.into_map())?;
        }
        Ok(out)
    }

    pub fn into_map(self) -> HashMap<String, Arc<dyn Tool>> {
        self.tools
    }

    pub fn to_map(&self) -> HashMap<String, Arc<dyn Tool>> {
        self.tools.clone()
    }
}

impl ToolRegistry for InMemoryToolRegistry {
    fn len(&self) -> usize {
        self.len()
    }

    fn get(&self, id: &str) -> Option<Arc<dyn Tool>> {
        self.get(id)
    }

    fn ids(&self) -> Vec<String> {
        sorted_registry_ids(&self.tools)
    }

    fn snapshot(&self) -> HashMap<String, Arc<dyn Tool>> {
        self.tools.clone()
    }
}

#[derive(Clone, Default)]
pub struct CompositeToolRegistry {
    registries: Vec<Arc<dyn ToolRegistry>>,
    cached_snapshot: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>>,
}

impl std::fmt::Debug for CompositeToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snapshot = match self.cached_snapshot.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        f.debug_struct("CompositeToolRegistry")
            .field("registries", &self.registries.len())
            .field("len", &snapshot.len())
            .finish()
    }
}

impl CompositeToolRegistry {
    pub fn try_new(
        regs: impl IntoIterator<Item = Arc<dyn ToolRegistry>>,
    ) -> Result<Self, ToolRegistryError> {
        let registries: Vec<Arc<dyn ToolRegistry>> = regs.into_iter().collect();
        let merged = Self::merge_snapshots(&registries)?;
        Ok(Self {
            registries,
            cached_snapshot: Arc::new(RwLock::new(merged)),
        })
    }

    fn merge_snapshots(
        registries: &[Arc<dyn ToolRegistry>],
    ) -> Result<HashMap<String, Arc<dyn Tool>>, ToolRegistryError> {
        let mut merged = InMemoryToolRegistry::new();
        for reg in registries {
            merged.extend_registry(reg.as_ref())?;
        }
        Ok(merged.into_map())
    }

    fn refresh_snapshot(&self) -> Result<HashMap<String, Arc<dyn Tool>>, ToolRegistryError> {
        Self::merge_snapshots(&self.registries)
    }

    fn read_cached_snapshot(&self) -> HashMap<String, Arc<dyn Tool>> {
        match self.cached_snapshot.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn write_cached_snapshot(&self, snapshot: HashMap<String, Arc<dyn Tool>>) {
        match self.cached_snapshot.write() {
            Ok(mut guard) => *guard = snapshot,
            Err(poisoned) => *poisoned.into_inner() = snapshot,
        };
    }
}

impl ToolRegistry for CompositeToolRegistry {
    fn len(&self) -> usize {
        self.snapshot().len()
    }

    fn get(&self, id: &str) -> Option<Arc<dyn Tool>> {
        self.snapshot().get(id).cloned()
    }

    fn ids(&self) -> Vec<String> {
        let snapshot = self.snapshot();
        sorted_registry_ids(&snapshot)
    }

    fn snapshot(&self) -> HashMap<String, Arc<dyn Tool>> {
        match self.refresh_snapshot() {
            Ok(snapshot) => {
                self.write_cached_snapshot(snapshot.clone());
                snapshot
            }
            Err(_) => self.read_cached_snapshot(),
        }
    }
}

// ---------------------------------------------------------------------------
// MCP ToolRegistry bridge
// ---------------------------------------------------------------------------

#[cfg(feature = "mcp")]
impl ToolRegistry for tirea_extension_mcp::McpToolRegistry {
    fn len(&self) -> usize {
        tirea_extension_mcp::McpToolRegistry::len(self)
    }

    fn get(&self, id: &str) -> Option<Arc<dyn Tool>> {
        tirea_extension_mcp::McpToolRegistry::get(self, id)
    }

    fn ids(&self) -> Vec<String> {
        tirea_extension_mcp::McpToolRegistry::ids(self)
    }

    fn snapshot(&self) -> HashMap<String, Arc<dyn Tool>> {
        tirea_extension_mcp::McpToolRegistry::snapshot(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::runtime::tool_call::{ToolDescriptor, ToolError, ToolResult};
    use crate::contracts::ToolCallContext;
    use serde_json::json;

    struct StaticTool {
        descriptor: ToolDescriptor,
    }

    impl StaticTool {
        fn new(id: &str) -> Self {
            Self {
                descriptor: ToolDescriptor::new(id, id, "test tool"),
            }
        }
    }

    #[async_trait::async_trait]
    impl Tool for StaticTool {
        fn descriptor(&self) -> ToolDescriptor {
            self.descriptor.clone()
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCallContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::success(
                self.descriptor.id.clone(),
                json!({"ok": true}),
            ))
        }
    }

    #[derive(Default)]
    struct MutableToolRegistry {
        tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
    }

    impl MutableToolRegistry {
        fn replace_ids(&self, ids: &[&str]) {
            let mut map = HashMap::new();
            for id in ids {
                map.insert(
                    (*id).to_string(),
                    Arc::new(StaticTool::new(id)) as Arc<dyn Tool>,
                );
            }
            match self.tools.write() {
                Ok(mut guard) => *guard = map,
                Err(poisoned) => *poisoned.into_inner() = map,
            }
        }
    }

    impl ToolRegistry for MutableToolRegistry {
        fn len(&self) -> usize {
            self.snapshot().len()
        }

        fn get(&self, id: &str) -> Option<Arc<dyn Tool>> {
            self.snapshot().get(id).cloned()
        }

        fn ids(&self) -> Vec<String> {
            let mut ids: Vec<String> = self.snapshot().keys().cloned().collect();
            ids.sort();
            ids
        }

        fn snapshot(&self) -> HashMap<String, Arc<dyn Tool>> {
            match self.tools.read() {
                Ok(guard) => guard.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            }
        }
    }

    #[test]
    fn composite_tool_registry_reads_live_updates_from_source_registries() {
        let dynamic = Arc::new(MutableToolRegistry::default());
        dynamic.replace_ids(&["dynamic_a"]);

        let mut static_registry = InMemoryToolRegistry::new();
        static_registry
            .register_named("static_tool", Arc::new(StaticTool::new("static_tool")))
            .expect("register static tool");

        let composite = CompositeToolRegistry::try_new(vec![
            dynamic.clone() as Arc<dyn ToolRegistry>,
            Arc::new(static_registry) as Arc<dyn ToolRegistry>,
        ])
        .expect("compose registries");

        assert!(composite.ids().contains(&"dynamic_a".to_string()));
        assert!(composite.ids().contains(&"static_tool".to_string()));

        dynamic.replace_ids(&["dynamic_a", "dynamic_b"]);

        let ids = composite.ids();
        assert!(ids.contains(&"dynamic_a".to_string()));
        assert!(ids.contains(&"dynamic_b".to_string()));
        assert!(ids.contains(&"static_tool".to_string()));
    }

    #[test]
    fn composite_tool_registry_keeps_last_good_snapshot_on_runtime_conflict() {
        let reg_a = Arc::new(MutableToolRegistry::default());
        reg_a.replace_ids(&["tool_a"]);

        let reg_b = Arc::new(MutableToolRegistry::default());
        reg_b.replace_ids(&["tool_b"]);

        let composite = CompositeToolRegistry::try_new(vec![
            reg_a.clone() as Arc<dyn ToolRegistry>,
            reg_b.clone() as Arc<dyn ToolRegistry>,
        ])
        .expect("compose registries");

        let initial_ids = composite.ids();
        assert_eq!(
            initial_ids,
            vec!["tool_a".to_string(), "tool_b".to_string()]
        );

        // Introduce a conflict at runtime. Composite should fall back to last good snapshot.
        reg_b.replace_ids(&["tool_a"]);

        assert_eq!(composite.ids(), initial_ids);
        assert!(composite.get("tool_b").is_some());
    }
}

#[cfg(all(test, feature = "mcp"))]
mod mcp_tests {
    use super::*;
    use async_trait::async_trait;
    use mcp::transport::{McpServerConnectionConfig, McpTransportError, TransportTypeId};
    use mcp::McpToolDefinition;
    use serde_json::Value;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    use crate::composition::{AgentDefinition, AgentDefinitionSpec};
    use crate::runtime::AgentOs;
    use tirea_extension_mcp::{McpProgressUpdate, McpToolRegistryManager, McpToolTransport};

    #[derive(Debug, Clone)]
    struct MutableTransport {
        tools: Arc<Mutex<Vec<McpToolDefinition>>>,
    }

    impl MutableTransport {
        fn new(tools: Vec<McpToolDefinition>) -> Self {
            Self {
                tools: Arc::new(Mutex::new(tools)),
            }
        }

        fn replace(&self, tools: Vec<McpToolDefinition>) {
            match self.tools.lock() {
                Ok(mut guard) => *guard = tools,
                Err(poisoned) => *poisoned.into_inner() = tools,
            }
        }
    }

    #[async_trait]
    impl McpToolTransport for MutableTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            let tools = match self.tools.lock() {
                Ok(guard) => guard.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            };
            Ok(tools)
        }

        async fn call_tool(
            &self,
            _name: &str,
            _args: Value,
            _progress_tx: Option<mpsc::UnboundedSender<McpProgressUpdate>>,
        ) -> Result<mcp::CallToolResult, McpTransportError> {
            Ok(mcp::CallToolResult {
                content: vec![mcp::ToolContent::text("ok")],
                structured_content: None,
                is_error: None,
            })
        }

        fn transport_type(&self) -> TransportTypeId {
            TransportTypeId::Stdio
        }
    }

    fn cfg(name: &str) -> McpServerConnectionConfig {
        McpServerConnectionConfig::stdio(name, "unused", vec![])
    }

    #[tokio::test]
    async fn mcp_registry_implements_dynamic_tool_registry() {
        let transport = Arc::new(MutableTransport::new(vec![McpToolDefinition::new("echo")]));
        let manager = McpToolRegistryManager::from_transports([(
            cfg("mcp_s1"),
            transport.clone() as Arc<dyn McpToolTransport>,
        )])
        .await
        .expect("build manager");
        let registry = Arc::new(manager.registry()) as Arc<dyn ToolRegistry>;

        let os = AgentOs::builder()
            .with_agent_spec(AgentDefinitionSpec::local_with_id(
                "assistant",
                AgentDefinition::new("gpt-4o-mini"),
            ))
            .with_tool_registry(registry)
            .build()
            .expect("build agent os");

        let resolved1 = os.resolve("assistant").expect("resolve first snapshot");
        assert!(resolved1.tools.contains_key("mcp__mcp_s1__echo"));
        assert!(!resolved1.tools.contains_key("mcp__mcp_s1__sum"));

        transport.replace(vec![McpToolDefinition::new("sum")]);
        manager.refresh().await.expect("refresh registry");

        let resolved2 = os.resolve("assistant").expect("resolve refreshed snapshot");
        assert!(!resolved2.tools.contains_key("mcp__mcp_s1__echo"));
        assert!(resolved2.tools.contains_key("mcp__mcp_s1__sum"));
    }
}
