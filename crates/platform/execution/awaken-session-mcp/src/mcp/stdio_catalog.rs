// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: Catalog storage supplies a bounded discovery hint and never admits a tool call.
use std::{sync::Arc, time::Duration};

use awaken_agent_contract::mcp_target::CachedMcpTool;
use awaken_session_configuration_contract::mcp::tool_catalog::{
    StdioToolCatalog, StdioToolCatalogKey, StdioToolDiscovery, VersionedStdioToolCatalog,
};

use super::{
    McpSessions, McpStagingSession, SessionMcpStates, attachment::staging::StagingAdmission,
};
use crate::McpProjectionState;
use awaken_session_configuration_contract::mcp::McpGenerationRef;

const IO_BUDGET: Duration = Duration::from_millis(250);

/// Exact launch inputs plus the Session's live decision to permit sharing.
#[derive(Clone)]
pub struct StdioCatalogAccess {
    pub key: StdioToolCatalogKey,
    pub launch_fingerprint: String,
    pub may_share: Arc<dyn Fn() -> bool + Send + Sync>,
}

#[derive(Clone)]
pub(crate) struct Catalog {
    store: Arc<dyn StdioToolDiscovery>,
    access: StdioCatalogAccess,
    server_name: String,
}

pub(crate) struct PendingObservation {
    pub(super) catalog: Catalog,
    pub(super) expected: Option<Option<String>>,
    pub(super) admission: StagingAdmission,
}

pub(super) fn active<S: SessionMcpStates>(
    slots: &S,
    generation: &McpGenerationRef,
    allocation: &Arc<tokio::sync::Mutex<()>>,
    admission: StagingAdmission,
    now: u64,
) -> bool {
    let current = slots
        .read_mcp(generation.session_id().as_str(), |mcp| {
            mcp.projections
                .iter()
                .find(|p| Arc::ptr_eq(&p.drain, allocation))
                .map(|p| p.request.generation.clone())
        })
        .ok()
        .flatten()
        .flatten();
    current.is_some_and(|current| {
        slots
            .modify_mcp_with_root(&current, |mcp, root| {
                mcp.admits_realization()
                    && admission.admits(root, &current, now)
                    && mcp.projections.iter().any(|p| {
                        Arc::ptr_eq(&p.drain, allocation)
                            && p.request.generation == current
                            && p.state == McpProjectionState::Active
                    })
            })
            .ok()
            .flatten()
            .unwrap_or(false)
    })
}

impl Catalog {
    pub(super) fn for_stage(
        mcp: &McpSessions,
        host: &dyn McpStagingSession,
        request: &awaken_session_configuration_contract::mcp::StageMcpAttachment,
    ) -> Option<Self> {
        let store = mcp.stdio_catalog.clone()?;
        let access = host.stdio_tool_catalog_access(request)?;
        Some(Self {
            store,
            access,
            server_name: request.name.clone(),
        })
    }

    pub(super) fn matches(&self, catalog: &StdioToolCatalog) -> bool {
        if catalog.launch_fingerprint != self.access.launch_fingerprint
            || !(self.access.may_share)()
        {
            return false;
        }
        let mut names = std::collections::HashSet::new();
        catalog.tools.len() <= 4_096
            && catalog.tools.iter().all(|tool| {
                if !names.insert(tool.name.as_str()) {
                    return false;
                }
                let mut definition = awaken_ext_mcp::McpToolDefinition::new(&tool.name);
                definition.description = Some(tool.description.clone());
                definition.input_schema = tool.input_schema.clone();
                awaken_ext_mcp::mcp_tool_descriptor(&self.server_name, &definition).is_ok()
            })
    }

    /// Outer None represents a failure, inner None an authoritative miss.
    pub(super) async fn read(&self) -> Option<Option<VersionedStdioToolCatalog>> {
        if !(self.access.may_share)() {
            return None;
        }
        match tokio::time::timeout(IO_BUDGET, self.store.get(&self.access.key)).await {
            Ok(Ok(value)) => Some(value),
            _ => {
                tracing::debug!("stdio MCP catalog read unavailable");
                None
            }
        }
    }

    pub(super) async fn expected(&self) -> Option<Option<String>> {
        self.read()
            .await
            .map(|entry| entry.map(|entry| entry.version))
    }

    pub(super) async fn publish(
        &self,
        expected: Option<&str>,
        plugin: &awaken_ext_mcp::McpPlugin,
        now: u64,
    ) {
        if !(self.access.may_share)() {
            return;
        }
        let Some(definitions) = plugin.regular_tool_definitions() else {
            return;
        };
        let mut tools = definitions
            .into_iter()
            .map(|tool| CachedMcpTool {
                name: tool.name,
                description: tool.description.unwrap_or_default(),
                input_schema: tool.input_schema,
            })
            .collect::<Vec<_>>();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        let catalog = StdioToolCatalog {
            observed_at_unix_ms: now,
            launch_fingerprint: self.access.launch_fingerprint.clone(),
            tools,
        };
        match tokio::time::timeout(
            IO_BUDGET,
            self.store
                .put_if_version(&self.access.key, expected, &catalog),
        )
        .await
        {
            Ok(Ok(true)) => (),
            Ok(Ok(false)) => tracing::debug!("stdio MCP catalog write conflicted"),
            _ => tracing::debug!("stdio MCP catalog write unavailable"),
        }
    }

    pub(super) fn observe(
        self,
        plugin: &Arc<awaken_ext_mcp::McpPlugin>,
        mut versions: tokio::sync::watch::Receiver<u64>,
        clock: Arc<dyn awaken_time::WallClock>,
        still_admitted: impl Fn() -> bool + Send + 'static,
        mut expected: Option<Option<String>>,
    ) {
        let plugin = Arc::downgrade(plugin);
        tokio::spawn(async move {
            while versions.changed().await.is_ok() {
                let Some(plugin) = plugin.upgrade() else {
                    return;
                };
                if !still_admitted() {
                    return;
                }
                if let Some(version) = expected.take() {
                    self.publish(
                        version.as_deref(),
                        &plugin,
                        awaken_time::WallClockExt::now_ms(clock.as_ref()),
                    )
                    .await;
                }
                // After a conflict another Session owns the current observation.
                // Read its token for a subsequent notification, never retry this write.
                expected = self.expected().await;
            }
        });
    }
}
