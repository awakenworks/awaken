//! ACP launch projection for the Session environment owned by the Runtime Host.
//!
//! Environment discovery and realization happen before this adapter is constructed.
//! This module only resolves a published launch, projects per-run configuration,
//! and starts the ACP process inside that already-bound environment.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_run_executor_acp::{AgentChannelSource, AgentSession, OpenError};
use awaken_runtime_contract::activation::RunActivation;
use awaken_sandbox_local::NamespaceProvider;

mod attached_tools;
mod launch;
pub use launch::{AcpLaunchRegistry, LaunchSource};

/// Launches ACP inside the environment already owned by the Session.
///
/// Discovery, capability publication, placement, and environment realization are
/// upstream responsibilities. This adapter only consumes their immutable result.
pub(crate) struct BoundLocalChannelSource {
    sandbox: Arc<dyn crate::session_environment::AgentSandbox>,
    launch: LaunchSource,
    codec: awaken_run_executor_acp::Codec,
    backend: awaken_runtime_contract::resolved::Backend,
    mcp_servers: Vec<awaken_run_executor_acp::SessionMcpServer>,
    attached_tools: Option<Arc<crate::acp_tool_export::AttachedToolSet>>,
}

/// The single launch-kind decision for Session MCP route ownership.
/// Projected catalog CLIs must pass their adapter capability gate, the trusted
/// official-ACP fixture consumes routes already admitted by the Host, and the
/// legacy newline fixture has no ACP `session/new` surface at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AcpMcpProjectionAdmission {
    CatalogChecked,
    TrustedFixedAcp,
    Omit,
}

#[must_use]
const fn acp_mcp_projection_admission(
    projected: bool,
    trusted_fixed_acp: bool,
) -> AcpMcpProjectionAdmission {
    if projected {
        AcpMcpProjectionAdmission::CatalogChecked
    } else if trusted_fixed_acp {
        AcpMcpProjectionAdmission::TrustedFixedAcp
    } else {
        AcpMcpProjectionAdmission::Omit
    }
}

#[cfg(kani)]
#[kani::proof]
fn acp_mcp_projection_has_one_exact_authority() {
    // Causes: C1 the launch is selected from the CLI catalog; C2 the launch is
    // the trusted official-ACP fixture. Constraint K1: C1 and C2 are mutually
    // exclusive variants of `LaunchSource`. Effects: E1 catalog capability
    // checking owns admission; E2 preserve Host-admitted routes exactly; E3
    // omit routes for the newline-only fixture. Decision table: R1 C1=>E1;
    // R2 !C1+C2=>E2; R3 !C1+!C2=>E3. The impossible C1+C2 combination is
    // constrained out so no synthetic precedence is credited as coverage.
    let projected = kani::any::<bool>();
    let trusted_fixed_acp = kani::any::<bool>();
    kani::assume(!(projected && trusted_fixed_acp));
    let selected = acp_mcp_projection_admission(projected, trusted_fixed_acp);
    if projected {
        assert_eq!(selected, AcpMcpProjectionAdmission::CatalogChecked);
    } else if trusted_fixed_acp {
        assert_eq!(selected, AcpMcpProjectionAdmission::TrustedFixedAcp);
    } else {
        assert_eq!(selected, AcpMcpProjectionAdmission::Omit);
    }
}

impl BoundLocalChannelSource {
    pub(crate) fn from_environment(
        sandbox: Arc<crate::session_environment::SessionEnvironment>,
        launch: LaunchSource,
        backend: awaken_runtime_contract::resolved::Backend,
        mcp_servers: Vec<awaken_run_executor_acp::SessionMcpServer>,
    ) -> Self {
        let codec = match &launch {
            LaunchSource::Fixed(_) => awaken_run_executor_acp::Codec::Newline,
            LaunchSource::FixedAcp(_) | LaunchSource::Projected(_) => {
                awaken_run_executor_acp::Codec::Acp
            }
        };
        Self {
            sandbox,
            launch,
            codec,
            backend,
            mcp_servers,
            attached_tools: None,
        }
    }

    pub(crate) fn with_attached_tools(
        mut self,
        tools: Option<Arc<crate::acp_tool_export::AttachedToolSet>>,
    ) -> Self {
        self.attached_tools = tools;
        self
    }

    /// Realize the one writable Session directory used by ACP adapter state and
    /// managed configuration. The Session Environment remains its only path and
    /// teardown owner.
    async fn materialize_session_home(&self) -> Result<String, OpenError> {
        let home = self.sandbox.config_home();
        let logical = self.sandbox.config_home_logical();
        self.sandbox
            .materialize_inline(&format!("{logical}/.awaken-config-home"), b"")
            .await
            .map_err(|error| OpenError(format!("materialize ACP config home: {error}")))?;
        Ok(home)
    }
}

