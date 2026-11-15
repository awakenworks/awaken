// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: The installed attachment owns the process and its drain barrier. The tool compares the advertised descriptor before sending tools/call.
//! Session-owned stdio startup and the initial model-facing tool view.

use std::sync::{Arc, Weak};

use awaken_agent_contract::mcp_target::SandboxStdioMcpTarget;
use awaken_runtime_contract::{
    plugin::{Contributions, DynamicTool, Plugin, PluginManifest},
    resolved::ToolDescriptor,
    runs::RunError,
};
use awaken_session_configuration_contract::mcp::McpGenerationRef;
use awaken_session_environment_contract::{ProcessReaper, SessionEnvironment};
use awaken_sync::LeafLock;
use awaken_tool_contract::tool::{RawTool, ToolCall, ToolError, ToolOutput};

use super::{DeferredStdioEnvironment, SessionMcpStates, attachment::staging::StagingAdmission};
use crate::mcp_material::{
    McpGenerationProjection, McpProjectionState, McpStagingActivity, McpTransportMaterial,
    McpWiring,
};

pub(super) async fn sandbox_stdio_command(
    environment: &SessionEnvironment,
    target: &SandboxStdioMcpTarget,
) -> Result<awaken_provisioning_contract::Command, RunError> {
    use awaken_provisioning_contract::{Command, EnvValue, EnvVar, EnvVisibility, Stdio};
    let agent = environment.agent_process();
    let cwd = agent.workspace_cwd();
    let logical_home = format!(".mcp-home/{}", target.fingerprint);
    let home = format!("{}/{}", cwd.trim_end_matches('/'), logical_home);
    let sentinel = if agent.supports_host_identity() {
        format!("{logical_home}/.awaken-mcp-home")
    } else {
        format!("{home}/.awaken-mcp-home")
    };
    agent
        .materialize_inline(&sentinel, b"")
        .await
        .map_err(|error| {
            RunError::classified(
                "mcp_sandbox_home_failed",
                format!("sandbox stdio MCP home could not be materialized: {error}"),
            )
        })?;
    Ok(Command {
        argv: std::iter::once(target.command.clone())
            .chain(target.args.iter().cloned())
            .collect(),
        cwd,
        env: vec![EnvVar {
            name: "HOME".into(),
            value: EnvValue::Inline { value: home },
            visibility: EnvVisibility::Process,
        }],
        stdio: Stdio::Piped,
    })
}

pub(super) struct DeferredStdioOwner<S> {
    pub slots: Weak<S>,
    pub generation: McpGenerationRef,
    pub allocation: Arc<tokio::sync::Mutex<()>>,
    pub clock: Arc<dyn awaken_time::WallClock>,
    pub admission: StagingAdmission,
    pub reaper: Arc<dyn ProcessReaper>,
    pub environment: DeferredStdioEnvironment,
    pub catalog: Option<super::stdio_catalog::Catalog>,
}

