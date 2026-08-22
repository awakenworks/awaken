//! ACP launch projection for the Session environment owned by the Runtime Host.
//!
//! Environment discovery and realization happen before this adapter is constructed.
//! This module only resolves a published launch, projects per-run configuration,
//! and starts the ACP process inside that already-bound environment.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_run_executor_acp::{AcpCli, AgentChannelSource, AgentSession, OpenError};
use awaken_runtime_contract::activation::RunActivation;
use awaken_sandbox_local::NamespaceProvider;

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
        }
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
        if let Some(cli) = cli
            && !backend_owned
        {
            if self.sandbox.is_container() {
                launch.argv = cli
                    .container_argv
                    .iter()
                    .map(|part| (*part).to_string())
                    .collect();
            }
            // The resolver's host config-home path is outside a namespace/container
            // sandbox. Point every CLI — including AcpSession CLIs with no generated
            // config file — at a directory realized inside the bound Session
            // environment. A sentinel forces the directory to exist before launch.
            let config_home = self.sandbox.config_home();
            let config_home_logical = self.sandbox.config_home_logical();
            self.sandbox
                .materialize_inline(&format!("{config_home_logical}/.awaken-config-home"), b"")
                .await
                .map_err(|error| OpenError(format!("materialize ACP config home: {error}")))?;
            if let Some(config_home_env) = cli.config_home_env {
                launch.env.retain(|var| var.name != config_home_env);
                launch.env.push(pc::EnvVar {
                    name: config_home_env.to_string(),
                    value: pc::EnvValue::Inline {
                        value: config_home.to_string(),
                    },
                    visibility: pc::EnvVisibility::Process,
                });
            }
            for alias in cli.config_home_aliases {
                launch.env.retain(|var| var.name != *alias);
                launch.env.push(pc::EnvVar {
                    name: (*alias).to_string(),
                    value: pc::EnvValue::Inline {
                        value: config_home.to_string(),
                    },
                    visibility: pc::EnvVisibility::Process,
                });
            }
            // Do not expose the operator's home to an opaque managed CLI. This
            // gives it one writable, Session-isolated configuration directory.
            launch.env.retain(|var| var.name != "HOME");
            launch.env.push(pc::EnvVar {
                name: "HOME".to_string(),
                value: pc::EnvValue::Inline { value: config_home },
                visibility: pc::EnvVisibility::Process,
            });
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
        let injection = match cli {
            Some(cli) => {
                awaken_run_executor_acp::mcp_injection_from_session_servers(cli, &self.mcp_servers)?
            }
            None => awaken_run_executor_acp::McpInjection::default(),
        };
        awaken_run_executor_acp::admit_mcp_injection(launch.identity, &injection)?;
        let config_home = self.sandbox.config_home();
        let config_home_logical = self.sandbox.config_home_logical();
        if let Some(cli) = cli
            && let Some((mount, (env_key, env_val))) = acp_config_mount(
                cli,
                injection.config_file.clone(),
                &config_home_logical,
                &config_home,
                false,
            )
        {
            let pc::MountSource::Inline { contents } = mount.source else {
                return Err(OpenError(
                    "ACP config projection did not produce an inline mount".to_string(),
                ));
            };
            self.sandbox
                .materialize_inline(&mount.mount_path, contents.as_bytes())
                .await
                .map_err(|error| OpenError(format!("materialize ACP config: {error}")))?;
            launch.env.retain(|var| var.name != env_key);
            launch.env.push(pc::EnvVar {
                name: env_key,
                value: pc::EnvValue::Inline { value: env_val },
                visibility: pc::EnvVisibility::Process,
            });
        }
        let (process, channel) = self
            .sandbox
            .spawn_agent(launch_command(&launch))
            .await
            .map_err(|error| OpenError(format!("bound agent launch: {error}")))?;
        Ok(AgentSession {
            channel,
            process: Arc::from(process),
            codec: self.codec,
            // ACP's cwd is separate from the spawned process cwd and is authoritative
            // for the CLI's own file tools. Point it at the same Session environment
            // that owns the staged File/Repository/MemoryStore projections.
            workspace_cwd: Some(self.sandbox.workspace_cwd()),
            mcp_session_servers: injection.session_servers,
            session_model: launch.session_model,
            session_mode: launch.session_mode,
            session_config_options: launch.session_config_options,
            expected_capability: launch.expected_capability,
        })
    }
}

fn launch_command(launch: &awaken_run_executor_acp::AcpLaunch) -> pc::Command {
    pc::Command {
        argv: launch.argv.clone(),
        cwd: String::new(),
        env: launch.env.clone(),
        stdio: pc::Stdio::Piped,
    }
}