#[async_trait]
impl AgentChannelSource for BoundLocalChannelSource {
    async fn open(
        &self,
        activation: &RunActivation,
        context: &awaken_runtime_contract::runtime_context::RuntimeRunContext,
    ) -> Result<AgentSession, OpenError> {
        let resolved = self
            .launch
            .resolve(activation, &self.backend, context)
            .await?;
        let mut launch = resolved.launch;
        let cli = self.launch.cli(&self.backend)?;
        let backend_owned =
            launch.identity == awaken_run_executor_acp::AcpLaunchIdentity::BackendOwned;
        if backend_owned && !self.sandbox.supports_host_identity() {
            return Err(OpenError(
                "backend-owned local ACP identity requires the Workdir isolation tier".into(),
            ));
        }
        // Cause/decision table for executable lookup and ambient isolation:
        //
        // | rule | container | projected CLI | result |
        // | L1   | false     | false         | local PATH/HOME allowlist |
        // | L2   | false     | true          | allowlist, then Session HOME |
        // | L3   | true      | false         | image argv/env only |
        // | L4   | true      | true          | container argv + Session HOME |
        //
        // C1=host executable lookup is required; C2=the image owns lookup;
        // C3=an opaque projected CLI requires an isolated home. L1/L2 satisfy
        // C1 through the one ACP allowlist, L3/L4 exclude host PATH because C2,
        // and L2/L4 replace any admitted host HOME because C3.
        if backend_owned {
            launch.env = awaken_run_executor_acp::with_backend_owned_host_environment(launch.env);
        } else if !self.sandbox.is_container() {
            launch.env = awaken_run_executor_acp::with_local_host_launch_environment(launch.env);
        }
        if let Some(cli) = cli {
            if !backend_owned && self.sandbox.is_container() {
                launch.argv = cli
                    .container_argv
                    .iter()
                    .map(|part| (*part).to_string())
                    .collect();
            }

            // The resolver's host config-home path is outside a namespace/container
            // sandbox. A managed CLI needs its complete home inside the Session;
            // backend-owned TrustedHost execution preserves the host login and state
            // as one identity boundary. Both remain branches of this one launch path.
            if !backend_owned {
                let config_home = self.materialize_session_home().await?;
                if let Some(config_home_env) = cli.config_home_env {
                    launch.env.retain(|var| var.name != config_home_env);
                    launch.env.push(pc::EnvVar {
                        name: config_home_env.to_string(),
                        value: pc::EnvValue::Inline {
                            value: config_home.clone(),
                        },
                        visibility: pc::EnvVisibility::Process,
                    });
                }
                for alias in cli.config_home_aliases {
                    launch.env.retain(|var| var.name != *alias);
                    launch.env.push(pc::EnvVar {
                        name: (*alias).to_string(),
                        value: pc::EnvValue::Inline {
                            value: config_home.clone(),
                        },
                        visibility: pc::EnvVisibility::Process,
                    });
                }
                // Do not expose the operator's home to an opaque managed CLI.
                // This gives it one writable, Session-isolated config directory.
                launch.env.retain(|var| var.name != "HOME");
                launch.env.push(pc::EnvVar {
                    name: "HOME".to_string(),
                    value: pc::EnvValue::Inline { value: config_home },
                    visibility: pc::EnvVisibility::Process,
                });
            }
        }
        if let Some(artifact) = resolved.credential_artifact {
            let broker = resolved.secret_broker.ok_or_else(|| {
                OpenError("credential_provision_failed: secret broker unavailable".into())
            })?;
            let bytes = broker
                .materialize(artifact.reference())
                .await
                .map_err(|error| OpenError(format!("credential_provision_failed: {error}")))?;
            let path = format!(
                "{}/{}",
                self.sandbox.config_home_logical().trim_end_matches('/'),
                artifact.relative_path()
            );
            self.sandbox
                .materialize_inline(&path, &bytes)
                .await
                .map_err(|error| OpenError(format!("credential_provision_failed: {error}")))?;
        }
        let mcp_projection = acp_mcp_projection_admission(
            cli.is_some(),
            matches!(&self.launch, LaunchSource::FixedAcp(_)),
        );
        let mut mcp_session_servers = match mcp_projection {
            AcpMcpProjectionAdmission::CatalogChecked => {
                let cli = cli.expect("catalog admission requires the selected CLI");
                awaken_run_executor_acp::admit_mcp_session_servers(cli, &self.mcp_servers)?
            }
            AcpMcpProjectionAdmission::TrustedFixedAcp => self.mcp_servers.clone(),
            AcpMcpProjectionAdmission::Omit => Vec::new(),
        };
        let attached = match &self.attached_tools {
            Some(tools) => {
                let adapter = cli.ok_or_else(|| {
                    OpenError("attached MCP tools require a catalog ACP adapter".into())
                })?;
                if mcp_session_servers
                    .iter()
                    .any(|server| server.name == "awaken_session")
                {
                    return Err(OpenError(
                        "duplicate attached Session MCP server name".into(),
                    ));
                }
                let attached =
                    attached_tools::AttachedTools::open(self.sandbox.as_ref(), tools).await?;
                let admitted = match awaken_run_executor_acp::admit_mcp_session_servers(
                    adapter,
                    std::slice::from_ref(&attached.server),
                ) {
                    Ok(admitted) => admitted,
                    Err(error) => {
                        attached
                            .close()
                            .await
                            .map_err(|cleanup| OpenError(format!("{error}; cleanup: {cleanup}")))?;
                        return Err(error);
                    }
                };
                mcp_session_servers.extend(admitted);
                Some(attached)
            }
            None => None,
        };
        let filesystem = self
            .sandbox
            .filesystem_view(launch.session_working_directory.as_deref());
        let spawned = self
            .sandbox
            .spawn_agent(launch_command(&launch, &filesystem.command_cwd))
            .await;
        let (process, channel) = match spawned {
            Ok(spawned) => spawned,
            Err(error) => {
                if let Some(attached) = &attached {
                    attached
                        .close()
                        .await
                        .map_err(|error| OpenError(error.to_string()))?;
                }
                return Err(OpenError(format!("bound agent launch: {error}")));
            }
        };
        let process: Arc<dyn pc::ProcessHandle> = match attached {
            Some(attached) => Arc::new(attached_tools::ProcessWithTools { process, attached }),
            None => Arc::from(process),
        };
        Ok(AgentSession {
            channel,
            process,
            codec: self.codec,
            // ACP's cwd is separate from the spawned process cwd and is authoritative
            // for the CLI's own file tools. Point it at the same Session environment
            // that owns the staged File/Repository/MemoryStore projections.
            workspace_cwd: Some(filesystem.protocol_cwd),
            mcp_session_servers,
            session_model: launch.session_model,
            session_mode: launch.session_mode,
            tool_aliases: launch.tool_aliases,
            session_config_options: launch.session_config_options,
            expected_capability: launch.expected_capability,
        })
    }
}