pub(super) fn cached_wiring<S: SessionMcpStates>(
    owner: DeferredStdioOwner<S>,
    server: &McpTransportMaterial,
    target: &SandboxStdioMcpTarget,
    definitions: &[awaken_agent_contract::mcp_target::CachedMcpTool],
) -> Result<McpWiring, RunError> {
    let manifest = awaken_ext_mcp::McpPlugin::manifest_for(&server.name).map_err(invalid_cache)?;
    let cached = definitions
        .iter()
        .map(|tool| {
            let mut definition = awaken_ext_mcp::McpToolDefinition::new(&tool.name);
            definition.description = Some(tool.description.clone());
            definition.input_schema = tool.input_schema.clone();
            awaken_ext_mcp::mcp_tool_descriptor(&server.name, &definition).map_err(invalid_cache)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut wiring = McpWiring::empty();
    wiring.tool_ids = cached.iter().map(|tool| tool.id.clone()).collect();
    wiring.plugins.push(Arc::new(DeferredStdioPlugin {
        manifest,
        cached,
        connection: Arc::new(DeferredStdio {
            owner,
            server: server.clone(),
            target: target.clone(),
            connected: LeafLock::new(None),
            starting: tokio::sync::Mutex::new(()),
        }),
    }));
    Ok(wiring)
}

fn invalid_cache(error: impl std::fmt::Display) -> RunError {
    RunError::classified("mcp_cached_tools_invalid", error.to_string())
}

fn unavailable(error: impl std::fmt::Display) -> ToolError {
    ToolError::UnavailableBeforeDispatch(error.to_string())
}

fn inactive() -> ToolError {
    unavailable("stdio MCP attachment is no longer active")
}

struct DeferredStdio<S> {
    owner: DeferredStdioOwner<S>,
    server: McpTransportMaterial,
    target: SandboxStdioMcpTarget,
    connected: LeafLock<Option<Arc<awaken_ext_mcp::McpPlugin>>>,
    starting: tokio::sync::Mutex<()>,
}

impl<S: SessionMcpStates> DeferredStdio<S> {
    fn connected_plugin(&self) -> Option<Arc<awaken_ext_mcp::McpPlugin>> {
        self.connected.lock().clone()
    }

    /// Resolve renewals by the original allocation, then recheck the latest exact fence.
    fn active<R>(
        &self,
        slots: &S,
        modify: impl FnOnce(&mut McpGenerationProjection) -> Result<R, ToolError>,
    ) -> Result<R, ToolError> {
        let session = self.owner.generation.session_id();
        let generation = slots
            .read_mcp(session.as_str(), |mcp| {
                mcp.projections
                    .iter()
                    .find(|p| Arc::ptr_eq(&p.drain, &self.owner.allocation))
                    .map(|p| p.request.generation.clone())
            })
            .map_err(unavailable)?
            .flatten()
            .ok_or_else(inactive)?;
        slots
            .modify_mcp_with_root(&generation, |mcp, root| {
                if !mcp.admits_realization()
                    || !self.owner.admission.admits(
                        root,
                        &generation,
                        awaken_time::WallClockExt::now_ms(self.owner.clock.as_ref()),
                    )
                {
                    return Err(inactive());
                }
                let projection = mcp
                    .projections
                    .iter_mut()
                    .find(|p| {
                        p.request.generation == generation
                            && p.state == McpProjectionState::Active
                            && Arc::ptr_eq(&p.drain, &self.owner.allocation)
                    })
                    .ok_or_else(inactive)?;
                modify(projection)
            })
            .map_err(unavailable)?
            .ok_or_else(inactive)?
    }

    async fn connect(self: Arc<Self>) -> Result<Arc<awaken_ext_mcp::McpPlugin>, ToolError> {
        let _startup = self.starting.lock().await;
        let slots = self.owner.slots.upgrade().ok_or_else(inactive)?;
        self.active(slots.as_ref(), |_| Ok(()))?;
        if let Some(plugin) = self.connected_plugin() {
            return Ok(plugin);
        }
        // Environment realization may itself need to drain an earlier projection.
        // Register process activity only after obtaining the committed Environment.
        let environment = (self.owner.environment)().await.map_err(unavailable)?;
        let activity = McpStagingActivity::default();
        self.active(slots.as_ref(), |projection| {
            if projection.mcp_process.is_some() {
                return Err(unavailable(
                    "previous stdio MCP process still needs cleanup",
                ));
            }
            projection.staging = Some(activity.clone());
            Ok(())
        })?;
        let _activity = activity.guard();
        self.connect_in_environment(slots.as_ref(), &environment)
            .await
    }

    async fn connect_in_environment(
        self: &Arc<Self>,
        slots: &S,
        environment: &SessionEnvironment,
    ) -> Result<Arc<awaken_ext_mcp::McpPlugin>, ToolError> {
        let command = sandbox_stdio_command(environment, &self.target)
            .await
            .map_err(unavailable)?;
        self.active(slots, |_| Ok(()))?;
        let (process, channel) = environment
            .agent_process()
            .spawn_agent(command)
            .await
            .map_err(unavailable)?;
        let process: Arc<dyn awaken_provisioning_contract::ProcessHandle> = Arc::from(process);
        let result = match self.active(slots, |_| Ok(())) {
            Ok(()) => self.publish_connection(slots, &process, channel).await,
            Err(error) => Err(error),
        };
        if result.is_err()
            && let Err(cleanup) = self
                .owner
                .reaper
                .reap(process.as_ref(), std::time::Duration::from_secs(5))
                .await
        {
            // A concurrent drain waits for this activity, then rereads the
            // exact allocation to recover a child whose cleanup did not settle.
            let session = self.owner.generation.session_id();
            slots
                .modify_mcp(session.as_str(), |mcp| {
                    if let Some(projection) = mcp
                        .projections
                        .iter_mut()
                        .find(|p| Arc::ptr_eq(&p.drain, &self.owner.allocation))
                    {
                        projection.state = McpProjectionState::Draining;
                        projection.mcp_process = Some(process.clone());
                    }
                })
                .map_err(unavailable)?;
            return Err(unavailable(cleanup));
        }
        result
    }

    async fn publish_connection(
        self: &Arc<Self>,
        slots: &S,
        process: &Arc<dyn awaken_provisioning_contract::ProcessHandle>,
        channel: Box<dyn awaken_agent_channel::AgentChannel>,
    ) -> Result<Arc<awaken_ext_mcp::McpPlugin>, ToolError> {
        let expected = match &self.owner.catalog {
            Some(catalog) => catalog.expected().await,
            None => None,
        };
        let wiring = super::connect_sandbox_stdio(&self.server, channel)
            .await
            .map_err(unavailable)?;
        let plugin = wiring
            .connected_stdio
            .ok_or_else(|| unavailable("stdio MCP connection returned no tool registry"))?;
        let versions = plugin.catalog_versions();
        self.active(slots, |projection| {
            let cached_wiring = projection.native_wiring.as_mut().ok_or_else(inactive)?;
            cached_wiring.call_fences.extend(wiring.call_fences);
            projection.mcp_process = Some(process.clone());
            projection.staging = None;
            *self.connected.lock() = Some(plugin.clone());
            Ok(())
        })?;
        if let Some((catalog, expected)) = self.owner.catalog.as_ref().zip(expected)
            && self.active(slots, |_| Ok(())).is_ok()
        {
            catalog
                .publish(
                    expected.as_deref(),
                    plugin.as_ref(),
                    awaken_time::WallClockExt::now_ms(self.owner.clock.as_ref()),
                )
                .await;
        }
        if let Some(catalog) = self.owner.catalog.clone() {
            let expected = catalog.expected().await;
            let connection = Arc::downgrade(self);
            catalog.observe(
                &plugin,
                versions,
                self.owner.clock.clone(),
                move || {
                    connection.upgrade().is_some_and(|connection| {
                        connection.owner.slots.upgrade().is_some_and(|slots| {
                            connection.active(slots.as_ref(), |_| Ok(())).is_ok()
                        })
                    })
                },
                expected,
            );
        }
        Ok(plugin)
    }
}

struct DeferredStdioPlugin<S> {
    manifest: PluginManifest,
    cached: Vec<ToolDescriptor>,
    connection: Arc<DeferredStdio<S>>,
}

impl<S: SessionMcpStates> Plugin for DeferredStdioPlugin<S> {
    fn manifest(&self) -> PluginManifest {
        self.manifest.clone()
    }

    fn resolve(&self) -> Contributions {
        let current = self.connection.connected_plugin();
        let mut contributions = current.as_ref().map_or_else(
            || Contributions::new(self.manifest.id.clone()),
            |plugin| plugin.resolve(),
        );
        let descriptors = if current.is_some() {
            // Durable task support can only be negotiated on the live connection.
            // Retain those live tools with their task and transport fences intact.
            let mut regular = Vec::new();
            contributions.dynamic_tools.retain(|tool| {
                if tool.descriptor().kind == awaken_runtime_contract::resolved::ToolKind::Regular {
                    regular.push(tool.descriptor().clone());
                    false
                } else {
                    true
                }
            });
            regular
        } else {
            self.cached.clone()
        };
        for descriptor in descriptors {
            let tool = DeferredStdioTool {
                id: descriptor.id.clone(),
                advertised: descriptor.content_hash(),
                connection: self.connection.clone(),
            };
            contributions.register_dynamic_tool(
                DynamicTool::try_new(descriptor, Arc::new(tool))
                    .expect("deferred MCP tool uses its descriptor's exact identity"),
            );
        }
        contributions
    }

    fn live_version(&self) -> Option<u64> {
        Some(
            self.connection
                .connected_plugin()
                .and_then(|plugin| plugin.live_version())
                .unwrap_or(0),
        )
    }
}

struct DeferredStdioTool<S> {
    id: String,
    advertised: String,
    connection: Arc<DeferredStdio<S>>,
}

#[async_trait::async_trait]
impl<S: SessionMcpStates> RawTool for DeferredStdioTool<S> {
    fn id(&self) -> &str {
        &self.id
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let plugin = match self.connection.connected_plugin() {
            Some(plugin) => plugin,
            // The startup owns every external await even if the requesting Run drops.
            None => tokio::spawn(self.connection.clone().connect())
                .await
                .map_err(unavailable)??,
        };
        let slots = self.connection.owner.slots.upgrade().ok_or_else(inactive)?;
        self.connection.active(slots.as_ref(), |_| Ok(()))?;
        if plugin.is_refreshing_catalog() {
            return Err(unavailable(
                "mcp_tool_catalog_refreshing: Wait for MCP tool discovery before generating a new call.",
            ));
        }
        let current = plugin.resolve().dynamic_tools.into_iter().find(|tool| {
            tool.descriptor().id == self.id && tool.descriptor().content_hash() == self.advertised
        }).ok_or_else(|| unavailable(
            "mcp_tool_schema_changed: MCP tool definitions were refreshed. This call was not sent. Choose a tool and generate arguments using the current definitions.",
        ))?;
        // The live tool still uses the revocable transport installed on the
        // canonical attachment, including its in-flight drain accounting.
        current.executable().invoke(call).await
    }
}