/// The interior config mount + config-home env override for a config-file CLI's
/// projected MCP config: an inline, per-run, never-harvested
/// mount at the fixed interior config home, plus `(config_home_env, path)` to point the
/// CLI there. `None` for a session-server CLI. `read_only` when the backend can enforce
/// it (bwrap); the Workdir tier cannot, so it takes a read-write copy — the never-harvest
/// property holds regardless (`Inline` is neither `Secret` nor `MemoryStore`). Pure, so
/// it is testable without a live sandbox.
fn acp_config_mount(
    cli: &AcpCli,
    config_file: Option<(String, String)>,
    materialization_home: &str,
    exposed_home: &str,
    read_only: bool,
) -> Option<(pc::MountRequirement, (String, String))> {
    let (rel_path, contents) = config_file?;
    Some((
        pc::MountRequirement {
            mount_id: "acp-config".to_string(),
            source: pc::MountSource::Inline { contents },
            mount_path: format!("{materialization_home}/{rel_path}"),
            access: if read_only {
                pc::MountAccess::ReadOnly
            } else {
                pc::MountAccess::ReadWrite
            },
            lifetime: pc::MountLifetime::PerRun,
            required: true,
        },
        (cli.config_home_env?.to_string(), exposed_home.to_string()),
    ))
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
                    max_steps: 4,
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
            })
        }
    }

    struct BackendOwnedResolver {
        selection: awaken_runtime_contract::resolved::BackendModelSelection,
        model: &'static str,
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
    }

    struct ArtifactResolver {
        broker: Arc<dyn pc::SecretBroker>,
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
                return Err(pc::SandboxError::new("unexpected artifact reference"));
            }
            Ok(br#"{"auth_mode":"chatgpt","tokens":{"access_token":"secret"}}"#.to_vec())
        }

        async fn materialize_process(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
            Err(pc::SandboxError::new("not a process credential"))
        }

        async fn write_back(
            &self,
            _reference: &str,
            _bytes: Vec<u8>,
        ) -> Result<(), pc::SandboxError> {
            Err(pc::SandboxError::new("per-run artifact is not durable"))
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
        command: Mutex<Option<pc::Command>>,
        materialized: Mutex<Vec<(String, Vec<u8>)>>,
    }

    impl Default for CapturingAgentSandbox {
        fn default() -> Self {
            Self {
                container: true,
                host_identity: false,
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
                ..Self::default()
            }
        }

        fn namespace() -> Self {
            Self {
                container: false,
                host_identity: false,
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
            SANDBOX_CONFIG_HOME.to_string()
        }

        fn config_home_logical(&self) -> String {
            if self.container {
                SANDBOX_CONFIG_HOME.to_string()
            } else {
                WORKDIR_CONFIG_HOME.to_string()
            }
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
        sandbox: Arc<CapturingAgentSandbox>,
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
        }
    }

    fn bound_backend_owned_source(
        sandbox: Arc<CapturingAgentSandbox>,
        cli_id: &str,
        selection: awaken_runtime_contract::resolved::BackendModelSelection,
        model: &'static str,
    ) -> BoundLocalChannelSource {
        let cli = awaken_run_executor_acp::acp_cli(cli_id).expect("known ACP CLI");
        BoundLocalChannelSource {
            sandbox,
            launch: LaunchSource::Projected(AcpLaunchRegistry::single(
                *cli,
                Arc::new(BackendOwnedResolver { selection, model }),
            )),
            codec: awaken_run_executor_acp::Codec::Acp,
            backend: awaken_runtime_contract::resolved::Backend::from_ref(&format!("acp:{cli_id}")),
            mcp_servers: Vec::new(),
        }
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
    async fn backend_owned_identity_is_host_only_and_never_materialized() {
        // Cause graph: BackendOwned -> Workdir host identity -> PATH/HOME + CLI.
        // Namespace/container cannot make host identity available without copying
        // credentials, so both terminate before materialization or spawn.
        //
        // Decision table:
        // H1 Workdir + Default     -> host HOME, no provider env/files, spawn
        // H2 Workdir + Codex Exact -> same identity + ACP Session model option
        // H3 Namespace + any       -> reject before spawn/materialization
        // H4 Container + any       -> reject before spawn/materialization
        let sandbox = Arc::new(CapturingAgentSandbox::local());
        let source = bound_backend_owned_source(
            sandbox.clone(),
            "gemini",
            awaken_runtime_contract::resolved::BackendModelSelection::Default,
            "",
        );
        let session = source
            .open(
                &acp_activation("acp:gemini"),
                &awaken_runtime_contract::RuntimeRunContext::new(),
            )
            .await
            .expect("H1");
        assert!(session.session_config_options.is_empty(), "H1");
        assert!(sandbox.materialized.lock().unwrap().is_empty(), "H1");
        {
            let command = sandbox.command.lock().unwrap();
            let command = command.as_ref().expect("H1 spawned");
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
            assert_eq!(inline("HOME"), std::env::var("HOME").ok().as_deref(), "H1");
            assert!(
                command.env.iter().all(|entry| {
                    !matches!(&entry.value, pc::EnvValue::Secret { .. })
                        && !entry.name.contains("API_KEY")
                        && !entry.name.ends_with("_MODEL")
                        && entry.name != "GEMINI_DIR"
                }),
                "H1"
            );
        }

        let sandbox = Arc::new(CapturingAgentSandbox::local());
        let source = bound_backend_owned_source(
            sandbox.clone(),
            "codex",
            awaken_runtime_contract::resolved::BackendModelSelection::Exact,
            "gpt-exact",
        );
        let session = source
            .open(
                &acp_activation("acp:codex"),
                &awaken_runtime_contract::RuntimeRunContext::new(),
            )
            .await
            .expect("H2");
        let selection = session.session_config_options.first().expect("H2");
        assert_eq!(selection.config_id, "model", "H2");
        assert_eq!(selection.value, "gpt-exact", "H2");
        assert!(sandbox.materialized.lock().unwrap().is_empty(), "H2");

        for (rule, sandbox) in [
            ("H3", Arc::new(CapturingAgentSandbox::namespace())),
            ("H4", Arc::new(CapturingAgentSandbox::default())),
        ] {
            let source = bound_backend_owned_source(
                sandbox.clone(),
                "claude",
                awaken_runtime_contract::resolved::BackendModelSelection::Default,
                "",
            );
            let error = source
                .open(
                    &acp_activation("acp:claude"),
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
        for (container, expect_path) in [(false, true), (true, false)] {
            let sandbox = Arc::new(if container {
                CapturingAgentSandbox::default()
            } else {
                CapturingAgentSandbox::local()
            });
            let source = BoundLocalChannelSource {
                sandbox: sandbox.clone(),
                launch: LaunchSource::Fixed(AcpLaunch::custom(vec!["fixture".into()], vec![])),
                codec: awaken_run_executor_acp::Codec::Newline,
                backend: awaken_runtime_contract::resolved::Backend::from_ref("acp:fixture"),
                mcp_servers: Vec::new(),
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

    #[tokio::test]
    async fn bound_container_delivers_session_new_mcp_servers() {
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
        source.mcp_servers = awaken_run_executor_acp::mcp_injection_from_servers(
            awaken_run_executor_acp::acp_cli("claude").unwrap(),
            &routes,
        )
        .unwrap()
        .session_servers;

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

    #[test]
    fn config_file_projection_uses_the_bound_environment_paths() {
        // Causes: config-file delivery present/absent; environment can/cannot
        // enforce read-only. Effects: one inline per-run mount plus the exposed
        // config-home override, or no projection.
        //
        // Decision table:
        // C1 present + read-only capable -> ReadOnly at materialization path
        // C2 present + not capable       -> ReadWrite copy at materialization path
        // C3 absent + either             -> no mount and no environment override
        let mut cli = *awaken_run_executor_acp::acp_cli("claude").unwrap();
        cli.config_home_env = Some("TEST_CONFIG_HOME");

        for (rule, read_only, expected_access) in [
            ("C1", true, pc::MountAccess::ReadOnly),
            ("C2", false, pc::MountAccess::ReadWrite),
        ] {
            let (mount, (key, exposed)) = acp_config_mount(
                &cli,
                Some(("config.toml".into(), "[mcp_servers.gh]\n".into())),
                WORKDIR_CONFIG_HOME,
                SANDBOX_CONFIG_HOME,
                read_only,
            )
            .unwrap_or_else(|| panic!("{rule}"));
            assert_eq!(mount.mount_path, ".acp-config/config.toml", "{rule}");
            assert_eq!(mount.access, expected_access, "{rule}");
            assert_eq!(mount.lifetime, pc::MountLifetime::PerRun, "{rule}");
            assert!(
                matches!(mount.source, pc::MountSource::Inline { .. }),
                "{rule}"
            );
            assert_eq!(key, "TEST_CONFIG_HOME", "{rule}");
            assert_eq!(exposed, SANDBOX_CONFIG_HOME, "{rule}");
        }
        assert!(
            acp_config_mount(&cli, None, WORKDIR_CONFIG_HOME, SANDBOX_CONFIG_HOME, true,).is_none(),
            "C3"
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