fn launch_command(launch: &awaken_run_executor_acp::AcpLaunch, cwd: &str) -> pc::Command {
    pc::Command {
        argv: launch.argv.clone(),
        cwd: cwd.to_string(),
        env: launch.env.clone(),
        stdio: pc::Stdio::Piped,
    }
}

/// Resolve the effective sandbox tier at startup, probing the OS-native launcher
/// ONCE (memoized) for the `Namespace` tier so an unsupported host gets a clear startup
/// decision instead of an opaque per-run spawn error. An unavailable provider fails
/// closed (`Err`) by default — the caller turns it into a startup abort with guidance.
/// Only when typed deployment policy opts in do we degrade to the UNSANDBOXED
/// `Local` tier with a loud notice so a dev/single-tenant worker runs.
/// Every other tier passes through unchanged.
pub async fn resolve_sandbox_tier(
    tier: crate::deployment_config::SandboxTier,
    allow_local_fallback: bool,
    namespace_base: &std::path::Path,
) -> Result<crate::deployment_config::SandboxTier, String> {
    use crate::deployment_config::SandboxTier;
    use awaken_provisioning_contract::SandboxProvider;
    if tier != SandboxTier::Namespace {
        return Ok(tier);
    }
    match NamespaceProvider::new(namespace_base.to_path_buf())
        .probe_ready()
        .await
    {
        Ok(()) => Ok(SandboxTier::Namespace),
        Err(e) if namespace_degrades_to_local(allow_local_fallback) => {
            eprintln!(
                "awaken: OS-native sandbox unavailable ({e}); running UNSANDBOXED local ACP \
                 execution (no OS isolation for this worker). Install bwrap on Linux or enable \
                 macOS Seatbelt, or disable sandbox.allow_local_fallback to require \
                 isolation (fail closed)."
            );
            Ok(SandboxTier::Local)
        }
        Err(e) => Err(format!(
            "OS-native sandbox unavailable: {e}. Install bwrap (Linux) / use macOS Seatbelt, \
             configure sandbox_allow_local_fallback=true for an explicit unsafe fallback, or \
             configure sandbox_tier=local to run unsandboxed"
        )),
    }
}

/// Whether an unavailable `Namespace` provider may degrade to the UNSANDBOXED
/// `Local` tier. Isolation never silently downgrades: the explicit unsafe opt-in is
/// required whether the namespace tier was configured explicitly or selected as a
/// default. Pure, so the policy is unit-testable off-host.
fn namespace_degrades_to_local(fallback_optin: bool) -> bool {
    fallback_optin
}

/// The image a container tier requires, or a fail-closed error naming the config var.
#[cfg(any(
    feature = "container-docker",
    feature = "container-podman",
    feature = "container-k8s"
))]
pub(crate) fn container_image(image: Option<&str>) -> Result<String, String> {
    image
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "a container sandbox tier requires AWAKEN_CONTAINER_IMAGE".to_string())
}

#[cfg(test)]
mod tests {
    mod attached;
    use super::*;
    use awaken_run_executor_acp::{AcpLaunch, LaunchResolver};
    use std::sync::Mutex;

    fn base() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("awaken-sbxsrc-ut-{}", std::process::id()))
    }

    const SANDBOX_CONFIG_HOME: &str = "/acp-config";
    const WORKDIR_CONFIG_HOME: &str = ".acp-config";
    const SANDBOX_WORKSPACE: &str = "/workspace";

    fn acp_activation(backend_ref: &str) -> RunActivation {
        acp_activation_with_plugin_config(backend_ref, Default::default())
    }

    fn acp_activation_with_plugin_config(
        backend_ref: &str,
        plugin_config: std::collections::BTreeMap<String, serde_json::Value>,
    ) -> RunActivation {
        use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
        use awaken_runtime_contract::snapshot::{
            AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
        };
        RunActivation::new(
            awaken_agent_contract::agent::run::Id("r".into()),
            awaken_agent_contract::agent::thread::Id("t".into()),
            ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("s".into()),
                metadata: Default::default(),
                root_agent_id: AgentId("a".into()),
                resolved_spec: ResolvedSpec {
                    model_candidates: Vec::new(),
                    catalog_fingerprint: CatalogFingerprint("fp".into()),
                    instructions: String::new(),
                    max_steps: Some(4),
                    delegation_limits: Default::default(),
                    model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                        ModelBinding::new("prov", "m", backend_ref),
                    ),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: plugin_config.into(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint: CatalogFingerprint("fp".into()),
            },
            Vec::new(),
        )
    }

    struct FakeResolver;

    #[async_trait]
    impl LaunchResolver for FakeResolver {
        async fn model(
            &self,
            _activation: &RunActivation,
            _context: &awaken_runtime_contract::RuntimeRunContext,
        ) -> Result<awaken_run_executor_acp::ResolvedModel, OpenError> {
            Ok(awaken_run_executor_acp::ResolvedModel::Managed {
                base_url: "http://model.invalid".into(),
                model: "model".into(),
                process_secret: None,
                credential_artifact: None,
                acp: None,
                provider_server_tools: Vec::new(),
            })
        }
    }

    struct BackendOwnedResolver {
        selection: awaken_runtime_contract::resolved::BackendModelSelection,
        model: &'static str,
        extra_env: Vec<(String, String)>,
    }

    #[async_trait]
    impl LaunchResolver for BackendOwnedResolver {
        async fn model(
            &self,
            _activation: &RunActivation,
            _context: &awaken_runtime_contract::RuntimeRunContext,
        ) -> Result<awaken_run_executor_acp::ResolvedModel, OpenError> {
            Ok(awaken_run_executor_acp::ResolvedModel::backend_owned(
                self.selection,
                self.model,
                "codex",
                "test",
                "sha256:test",
                Default::default(),
            ))
        }

        fn extra_env(
            &self,
            _activation: &RunActivation,
        ) -> Result<Vec<(String, String)>, OpenError> {
            Ok(self.extra_env.clone())
        }
    }

    struct ArtifactResolver {
        broker: Arc<dyn pc::SecretBroker>,
    }

    struct WorkingDirectoryResolver;

    #[async_trait]
    impl LaunchResolver for WorkingDirectoryResolver {
        async fn model(
            &self,
            _activation: &RunActivation,
            _context: &awaken_runtime_contract::RuntimeRunContext,
        ) -> Result<awaken_run_executor_acp::ResolvedModel, OpenError> {
            Ok(awaken_run_executor_acp::ResolvedModel::backend_owned(
                awaken_runtime_contract::resolved::BackendModelSelection::Default,
                "",
                "codex",
                "test",
                "sha256:test",
                awaken_runtime_contract::resolved::AcpSessionConfiguration {
                    working_directory: Some("repo/src".into()),
                    ..Default::default()
                },
            ))
        }
    }

    #[async_trait]
    impl LaunchResolver for ArtifactResolver {
        async fn model(
            &self,
            _activation: &RunActivation,
            _context: &awaken_runtime_contract::RuntimeRunContext,
        ) -> Result<awaken_run_executor_acp::ResolvedModel, OpenError> {
            Ok(awaken_run_executor_acp::ResolvedModel::Managed {
                base_url: "http://model.invalid".into(),
                model: "model".into(),
                process_secret: None,
                credential_artifact: Some(
                    awaken_run_executor_acp::CredentialArtifactRequirement::new(
                        "credential-artifact://one-shot",
                        ".codex/auth.json",
                    ),
                ),
                acp: None,
                provider_server_tools: Vec::new(),
            })
        }

        fn secret_broker(&self) -> Option<Arc<dyn pc::SecretBroker>> {
            Some(self.broker.clone())
        }
    }

    struct ArtifactBroker;

    #[async_trait]
    impl pc::SecretBroker for ArtifactBroker {
        async fn materialize(&self, reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
            if reference != "credential-artifact://one-shot" {
                return Err(pc::SandboxError::not_found("unexpected artifact reference"));
            }
            Ok(br#"{"auth_mode":"chatgpt","tokens":{"access_token":"secret"}}"#.to_vec())
        }

        async fn materialize_process(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
            Err(pc::SandboxError::invalid("not a process credential"))
        }

        async fn write_back(
            &self,
            _target: pc::SecretWritebackTarget<'_>,
            _bytes: Vec<u8>,
        ) -> Result<(), pc::SandboxError> {
            Err(pc::SandboxError::unsupported(
                "per-run artifact is not durable",
            ))
        }
    }

    struct FakeProcess;

    #[async_trait]
    impl pc::ProcessHandle for FakeProcess {
        fn id(&self) -> &str {
            "agent"
        }

        async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
            Ok(pc::ExitStatus {
                code: Some(0),
                signaled: false,
            })
        }

        async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
            Ok(None)
        }

        async fn signal(&self, _signal: pc::Signal) -> Result<(), pc::SandboxError> {
            Ok(())
        }
    }

    struct CapturingAgentSandbox {
        container: bool,
        host_identity: bool,
        config_home: String,
        config_home_logical: String,
        command: Mutex<Option<pc::Command>>,
        materialized: Mutex<Vec<(String, Vec<u8>)>>,
    }

    impl Default for CapturingAgentSandbox {
        fn default() -> Self {
            Self {
                container: true,
                host_identity: false,
                config_home: SANDBOX_CONFIG_HOME.to_string(),
                config_home_logical: SANDBOX_CONFIG_HOME.to_string(),
                command: Mutex::new(None),
                materialized: Mutex::new(Vec::new()),
            }
        }
    }

    impl CapturingAgentSandbox {
        fn local() -> Self {
            Self {
                container: false,
                host_identity: true,
                config_home_logical: WORKDIR_CONFIG_HOME.to_string(),
                ..Self::default()
            }
        }

        fn namespace() -> Self {
            Self {
                container: false,
                host_identity: false,
                config_home_logical: WORKDIR_CONFIG_HOME.to_string(),
                ..Self::default()
            }
        }
    }

    #[async_trait]
    impl crate::session_environment::AgentSandbox for CapturingAgentSandbox {
        fn is_container(&self) -> bool {
            self.container
        }

        fn supports_host_identity(&self) -> bool {
            self.host_identity
        }

        fn config_home(&self) -> String {
            self.config_home.clone()
        }

        fn config_home_logical(&self) -> String {
            self.config_home_logical.clone()
        }

        fn workspace_cwd(&self) -> String {
            SANDBOX_WORKSPACE.to_string()
        }

        async fn materialize_inline(
            &self,
            logical: &str,
            contents: &[u8],
        ) -> Result<(), pc::SandboxError> {
            self.materialized
                .lock()
                .unwrap()
                .push((logical.to_string(), contents.to_vec()));
            Ok(())
        }

        async fn spawn_agent(
            &self,
            command: pc::Command,
        ) -> Result<
            (
                Box<dyn pc::ProcessHandle>,
                Box<dyn awaken_run_executor_acp::AgentChannelType>,
            ),
            pc::SandboxError,
        > {
            *self.command.lock().unwrap() = Some(command);
            let (channel, peer) = tokio::io::duplex(64);
            tokio::spawn(async move {
                let _peer = peer;
            });
            Ok((Box::new(FakeProcess), Box::new(channel)))
        }
    }

    fn bound_projecting_source(
        sandbox: Arc<dyn crate::session_environment::AgentSandbox>,
        cli_id: &str,
    ) -> BoundLocalChannelSource {
        let cli = awaken_run_executor_acp::acp_cli(cli_id).expect("known ACP CLI");
        BoundLocalChannelSource {
            sandbox,
            launch: LaunchSource::Projected(AcpLaunchRegistry::single(
                *cli,
                Arc::new(FakeResolver),
            )),
            codec: awaken_run_executor_acp::Codec::Acp,
            backend: awaken_runtime_contract::resolved::Backend::from_ref(&format!("acp:{cli_id}")),
            mcp_servers: Vec::new(),
            attached_tools: None,
        }
    }

    fn bound_backend_owned_source(
        sandbox: Arc<CapturingAgentSandbox>,
        cli_id: &str,
        selection: awaken_runtime_contract::resolved::BackendModelSelection,
        model: &'static str,
    ) -> BoundLocalChannelSource {
        bound_backend_owned_source_with_env(sandbox, cli_id, selection, model, Vec::new())
    }

    fn bound_backend_owned_source_with_env(
        sandbox: Arc<CapturingAgentSandbox>,
        cli_id: &str,
        selection: awaken_runtime_contract::resolved::BackendModelSelection,
        model: &'static str,
        extra_env: Vec<(String, String)>,
    ) -> BoundLocalChannelSource {
        let cli = awaken_run_executor_acp::acp_cli(cli_id).expect("known ACP CLI");
        BoundLocalChannelSource {
            sandbox,
            launch: LaunchSource::Projected(AcpLaunchRegistry::single(
                *cli,
                Arc::new(BackendOwnedResolver {
                    selection,
                    model,
                    extra_env,
                }),
            )),
            codec: awaken_run_executor_acp::Codec::Acp,
            backend: awaken_runtime_contract::resolved::Backend::from_ref(&format!("acp:{cli_id}")),
            mcp_servers: Vec::new(),
            attached_tools: None,
        }
    }

    fn inline_command_env<'a>(command: &'a pc::Command, name: &str) -> Option<&'a str> {
        command.env.iter().find_map(|entry| {
            if entry.name != name {
                return None;
            }
            match &entry.value {
                pc::EnvValue::Inline { value } => Some(value.as_str()),
                pc::EnvValue::Secret { .. } => None,
            }
        })
    }

    #[tokio::test]
    async fn bound_container_runs_the_selected_preinstalled_cli() {
        let sandbox = Arc::new(CapturingAgentSandbox::default());
        let source = bound_projecting_source(sandbox.clone(), "claude");

        source
            .open(
                &acp_activation("acp:claude"),
                &awaken_runtime_contract::RuntimeRunContext::new(),
            )
            .await
            .expect("open the bound container agent");

        let command = sandbox.command.lock().unwrap();
        assert_eq!(
            command.as_ref().expect("agent was spawned").argv,
            vec!["claude-agent-acp"],
            "the bound container uses the catalog's preinstalled argv"
        );
        assert!(
            !command
                .as_ref()
                .unwrap()
                .env
                .iter()
                .any(|entry| entry.name == "PATH"),
            "L4: a container must use image lookup rather than the Worker's PATH"
        );
    }

    #[tokio::test]
    async fn configured_working_directory_drives_process_and_acp_from_one_view() {
        // Cause/effect graph: C1 provider view={sandbox absolute}; C2 validated
        // working_directory={absent,repo/src}. E1 process default is the
        // canonical workspace; E2 configured cwd is rooted at WorkspaceLayout::ROOT;
        // E3 ACP session/new and process receive the identical path. Invalid
        // authored paths are rejected by the Agent contract/resolver tests.
        // Decision rules: C2 absent=>E1; C2 configured=>E2+E3.
        let sandbox = Arc::new(CapturingAgentSandbox::local());
        let default =
            crate::session_environment::AgentSandbox::filesystem_view(sandbox.as_ref(), None);
        assert_eq!(default.command_cwd, "/workspace", "E1 process");
        assert_eq!(default.protocol_cwd, "/workspace", "E1 ACP");
        let cli = *awaken_run_executor_acp::acp_cli("codex").unwrap();
        let source = BoundLocalChannelSource {
            sandbox: sandbox.clone(),
            launch: LaunchSource::Projected(AcpLaunchRegistry::single(
                cli,
                Arc::new(WorkingDirectoryResolver),
            )),
            codec: awaken_run_executor_acp::Codec::Acp,
            backend: awaken_runtime_contract::resolved::Backend::from_ref("acp:codex"),
            mcp_servers: Vec::new(),
            attached_tools: None,
        };
        let session = source
            .open(
                &acp_activation("acp:codex"),
                &awaken_runtime_contract::RuntimeRunContext::new(),
            )
            .await
            .expect("configured cwd");
        let expected = "/workspace/repo/src";
        assert_eq!(session.workspace_cwd.as_deref(), Some(expected), "E2/E3");
        assert_eq!(
            sandbox
                .command
                .lock()
                .unwrap()
                .as_ref()
                .expect("spawned")
                .cwd,
            expected,
            "E2/E3"
        );
    }

    #[tokio::test]
    async fn bound_local_launch_reuses_host_path_but_replaces_host_home() {
        let sandbox = Arc::new(CapturingAgentSandbox::local());
        let source = bound_projecting_source(sandbox.clone(), "gemini");

        source
            .open(
                &acp_activation("acp:gemini"),
                &awaken_runtime_contract::RuntimeRunContext::new(),
            )
            .await
            .expect("open the bound local agent");

        let command = sandbox.command.lock().unwrap();
        let command = command.as_ref().expect("agent was spawned");
        let inline = |name: &str| {
            command.env.iter().find_map(|entry| {
                if entry.name != name {
                    return None;
                }
                match &entry.value {
                    pc::EnvValue::Inline { value } => Some(value.as_str()),
                    pc::EnvValue::Secret { .. } => None,
                }
            })
        };
        assert_eq!(
            inline("PATH"),
            std::env::var("PATH").ok().as_deref(),
            "L2: a local projected CLI uses the canonical host PATH allowlist"
        );
        assert_eq!(
            inline("HOME"),
            Some(SANDBOX_CONFIG_HOME),
            "L2: the Session config home replaces any admitted host HOME"
        );
        assert_eq!(command.argv.first().map(String::as_str), Some("gemini"));
    }

    #[tokio::test]
    async fn backend_owned_identity_preserves_the_complete_host_boundary() {
        // Cause/effect graph: C1 provisioning is BackendOwned; C2 the environment
        // is trusted Workdir or an isolated tier; C3 an upstream projection may
        // carry an explicit Codex process setting. Effects: E1 host PATH/HOME and
        // the CLI-owned login/state remain intact; E2 no Session config home or
        // managed credential is materialized; E3 an explicit modeled process
        // setting survives; E4 isolated tiers reject before launch. Ambient
        // non-allowlisted variables are covered at the subprocess env_clear gate.
        //
        // Decision table:
        // | rule | tier      | conflict state env | effect    |
        // | H1   | Workdir   | yes                | E1+E2+E3  |
        // | H2   | Namespace | any                | E4        |
        // | H3   | Container | any                | E4        |
        let sandbox = Arc::new(CapturingAgentSandbox::local());
        let source = bound_backend_owned_source_with_env(
            sandbox.clone(),
            "codex",
            awaken_runtime_contract::resolved::BackendModelSelection::Exact,
            "gpt-exact",
            vec![(
                "CODEX_SQLITE_HOME".to_string(),
                "/untrusted/upstream-state".to_string(),
            )],
        );
        let session = source
            .open(
                &acp_activation("acp:codex"),
                &awaken_runtime_contract::RuntimeRunContext::new(),
            )
            .await
            .expect("H1");
        let selection = session.session_config_options.first().expect("H1");
        assert_eq!(selection.config_id, "model", "H1");
        assert_eq!(selection.value, "gpt-exact", "H1");
        assert!(sandbox.materialized.lock().unwrap().is_empty(), "H1/E2");
        {
            let command = sandbox.command.lock().unwrap();
            let command = command.as_ref().expect("H1 spawned");
            assert_eq!(
                inline_command_env(command, "HOME"),
                std::env::var("HOME").ok().as_deref(),
                "H1/E1"
            );
            assert_eq!(
                inline_command_env(command, "CODEX_SQLITE_HOME"),
                Some("/untrusted/upstream-state"),
                "H1/E3: explicit modeled process input is not ambient inheritance"
            );
            assert!(
                command.env.iter().all(|entry| {
                    !matches!(&entry.value, pc::EnvValue::Secret { .. })
                        && !entry.name.contains("API_KEY")
                        && !entry.name.ends_with("_MODEL")
                        && entry.name != "GEMINI_DIR"
                }),
                "H1/E2"
            );
        }

        for (rule, sandbox) in [
            ("H2", Arc::new(CapturingAgentSandbox::namespace())),
            ("H3", Arc::new(CapturingAgentSandbox::default())),
        ] {
            let source = bound_backend_owned_source(
                sandbox.clone(),
                "codex",
                awaken_runtime_contract::resolved::BackendModelSelection::Default,
                "",
            );
            let error = source
                .open(
                    &acp_activation("acp:codex"),
                    &awaken_runtime_contract::RuntimeRunContext::new(),
                )
                .await
                .err()
                .unwrap_or_else(|| panic!("{rule}: expected rejection"));
            assert!(error.0.contains("Workdir isolation tier"), "{rule}");
            assert!(sandbox.command.lock().unwrap().is_none(), "{rule}");
            assert!(sandbox.materialized.lock().unwrap().is_empty(), "{rule}");
        }
    }

    #[tokio::test]
    async fn bound_codex_provisions_the_claimed_artifact_without_credential_environment() {
        // Causes: C1 Codex consumes provider coordinates through its OpenAI
        // dialect; C2 its credential delivery is an artifact. Effects: E1
        // base/model and non-secret provider config enter process env; E2
        // credential bytes exist only at the claimed auth.json path, never
        // OPENAI_API_KEY, Codex host-path env, or serialized provider config.
        let sandbox = Arc::new(CapturingAgentSandbox::default());
        let cli = *awaken_run_executor_acp::acp_cli("codex").expect("Codex ACP profile");
        let source = BoundLocalChannelSource {
            sandbox: sandbox.clone(),
            launch: LaunchSource::Projected(AcpLaunchRegistry::single(
                cli,
                Arc::new(ArtifactResolver {
                    broker: Arc::new(ArtifactBroker),
                }),
            )),
            codec: awaken_run_executor_acp::Codec::Acp,
            backend: awaken_runtime_contract::resolved::Backend::from_ref("acp:codex"),
            mcp_servers: Vec::new(),
            attached_tools: None,
        };

        source
            .open(
                &acp_activation("acp:codex"),
                &awaken_runtime_contract::RuntimeRunContext::new(),
            )
            .await
            .expect("provision Codex credential artifact");

        let files = sandbox.materialized.lock().unwrap();
        let (_, bytes) = files
            .iter()
            .find(|(path, _)| path == "/acp-config/.codex/auth.json")
            .expect("artifact uses the provider codec's exact sandbox path");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(bytes).unwrap()["auth_mode"],
            "chatgpt"
        );
        drop(files);

        let command = sandbox.command.lock().unwrap();
        let command = command.as_ref().expect("Codex was spawned");
        assert!(command.env.iter().any(|entry| {
            entry.name == "HOME"
                && entry.value
                    == pc::EnvValue::Inline {
                        value: SANDBOX_CONFIG_HOME.into(),
                    }
        }));
        let env = |name: &str| command.env.iter().find(|entry| entry.name == name);
        assert!(env("OPENAI_BASE_URL").is_some(), "E1");
        assert!(env("OPENAI_MODEL").is_some(), "E1");
        assert!(env("OPENAI_API_KEY").is_none(), "E2");
        assert!(env("CODEX_HOME").is_none(), "E2");
        assert_eq!(
            env("MODEL_PROVIDER").map(|entry| &entry.value),
            Some(&pc::EnvValue::Inline {
                value: "awaken-managed".into(),
            }),
            "E1"
        );
        let provider_config = env("CODEX_CONFIG").expect("E1 provider config");
        let pc::EnvValue::Inline { value } = &provider_config.value else {
            panic!("E1 provider config must be non-secret inline metadata");
        };
        let value: serde_json::Value = serde_json::from_str(value).expect("E1 valid config");
        assert_eq!(value["model"], "model", "E1");
        assert_eq!(
            value["model_providers"]["awaken-managed"]["base_url"], "http://model.invalid",
            "E1"
        );
        assert!(!value.to_string().contains("secret"), "E2");
    }

    #[tokio::test]
    async fn bound_fixed_launch_admits_host_path_only_outside_a_container() {
        // Fixed-launch cause/effect table: C1 newline/official ACP fixture;
        // C2 local/container Sandbox. F1/F2 either C1 + local => exact argv
        // and host PATH; F3/F4 either C1 + container => exact argv without
        // host PATH. Both fixtures share launch resolution; their wire codecs
        // remain different. No credentials or catalog route are synthesized.
        for official_acp in [false, true] {
            for (container, expect_path) in [(false, true), (true, false)] {
                let sandbox = Arc::new(if container {
                    CapturingAgentSandbox::default()
                } else {
                    CapturingAgentSandbox::local()
                });
                let launch = Box::new(AcpLaunch::custom(vec!["fixture".into()], vec![]));
                let source = BoundLocalChannelSource {
                    sandbox: sandbox.clone(),
                    launch: if official_acp {
                        LaunchSource::FixedAcp(launch)
                    } else {
                        LaunchSource::Fixed(launch)
                    },
                    codec: if official_acp {
                        awaken_run_executor_acp::Codec::Acp
                    } else {
                        awaken_run_executor_acp::Codec::Newline
                    },
                    backend: awaken_runtime_contract::resolved::Backend::from_ref("acp:fixture"),
                    mcp_servers: Vec::new(),
                    attached_tools: None,
                };
                source
                    .open(
                        &acp_activation("acp:fixture"),
                        &awaken_runtime_contract::RuntimeRunContext::new(),
                    )
                    .await
                    .expect("capture the fixed launch");
                let command = sandbox.command.lock().unwrap();
                assert_eq!(
                    command.as_ref().unwrap().argv,
                    vec!["fixture"],
                    "F1-F4 exact launch"
                );
                assert_eq!(
                    command
                        .as_ref()
                        .unwrap()
                        .env
                        .iter()
                        .any(|entry| entry.name == "PATH"),
                    expect_path,
                    "L1/L3: host lookup belongs only to a local launch"
                );
            }
        }
    }

    #[tokio::test]
    async fn trusted_fixed_acp_preserves_routes_while_newline_fixed_omits_them() {
        // Causes: C1 a trusted fixed launch speaks official ACP or newline;
        // C2 the Host supplies one already-admitted process-local MCP route.
        // Effects: E1 FixedAcp preserves the exact route for `session/new`; E2
        // Fixed omits it because newline has no ACP MCP projection. Constraint:
        // catalog capability checking is inapplicable because neither fixed
        // launch has a catalog CLI. Rules F1=C1(ACP)+C2=>E1 and
        // F2=C1(newline)+C2=>E2 cover both fixed launch variants.
        let routes = awaken_runtime_contract::resolved::AcpSpec::from_plugin_config(
            &std::collections::BTreeMap::from([(
                "acp".to_string(),
                serde_json::json!({ "mcp_servers": [{
                    "name": "awaken-session",
                    "transport": { "kind": "http", "url": "http://mcp.invalid" }
                }] }),
            )]),
        )
        .mcp_servers;
        let mcp_servers = awaken_run_executor_acp::mcp_session_servers_from_routes(&routes);

        for (official_acp, expected_routes) in [(true, 1), (false, 0)] {
            let sandbox = Arc::new(CapturingAgentSandbox::default());
            let launch = AcpLaunch::custom(vec!["fixture".into()], vec![]);
            let source = BoundLocalChannelSource {
                sandbox,
                launch: if official_acp {
                    LaunchSource::FixedAcp(Box::new(launch))
                } else {
                    LaunchSource::Fixed(Box::new(launch))
                },
                codec: if official_acp {
                    awaken_run_executor_acp::Codec::Acp
                } else {
                    awaken_run_executor_acp::Codec::Newline
                },
                backend: awaken_runtime_contract::resolved::Backend::from_ref("acp:fixture"),
                mcp_servers: mcp_servers.clone(),
                attached_tools: None,
            };
            let session = source
                .open(
                    &acp_activation("acp:fixture"),
                    &awaken_runtime_contract::RuntimeRunContext::new(),
                )
                .await
                .expect("open fixed launch");
            assert_eq!(session.mcp_session_servers.len(), expected_routes);
        }
    }

    #[tokio::test]
    async fn bound_container_delivers_session_new_mcp_servers() {
        // Cause/effect graph: C1 a container-owned SessionEnvironment is bound;
        // C2 the selected Claude adapter admits typed `session/new` MCP servers;
        // C3 the frozen Session projection supplies one exact staged HTTP route.
        // Effects: E1 the ACP Session uses the bound workspace and config home;
        // E2 the exact route reaches `AgentSession::mcp_session_servers`; E3 the
        // selected CLI receives its sandbox-interior config-home environment.
        // Constraint K1: route conversion and adapter admission remain owned by
        // `awaken-run-executor-acp`; this Host seam may only deliver that admitted
        // value and must not recreate a config-file or credential projection.
        // Decision rule B1=C1+C2+C3=>E1+E2+E3. Coverage rationale: the executor's
        // adjacent decision tables own empty, transport, and rejection states;
        // this case covers the remaining positive handoff into the bound process.
        let sandbox = Arc::new(CapturingAgentSandbox::default());
        let mut source = bound_projecting_source(sandbox.clone(), "claude");
        let activation = acp_activation_with_plugin_config(
            "acp:claude",
            std::collections::BTreeMap::from([(
                "acp".to_string(),
                serde_json::json!({ "mcp_servers": [{
                    "name": "github",
                    "transport": { "kind": "http", "url": "https://mcp.invalid" },
                    "credential": { "auth": "reference", "reference": "broker://github" }
                }] }),
            )]),
        );
        let routes = awaken_runtime_contract::resolved::AcpSpec::from_plugin_config(
            activation.snapshot.resolved_spec.plugin_config.plugins(),
        )
        .mcp_servers;
        source.mcp_servers = awaken_run_executor_acp::mcp_session_servers_from_routes(&routes);

        let session = source
            .open(
                &activation,
                &awaken_runtime_contract::RuntimeRunContext::new(),
            )
            .await
            .expect("open bound ACP agent");
        assert_eq!(
            session.workspace_cwd.as_deref(),
            Some(SANDBOX_WORKSPACE),
            "session/new must point the CLI's file tools at the bound workspace"
        );
        assert_eq!(session.mcp_session_servers.len(), 1);
        assert_eq!(session.mcp_session_servers[0].name, "github");
        assert!(
            sandbox
                .materialized
                .lock()
                .unwrap()
                .iter()
                .any(|(path, bytes)| path == "/acp-config/.awaken-config-home" && bytes.is_empty()),
            "an AcpSession CLI gets a realized writable config home"
        );
        let command = sandbox.command.lock().unwrap();
        assert!(
            command
                .as_ref()
                .unwrap()
                .env
                .iter()
                .any(|entry| entry.name == "CLAUDE_CONFIG_DIR"
                    && entry.value
                        == pc::EnvValue::Inline {
                            value: "/acp-config".to_string()
                        }),
            "the CLI points at the config home inside its sandbox"
        );
    }

    #[tokio::test]
    async fn bound_container_rejects_a_cli_other_than_the_one_it_serves() {
        let sandbox = Arc::new(CapturingAgentSandbox::default());
        let mut source = bound_projecting_source(sandbox.clone(), "claude");
        source.backend = awaken_runtime_contract::resolved::Backend::from_ref("acp:codex");

        let error = source
            .open(
                &acp_activation("acp:codex"),
                &awaken_runtime_contract::RuntimeRunContext::new(),
            )
            .await
            .err()
            .expect("a mismatched CLI must fail closed");

        assert!(error.0.contains("acp:codex") && error.0.contains("acp:claude"));
        assert!(
            sandbox.command.lock().unwrap().is_none(),
            "a mismatched run must not spawn in the SessionEnvironment"
        );
    }

    #[tokio::test]
    async fn resolve_sandbox_tier_passes_non_namespace_tiers_through_unprobed() {
        use crate::deployment_config::SandboxTier;
        // Only the namespace tier is bwrap-probed; the rest resolve to themselves.
        assert_eq!(
            resolve_sandbox_tier(SandboxTier::Local, true, &base())
                .await
                .unwrap(),
            SandboxTier::Local
        );
        assert_eq!(
            resolve_sandbox_tier(SandboxTier::Docker, true, &base())
                .await
                .unwrap(),
            SandboxTier::Docker
        );
    }

    #[tokio::test]
    async fn resolve_sandbox_tier_never_errors_for_namespace_with_the_local_fallback_optin() {
        use crate::deployment_config::SandboxTier;
        // Cause/effect decision table:
        // | Namespace ready | typed fallback | Effect |
        // | yes             | either         | Namespace |
        // | no              | true           | Local |
        // The assertion covers either host state without ambient configuration.
        let resolved = resolve_sandbox_tier(SandboxTier::Namespace, true, &base()).await;
        assert!(matches!(
            resolved,
            Ok(SandboxTier::Namespace | SandboxTier::Local)
        ));
    }

    #[test]
    fn namespace_degrade_policy_requires_an_explicit_unsafe_optin() {
        // Cause/effect decision table:
        // | Namespace ready | typed fallback | Effect |
        // | no              | false          | reject |
        // | no              | true           | Local |
        // Readiness is tested at the impure caller; this covers the pure policy.
        assert!(
            !namespace_degrades_to_local(false),
            "namespace fails closed without an explicit fallback opt-in"
        );
        assert!(
            namespace_degrades_to_local(true),
            "the explicit unsafe opt-in permits local fallback"
        );
    }
}
