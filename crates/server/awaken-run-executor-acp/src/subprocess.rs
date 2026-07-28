//! A subprocess-backed [`AgentChannelSource`]: launch an ACP CLI as a child
//! process and pipe its stdio into an [`AgentChannel`] (R4).
//!
//! This is the *local* (unsandboxed) launch path — the composition root uses it
//! for a trusted CLI or a test agent; a sandbox provider is the isolated
//! production counterpart behind the same [`AgentChannelSource`] trait, so the
//! executor is unchanged either way. Per-CLI config projection is data
//! ([`AcpLaunch`]): the host resolves the model (config plane) and hands this
//! module typed coordinates plus an opaque process-secret requirement — no config
//! lookup or credential materialization policy lives here.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_channel::{AgentChannel, SplitChannel};
use awaken_local_process::{LocalProcess, configure_process_group};
use awaken_provisioning_contract as pc;
use awaken_runtime_contract::activation::RunActivation;
use tokio::process::Command;

use crate::{AcpCli, AgentChannelSource, AgentSession, OpenError, ResolvedModel};

/// A resolved launch for an ACP CLI: the argv plus the env to set (model, base
/// URL, key). Runtime-specific projection lives only in the [`AcpCli`] catalog;
/// this value is its mechanism-neutral output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpLaunchIdentity {
    Managed,
    BackendOwned,
}

#[derive(Debug, Clone)]
pub struct AcpLaunch {
    pub argv: Vec<String>,
    pub env: Vec<pc::EnvVar>,
    pub identity: AcpLaunchIdentity,
    pub session_mode: Option<String>,
    pub session_config_options: Vec<awaken_protocol_acp::SessionConfigOptionSelection>,
    pub expected_capability: Option<awaken_protocol_acp::AcpCapabilityExpectation>,
}

impl AcpLaunch {
    /// A custom launch (argv + env) — another CLI, or a test ACP agent.
    #[must_use]
    pub fn custom(argv: Vec<String>, env: Vec<(String, String)>) -> Self {
        Self {
            argv,
            env: env
                .into_iter()
                .map(|(name, value)| pc::EnvVar {
                    name,
                    value: pc::EnvValue::Inline { value },
                    visibility: pc::EnvVisibility::Process,
                })
                .collect(),
            identity: AcpLaunchIdentity::Managed,
            session_mode: None,
            session_config_options: Vec::new(),
            expected_capability: None,
        }
    }
}

#[cfg(test)]
fn inline_env(name: &str, value: &str) -> pc::EnvVar {
    pc::EnvVar {
        name: name.to_string(),
        value: pc::EnvValue::Inline {
            value: value.to_string(),
        },
        visibility: pc::EnvVisibility::Process,
    }
}

/// Host env keys passed through into the otherwise-cleared child env (the
/// `["PATH","HOME"]` allowlist): `PATH` so an absolute Node-backed wrapper can
/// resolve `node`, and `HOME` so a backend-owned CLI finds its own login and user
/// config. Everything else stays cleared — no ambient leak (G22). A key
/// already set by the projection (a modeled value) is never overridden.
const HOST_PASSTHROUGH_ENV: &[&str] = &["PATH", "HOME"];

/// Add the [`HOST_PASSTHROUGH_ENV`] allowlist to a launch's env, reading each from
/// `lookup` (the host env at spawn). Pure so the allowlist is unit-testable without
/// spawning; a projected key is not shadowed.
fn with_host_passthrough(
    mut env: Vec<pc::EnvVar>,
    lookup: impl Fn(&str) -> Option<String>,
) -> Vec<pc::EnvVar> {
    for key in HOST_PASSTHROUGH_ENV {
        if !env.iter().any(|var| var.name == *key)
            && let Some(value) = lookup(key)
        {
            env.push(pc::EnvVar {
                name: (*key).to_string(),
                value: pc::EnvValue::Inline { value },
                visibility: pc::EnvVisibility::Process,
            });
        }
    }
    env
}

/// Add the deliberately small host launch allowlist to a local ACP process.
///
/// Both the direct subprocess adapter and a Session-bound local/namespace
/// provider use this function. Keeping the policy here prevents the two launch
/// paths from drifting on which ambient values may cross the process boundary.
/// Container launches must not call this function: their executable lookup and
/// home belong to the image rather than the Worker host.
#[must_use]
pub fn with_local_host_launch_environment(env: Vec<pc::EnvVar>) -> Vec<pc::EnvVar> {
    with_host_passthrough(env, |key| std::env::var(key).ok())
}

/// Bind a backend-owned launch to the actual host user's PATH/HOME. Resolver
/// passthrough cannot replace either identity coordinate.
#[must_use]
pub fn with_backend_owned_host_environment(env: Vec<pc::EnvVar>) -> Vec<pc::EnvVar> {
    with_backend_owned_host_environment_from(env, |key| std::env::var(key).ok())
}

fn with_backend_owned_host_environment_from(
    mut env: Vec<pc::EnvVar>,
    lookup: impl Fn(&str) -> Option<String>,
) -> Vec<pc::EnvVar> {
    env.retain(|var| !HOST_PASSTHROUGH_ENV.contains(&var.name.as_str()));
    with_host_passthrough(env, lookup)
}

/// Launches an ACP CLI as a local child (`env_clear` + only the projected env, so
/// no ambient leak), piping its stdio into an [`AgentChannel`].
pub struct SubprocessChannelSource {
    launch: AcpLaunch,
    codec: awaken_protocol_acp::Codec,
    secret_broker: Option<Arc<dyn pc::SecretBroker>>,
}

impl SubprocessChannelSource {
    /// A source that launches `launch` and speaks the newline stand-in — the wire
    /// the in-tree fixture agents use. A trusted real CLI over this source sets
    /// [`Self::with_codec`] to `Codec::Acp`.
    #[must_use]
    pub fn new(launch: AcpLaunch) -> Self {
        Self {
            launch,
            codec: awaken_protocol_acp::Codec::Newline,
            secret_broker: None,
        }
    }

    /// Override the wire this source's launched agent speaks.
    #[must_use]
    pub fn with_codec(mut self, codec: awaken_protocol_acp::Codec) -> Self {
        self.codec = codec;
        self
    }

    #[must_use]
    pub fn with_secret_broker(mut self, broker: Arc<dyn pc::SecretBroker>) -> Self {
        self.secret_broker = Some(broker);
        self
    }
}

/// Spawn an ACP CLI child from a resolved [`AcpLaunch`] and pipe its stdio into an
/// [`AgentChannel`]. `env_clear` + only the projected env, so no ambient leak.
async fn spawn(
    launch: &AcpLaunch,
    codec: awaken_protocol_acp::Codec,
    broker: Option<&Arc<dyn pc::SecretBroker>>,
) -> std::result::Result<AgentSession, OpenError> {
    let (program, args) = launch
        .argv
        .split_first()
        .ok_or_else(|| OpenError("empty argv".to_string()))?;
    let mut command = Command::new(program);
    configure_process_group(&mut command);
    command
        .args(args)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    // Projected model/secret env plus the PATH/HOME allowlist, so a Node-backed
    // wrapper and the CLI-owned login resolve — everything else stays cleared.
    let env = match launch.identity {
        AcpLaunchIdentity::Managed => with_local_host_launch_environment(launch.env.clone()),
        AcpLaunchIdentity::BackendOwned => with_backend_owned_host_environment(launch.env.clone()),
    };
    let mut planned = pc::Command::new(launch.argv.clone());
    planned.env = env;
    planned.stdio = pc::Stdio::Piped;
    let materialized = pc::materialize_process_command(&[], planned, broker)
        .await
        .map_err(|error| OpenError(error.to_string()))?;
    for var in &materialized.env {
        command.env(&var.name, var.value.expose());
    }
    let mut child = command
        .spawn()
        .map_err(|e| OpenError(format!("spawn `{program}`: {e}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| OpenError("child has no stdout".to_string()))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| OpenError("child has no stdin".to_string()))?;
    let channel: Box<dyn AgentChannel> = Box::new(SplitChannel::new(stdout, stdin));
    let process: Arc<dyn pc::ProcessHandle> = Arc::new(LocalProcess::spawned(child));
    Ok(AgentSession {
        channel,
        process,
        codec,
        // Unsandboxed local launch runs in the process cwd; the sandboxed source is
        // what pins a stable interior workspace path for cross-directory recovery.
        workspace_cwd: None,
        // Populated by `ProjectingChannelSource::open` for an `AcpSession` CLI.
        mcp_session_servers: Vec::new(),
        session_mode: launch.session_mode.clone(),
        session_config_options: launch.session_config_options.clone(),
        expected_capability: launch.expected_capability.clone(),
    })
}

/// Map one already-mediated endpoint onto the protocol's `session/new` shape.
/// Authentication is always absent: the Runtime Host owns credential realization
/// before this protocol adapter receives the route.
pub(crate) fn to_session_mcp_server(
    cfg: &crate::McpServerConfig,
) -> awaken_protocol_acp::SessionMcpServer {
    use crate::McpTransport;
    let (command, args, url) = match &cfg.transport {
        McpTransport::Stdio { command, args } => (Some(command.clone()), args.clone(), None),
        McpTransport::Http { url } => (None, Vec::new(), Some(url.clone())),
    };
    awaken_protocol_acp::SessionMcpServer {
        name: cfg.name.clone(),
        command,
        args,
        url,
        auth: None,
    }
}

/// The wire a real ACP CLI (`claude --acp`, `codex acp`) speaks: official
/// JSON-RPC when the `real-acp` codec is compiled in, else the newline stand-in.
pub(crate) const CLI_CODEC: awaken_protocol_acp::Codec = {
    #[cfg(feature = "real-acp")]
    {
        awaken_protocol_acp::Codec::Acp
    }
    #[cfg(not(feature = "real-acp"))]
    {
        awaken_protocol_acp::Codec::Newline
    }
};

#[async_trait]
impl AgentChannelSource for SubprocessChannelSource {
    async fn open(
        &self,
        _activation: &RunActivation,
        _context: &awaken_runtime_contract::runtime_context::RuntimeRunContext,
    ) -> std::result::Result<AgentSession, OpenError> {
        spawn(&self.launch, self.codec, self.secret_broker.as_ref()).await
    }
}

/// Resolves the host-provided inputs for one ACP run: the model coordinates (base
/// URL, model name, process-secret requirement — the config-plane + vault lookup the
/// executor must not do itself) and any per-run non-secret env (e.g. the thread's config-home
/// path, which depends on `activation.thread_id` and so cannot be fixed up front).
/// The one seam between the neutral projection and the host's config/secret world.
pub trait LaunchResolver: Send + Sync {
    fn model(
        &self,
        activation: &RunActivation,
        context: &awaken_runtime_contract::runtime_context::RuntimeRunContext,
    ) -> std::result::Result<ResolvedModel, OpenError>;

    /// Host-provided non-secret env for this run (config-home path, passthrough).
    /// Merged under the typed model delivery — it can never shadow the model or key.
    fn extra_env(
        &self,
        _activation: &RunActivation,
    ) -> std::result::Result<Vec<(String, String)>, OpenError> {
        Ok(Vec::new())
    }

    /// Broker paired with the process-secret requirement returned by [`Self::model`].
    /// A projected local source consumes it at spawn; a sandboxed source uses the
    /// same broker installed in its provider.
    fn secret_broker(&self) -> Option<Arc<dyn pc::SecretBroker>> {
        None
    }

    /// Exact credential realization evidence implemented by this resolver and
    /// its paired broker. Admission consumes this before process launch; the
    /// default is empty so a resolver cannot accidentally claim plaintext custody.
    fn credential_realization_capabilities(
        &self,
    ) -> awaken_runtime_contract::CredentialRealizationCapabilities {
        awaken_runtime_contract::CredentialRealizationCapabilities::default()
    }
}

/// An [`AgentChannelSource`] that projects a run onto a launch via its [`AcpCli`]
/// row (R4): it reads the run's inputs through a host [`LaunchResolver`] and hands
/// the data to [`AcpCli::try_project`], so *which* CLI and *how* the model is delivered
/// are data, not a branch.
pub struct ProjectingChannelSource {
    cli: AcpCli,
    resolver: Arc<dyn LaunchResolver>,
}

impl ProjectingChannelSource {
    #[must_use]
    pub fn new(cli: AcpCli, resolver: Arc<dyn LaunchResolver>) -> Self {
        Self { cli, resolver }
    }

    /// Project this run onto a concrete [`AcpLaunch`] (no spawn).
    pub(crate) fn plan(
        &self,
        activation: &RunActivation,
        context: &awaken_runtime_contract::runtime_context::RuntimeRunContext,
    ) -> std::result::Result<AcpLaunch, OpenError> {
        project_launch(&self.cli, self.resolver.as_ref(), activation, context)
    }
}

/// Project a run onto a concrete [`AcpLaunch`] (no spawn): resolve the model + per-run
/// env through `resolver`, read the compaction window from the run's config, and hand
/// all of it to the CLI's [`AcpCli::try_project`] row. The reusable projection core — a
/// sandboxed / containerized ACP source uses it to launch the **per-agent** CLI (the
/// run's `acp:<cli>` backend_ref) inside its isolation, not a fixed argv.
pub fn project_launch(
    cli: &AcpCli,
    resolver: &dyn LaunchResolver,
    activation: &RunActivation,
    context: &awaken_runtime_contract::runtime_context::RuntimeRunContext,
) -> std::result::Result<AcpLaunch, OpenError> {
    let model = resolver.model(activation, context)?;
    let extra_env = resolver.extra_env(activation)?;
    let window = awaken_runtime_contract::resolved::AcpSpec::from_plugin_config(
        &activation.snapshot.resolved_spec.plugin_config,
    )
    .compact_window;
    cli.try_project(&model, window, &extra_env)
}

/// A run's MCP servers projected into host-neutral, environment-agnostic delivery: the
/// `session/new` params to thread in-band (`AcpSession` CLIs) and/or a config file to
/// write (`ConfigFileToml` CLIs, delivered as [`crate::McpDelivery::ConfigFile`]). The
/// ONE seam every [`AgentChannelSource`] uses to deliver MCP, so a sandboxed source
/// delivers the same servers as the unsandboxed one — realizing each part per its
/// environment (ADR-0057 `mcp_injection`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct McpInjection {
    /// Servers passed at `session/new` (in-band over the ACP wire the executor drives).
    pub session_servers: Vec<awaken_protocol_acp::SessionMcpServer>,
    /// A config file to place in the CLI's config home: `(relative path, contents)`.
    pub config_file: Option<(String, String)>,
}

/// Project `plugin_config`'s declared secret-free MCP routes for `cli`.
pub fn mcp_injection(
    cli: &AcpCli,
    plugin_config: &BTreeMap<String, serde_json::Value>,
) -> std::result::Result<McpInjection, OpenError> {
    let servers =
        awaken_runtime_contract::resolved::AcpSpec::from_plugin_config(plugin_config).mcp_servers;
    mcp_injection_from_servers(cli, &servers)
}

/// Project an already-decoded, host-staged MCP set. Session execution uses this
/// typed seam instead of rewriting an immutable Agent publication.
pub fn mcp_injection_from_servers(
    cli: &AcpCli,
    servers: &[crate::McpServerConfig],
) -> std::result::Result<McpInjection, OpenError> {
    if servers.is_empty() {
        return Ok(McpInjection::default());
    }
    Ok(match cli.project_mcp(servers) {
        crate::McpDelivery::SessionServers(list) => McpInjection {
            session_servers: list.iter().map(to_session_mcp_server).collect(),
            config_file: None,
        },
        crate::McpDelivery::ConfigFile { path, contents } => McpInjection {
            session_servers: Vec::new(),
            config_file: Some((path.to_string(), contents)),
        },
    })
}

/// Admit one already-projected MCP delivery against the launch identity. A
/// backend-owned CLI must keep its host config home intact, so only in-band ACP
/// Session delivery is compatible. All channel sources share this one rule.
pub fn admit_mcp_injection(
    identity: AcpLaunchIdentity,
    injection: &McpInjection,
) -> std::result::Result<(), OpenError> {
    if identity == AcpLaunchIdentity::BackendOwned && injection.config_file.is_some() {
        return Err(OpenError(
            "backend-owned local ACP identity requires in-band ACP MCP configuration".into(),
        ));
    }
    Ok(())
}

/// Write a legacy `ConfigFileToml` CLI's MCP config into its isolated config home.
/// The config-home dir is the value the projection put in the launch env
/// under the CLI's `config_home_env`. No-op when the run declares no MCP servers, the CLI
/// uses the `session/new` interface, or no config home was resolved.
fn write_mcp_config(
    cli: &AcpCli,
    injection: &McpInjection,
    launch_env: &[pc::EnvVar],
) -> std::result::Result<(), OpenError> {
    let Some((path, contents)) = &injection.config_file else {
        return Ok(());
    };
    let config_home_env = cli
        .config_home_env
        .ok_or_else(|| OpenError(format!("credential_driver_required: {}", cli.id)))?;
    let Some(dir) = launch_env
        .iter()
        .find(|var| var.name == config_home_env)
        .and_then(|var| match &var.value {
            pc::EnvValue::Inline { value } => Some(value.as_str()),
            pc::EnvValue::Secret { .. } => None,
        })
    else {
        return Err(OpenError("local_config_home_unavailable".to_owned()));
    };
    let full = std::path::Path::new(dir).join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).map_err(|e| OpenError(format!("mcp config dir: {e}")))?;
    }
    std::fs::write(&full, contents).map_err(|e| OpenError(format!("write mcp config: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod mcp_wiring_tests {
    use super::*;
    use crate::acp_cli;

    fn plugin_config_with_mcp() -> std::collections::BTreeMap<String, serde_json::Value> {
        let mut pc = std::collections::BTreeMap::new();
        pc.insert(
            "acp".to_string(),
            serde_json::json!({
                "mcp_servers": [{
                    "name": "github",
                    "transport": { "kind": "stdio", "command": "npx", "args": ["-y", "@mcp/github"] }
                }]
            }),
        );
        pc
    }

    #[test]
    fn mcp_injection_reads_the_config_plane_and_projects_to_the_cli() {
        let pc = plugin_config_with_mcp();
        // A ConfigFileToml driver gets a secret-free config.toml carrying the route.
        let config = mcp_injection(&crate::acp_cli::legacy_config_file_cli(), &pc).unwrap();
        let (path, contents) = config.config_file.expect("config-file projection");
        assert_eq!(path, "config.toml");
        assert!(contents.contains("[mcp_servers.github]"));
        assert!(!contents.to_ascii_lowercase().contains("credential"));
        // claude (AcpSession) → the servers pass through for session/new.
        let session = mcp_injection(acp_cli("claude").unwrap(), &pc).unwrap();
        assert_eq!(session.session_servers.len(), 1);
        assert_eq!(session.session_servers[0].name, "github");
    }

    #[test]
    fn session_mcp_projection_follows_the_transport_decision_table() {
        use crate::{McpServerConfig, McpTransport};

        // Cause-effect graph:
        // C1 transport is Stdio xor HTTP (O constraint)
        //  -> E1 project only that transport's fields
        //  -> E2 auth is always absent.
        //
        // | Rule | C1 | command | URL | auth |
        // |---|---|---|---|---|
        // | P1 | Stdio | exact | none | none |
        // | P2 | HTTP | none | exact | none |
        let rules = [
            McpServerConfig {
                name: "github".into(),
                transport: McpTransport::Stdio {
                    command: "npx".into(),
                    args: vec!["-y".into(), "@mcp/github".into()],
                },
            },
            McpServerConfig {
                name: "open".into(),
                transport: McpTransport::Http {
                    url: "https://mcp.open/sse".into(),
                },
            },
        ];
        let stdio = to_session_mcp_server(&rules[0]);
        assert_eq!(stdio.command.as_deref(), Some("npx"), "P1");
        assert!(stdio.url.is_none(), "P1");
        assert!(stdio.auth.is_none(), "P1");
        let http = to_session_mcp_server(&rules[1]);
        assert!(http.command.is_none(), "P2");
        assert_eq!(http.url.as_deref(), Some("https://mcp.open/sse"), "P2");
        assert!(http.auth.is_none(), "P2");
    }

    #[test]
    fn no_declared_servers_yields_no_delivery() {
        let empty = std::collections::BTreeMap::new();
        assert_eq!(
            mcp_injection(&crate::acp_cli::legacy_config_file_cli(), &empty).unwrap(),
            McpInjection::default()
        );
    }

    #[test]
    fn write_mcp_config_writes_config_toml_into_the_isolated_config_home() {
        let dir = std::env::temp_dir().join(format!("awaken-mcpcfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let env = vec![inline_env("TEST_CONFIG_HOME", &dir.to_string_lossy())];

        write_mcp_config(
            &crate::acp_cli::legacy_config_file_cli(),
            &mcp_injection(
                &crate::acp_cli::legacy_config_file_cli(),
                &plugin_config_with_mcp(),
            )
            .unwrap(),
            &env,
        )
        .unwrap();

        let written = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(written.contains("[mcp_servers.github]"));
        assert!(!written.to_ascii_lowercase().contains("credential"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_mcp_config_writes_only_into_the_isolated_home_never_the_host_default() {
        // A stand-in for the operator's real `~/.codex`: it holds a config the ACP run
        // must never touch. The isolated per-thread home is a *different* directory.
        let root = std::env::temp_dir().join(format!("awaken-noclobber-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let host_default = root.join("host-codex");
        let isolated = root.join("thread-config-home");
        std::fs::create_dir_all(&host_default).unwrap();
        std::fs::create_dir_all(&isolated).unwrap();
        std::fs::write(
            host_default.join("config.toml"),
            b"HOST DEFAULT - DO NOT TOUCH",
        )
        .unwrap();

        // The launch points the CLI's config-home env at the ISOLATED dir (what the host's
        // resolver does), so the MCP config lands there.
        let env = vec![inline_env("TEST_CONFIG_HOME", &isolated.to_string_lossy())];
        write_mcp_config(
            &crate::acp_cli::legacy_config_file_cli(),
            &mcp_injection(
                &crate::acp_cli::legacy_config_file_cli(),
                &plugin_config_with_mcp(),
            )
            .unwrap(),
            &env,
        )
        .unwrap();

        // The isolated home got the injected server; the host default is byte-unchanged.
        assert!(
            std::fs::read_to_string(isolated.join("config.toml"))
                .unwrap()
                .contains("[mcp_servers.github]"),
            "the isolated config home received the MCP config"
        );
        assert_eq!(
            std::fs::read_to_string(host_default.join("config.toml")).unwrap(),
            "HOST DEFAULT - DO NOT TOUCH",
            "the host's default config was never overwritten"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn write_mcp_config_is_a_noop_when_no_isolated_home_is_in_the_launch_env() {
        // Fail-closed: without the CLI's config-home env in the launch (no isolated home
        // resolved), the writer must not fall back to a default / CWD location — it writes
        // NOTHING rather than risk clobbering the host's real config. Sentinel a host
        // default and prove it stays untouched with an empty launch env.
        let root =
            std::env::temp_dir().join(format!("awaken-noclobber-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let host_default = root.join("host-codex");
        std::fs::create_dir_all(&host_default).unwrap();
        std::fs::write(host_default.join("config.toml"), b"HOST DEFAULT").unwrap();

        let error = write_mcp_config(
            &crate::acp_cli::legacy_config_file_cli(),
            &mcp_injection(
                &crate::acp_cli::legacy_config_file_cli(),
                &plugin_config_with_mcp(),
            )
            .unwrap(),
            &[],
        )
        .unwrap_err();
        assert_eq!(error.0, "local_config_home_unavailable");

        assert_eq!(
            std::fs::read_to_string(host_default.join("config.toml")).unwrap(),
            "HOST DEFAULT",
            "with no isolated home, nothing is written — the host default is safe"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn write_mcp_config_is_a_noop_for_a_session_new_cli() {
        // claude delivers at session/new, not a config file → nothing is written.
        let dir = std::env::temp_dir().join(format!("awaken-mcpnoop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let env = vec![inline_env("CLAUDE_CONFIG_DIR", &dir.to_string_lossy())];
        write_mcp_config(
            acp_cli("claude").unwrap(),
            &mcp_injection(acp_cli("claude").unwrap(), &plugin_config_with_mcp()).unwrap(),
            &env,
        )
        .unwrap();
        assert!(!dir.join("config.toml").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[async_trait]
impl AgentChannelSource for ProjectingChannelSource {
    async fn open(
        &self,
        activation: &RunActivation,
        context: &awaken_runtime_contract::runtime_context::RuntimeRunContext,
    ) -> std::result::Result<AgentSession, OpenError> {
        // A real CLI speaks official ACP JSON-RPC.
        let launch = self.plan(activation, context)?;
        // A legacy `ConfigFileToml` CLI gets its MCP servers written into the config
        // home before launch; an `AcpSession` CLI carries them at `session/new` instead.
        let plugin_config = &activation.snapshot.resolved_spec.plugin_config;
        let injection = mcp_injection(&self.cli, plugin_config)?;
        admit_mcp_injection(launch.identity, &injection)?;
        write_mcp_config(&self.cli, &injection, &launch.env)?;
        let broker = self.resolver.secret_broker();
        let mut session = spawn(&launch, CLI_CODEC, broker.as_ref()).await?;
        // An `AcpSession` CLI (claude/gemini/opencode) carries its MCP servers at
        // `session/new`; the driver reads them off the session into the turn config.
        session.mcp_session_servers = injection.session_servers;
        Ok(session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::AcpRunExecutor;
    #[cfg(unix)]
    use awaken_agent_contract::agent::run::{EndCause, RunState};
    #[cfg(unix)]
    use awaken_runtime_contract::execution::RunExecutor;

    // Reuse the fixture activation from the crate tests.
    #[cfg(unix)]
    use crate::tests::activation;

    fn pc(section: serde_json::Value) -> BTreeMap<String, serde_json::Value> {
        BTreeMap::from([("acp".to_string(), section)])
    }

    fn env_value<'a>(env: &'a [pc::EnvVar], key: &str) -> Option<&'a str> {
        env.iter()
            .find(|var| var.name == key)
            .map(|var| match &var.value {
                pc::EnvValue::Inline { value } => value.as_str(),
                pc::EnvValue::Secret { reference } => reference.as_str(),
            })
    }

    // Cause/effect table for the one shared ACP codec:
    // R1 section absent -> default intent;
    // R2 window valid + MCP valid -> preserve both fields;
    // R3 window valid + MCP malformed -> preserve window and reject only MCP.
    // The constraints are field independence and historical fail-soft decoding;
    // these three rules cover every structural branch of `from_plugin_config`.
    #[test]
    fn acp_spec_decodes_compact_window_and_mcp_servers() {
        let s = awaken_runtime_contract::resolved::AcpSpec::from_plugin_config(&pc(
            serde_json::json!({
                "compact_window": 120_000,
                "mcp_servers": [{ "name": "gh", "transport": { "kind": "http", "url": "https://mcp" } }],
            }),
        ));
        assert_eq!(s.compact_window, Some(120_000));
        assert_eq!(s.mcp_servers.len(), 1);
        assert_eq!(s.mcp_servers[0].name, "gh");
    }

    #[test]
    fn acp_spec_absent_section_is_default() {
        assert_eq!(
            awaken_runtime_contract::resolved::AcpSpec::from_plugin_config(&BTreeMap::new()),
            awaken_runtime_contract::resolved::AcpSpec::default()
        );
    }

    #[test]
    fn acp_spec_malformed_mcp_servers_does_not_drop_compact_window() {
        // Fail-soft per field: a bad mcp_servers shape must not lose the window —
        // the exact semantics of the two readers this codec replaces.
        let s = awaken_runtime_contract::resolved::AcpSpec::from_plugin_config(&pc(
            serde_json::json!({
                "compact_window": 4096,
                "mcp_servers": "not-an-array",
            }),
        ));
        assert_eq!(s.compact_window, Some(4096));
        assert!(s.mcp_servers.is_empty());
    }

    fn mcp_pc(servers: serde_json::Value) -> BTreeMap<String, serde_json::Value> {
        BTreeMap::from([(
            "acp".to_string(),
            serde_json::json!({ "mcp_servers": servers }),
        )])
    }

    #[test]
    fn mcp_injection_delivers_session_servers_for_an_acp_session_cli() {
        // claude is an AcpSession CLI → servers ride in-band at session/new, no config file.
        let cli = crate::acp_cli("claude").unwrap();
        let proj = mcp_injection(
            cli,
            &mcp_pc(serde_json::json!([{
                "name": "gh",
                "transport": { "kind": "http", "url": "https://mcp" }
            }])),
        )
        .unwrap();
        assert_eq!(proj.session_servers.len(), 1);
        assert_eq!(proj.session_servers[0].name, "gh");
        assert!(proj.config_file.is_none());
    }

    #[test]
    fn mcp_injection_delivers_a_config_file_for_a_config_file_cli() {
        let cli = crate::acp_cli::legacy_config_file_cli();
        let proj = mcp_injection(
            &cli,
            &mcp_pc(serde_json::json!([{
                "name": "gh",
                "transport": { "kind": "http", "url": "https://mcp" },
                "credential": { "auth": "reference", "reference": "broker://tok" }
            }])),
        )
        .unwrap();
        assert!(proj.session_servers.is_empty());
        let (path, contents) = proj.config_file.expect("config-file CLI gets a file");
        assert!(path.ends_with("config.toml"), "path: {path}");
        assert!(
            contents.contains("[mcp_servers.gh]"),
            "contents: {contents}"
        );
    }

    #[test]
    fn retained_inline_credential_field_is_ignored_by_the_only_projection() {
        // Serde remains tolerant of a retained snapshot field, but the removed
        // credential path cannot project it into ACP.
        let cli = crate::acp_cli("claude").unwrap();
        let pc = mcp_pc(serde_json::json!([{
            "name": "gh",
            "transport": { "kind": "http", "url": "https://mcp" },
            "credential": { "auth": "trusted_inline", "secret": "sk-raw" }
        }]));
        let projection = mcp_injection(cli, &pc).expect("legacy field is ignored");
        assert_eq!(projection.session_servers.len(), 1);
        assert!(projection.session_servers[0].auth.is_none());
        assert!(!format!("{projection:?}").contains("sk-raw"));
    }

    #[test]
    fn mcp_injection_is_empty_when_none_declared() {
        let cli = crate::acp_cli("claude").unwrap();
        let proj = mcp_injection(cli, &BTreeMap::new()).unwrap();
        assert_eq!(proj, McpInjection::default());
    }

    #[test]
    fn mcp_injection_admission_has_one_identity_rule() {
        use crate::{McpServerConfig, McpTransport};
        // Cause graph / decision table: managed accepts either mechanism;
        // backend-owned accepts Session delivery and rejects config-file delivery
        // so its host config/login home is never replaced.
        let config_file = McpInjection {
            config_file: Some(("config.toml".into(), "x".into())),
            session_servers: Vec::new(),
        };
        let session = McpInjection {
            config_file: None,
            session_servers: vec![to_session_mcp_server(&McpServerConfig {
                name: "mcp".into(),
                transport: McpTransport::Http {
                    url: "https://mcp.invalid".into(),
                },
            })],
        };
        assert!(admit_mcp_injection(AcpLaunchIdentity::Managed, &config_file).is_ok());
        assert!(admit_mcp_injection(AcpLaunchIdentity::BackendOwned, &session).is_ok());
        assert!(admit_mcp_injection(AcpLaunchIdentity::BackendOwned, &config_file).is_err());
    }

    #[test]
    fn acp_settings_round_trips_through_json() {
        let s = awaken_runtime_contract::resolved::AcpSpec {
            compact_window: Some(8192),
            mcp_servers: Vec::new(),
        };
        let round: awaken_runtime_contract::resolved::AcpSpec =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(s, round);
    }

    #[test]
    fn host_passthrough_adds_path_and_home_without_shadowing_projected_env() {
        let projected = vec![
            inline_env("ANTHROPIC_API_KEY", "secret"),
            inline_env("HOME", "/projected/home"),
        ];
        let env = with_host_passthrough(projected, |k| match k {
            "PATH" => Some("/usr/local/bin:/usr/bin".to_string()),
            "HOME" => Some("/host/home".to_string()),
            _ => None,
        });
        // PATH added from the host (so npx/node resolve).
        assert!(
            env.iter().any(|var| var.name == "PATH"
                && env_value(&env, "PATH") == Some("/usr/local/bin:/usr/bin"))
        );
        // HOME already set by the projection is NOT overridden by the host value.
        let homes: Vec<&str> = env
            .iter()
            .filter(|var| var.name == "HOME")
            .filter_map(|_| env_value(&env, "HOME"))
            .collect();
        assert_eq!(homes, vec!["/projected/home"], "projected HOME wins");
        // The secret survives untouched.
        assert!(env_value(&env, "ANTHROPIC_API_KEY") == Some("secret"));
    }

    #[test]
    fn host_passthrough_omits_a_key_absent_from_the_host() {
        let env = with_host_passthrough(Vec::new(), |k| (k == "PATH").then(|| "/bin".to_string()));
        assert!(env.iter().any(|var| var.name == "PATH"));
        assert!(
            !env.iter().any(|var| var.name == "HOME"),
            "absent host key omitted"
        );
    }

    #[test]
    fn backend_owned_host_identity_cannot_be_overridden_by_projection() {
        // Cause graph / decision table: a projected PATH/HOME is removed; a host
        // value replaces it. Non-identity env remains untouched.
        let env = with_backend_owned_host_environment_from(
            vec![
                inline_env("PATH", "/projected/bin"),
                inline_env("HOME", "/projected/home"),
                inline_env("LANG", "C"),
            ],
            |key| Some(format!("/host/{key}")),
        );
        assert_eq!(env_value(&env, "PATH"), Some("/host/PATH"));
        assert_eq!(env_value(&env, "HOME"), Some("/host/HOME"));
        assert_eq!(env_value(&env, "LANG"), Some("C"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn spawn_clears_the_host_env_so_a_sentinel_never_reaches_the_child() {
        // G22, the highest-value missing security invariant: `spawn` uses `env_clear`,
        // so an ambient host secret in THIS (parent) process must NOT be inherited by
        // the launched child. Only the PATH/HOME allowlist and the projected launch env
        // cross. Exercises the REAL spawn path (not the ScriptedSource shortcut).
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        // A host secret in the parent env. SAFETY (edition 2024): set on the test thread
        // before the spawn reads the host env; the name is unique to this test and read
        // only by the child's env dump below, then removed.
        unsafe {
            std::env::set_var("AWAKEN_HOST_SENTINEL_LEAK", "leaky-secret-should-not-cross");
        }

        // A shell agent that, on the prompt line, echoes back three probes: the host
        // sentinel (must be EMPTY — cleared), whether PATH is set (must be — allowlisted
        // so `npx`/`node`/binaries resolve), and a projected env value (must arrive).
        let script = "read _prompt; \
             printf 'SENTINEL=[%s] PATH_SET=[%s] PROJECTED=[%s]\\n' \
             \"$AWAKEN_HOST_SENTINEL_LEAK\" \"${PATH:+yes}\" \"$MY_PROJECTED\"";
        let launch = AcpLaunch::custom(
            vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()],
            vec![("MY_PROJECTED".to_string(), "projected-value".to_string())],
        );

        let session = spawn(&launch, awaken_protocol_acp::Codec::Newline, None)
            .await
            .expect("spawn the real child");
        let mut channel = session.channel;
        channel
            .write_all(b"go\n")
            .await
            .expect("send the prompt line");
        channel.flush().await.expect("flush");
        let mut reader = BufReader::new(&mut channel);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .expect("read the child env dump");

        // SAFETY: same-thread teardown, mirroring the set above.
        unsafe {
            std::env::remove_var("AWAKEN_HOST_SENTINEL_LEAK");
        }

        assert!(
            line.contains("SENTINEL=[]"),
            "the host sentinel leaked into the env_clear'd child: {line:?}"
        );
        assert!(
            line.contains("PATH_SET=[yes]"),
            "PATH must pass through the allowlist so the launcher resolves binaries: {line:?}"
        );
        assert!(
            line.contains("PROJECTED=[projected-value]"),
            "the projected launch env must reach the child: {line:?}"
        );
    }

    /// ACP last-mile cause graph:
    ///
    /// typed Secret env -> broker installed -> exact reference resolves -> spawn
    /// -> child receives plaintext. The reference and plaintext remain absent from
    /// launch Debug; without the broker the child is never started.
    ///
    /// | Rule | typed secret | broker | resolves | Result |
    /// |---|---|---|---|---|
    /// | A1 | T | T | T | child sees secret exactly once |
    /// | A2 | T | F | - | fail before spawn |
    #[tokio::test]
    #[cfg(unix)]
    async fn typed_process_secret_reaches_only_the_spawned_acp_process() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::AsyncReadExt;

        struct Broker(AtomicUsize);
        #[async_trait]
        impl pc::SecretBroker for Broker {
            async fn materialize(&self, reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
                assert_eq!(reference, "lease://acp-exact");
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(b"process-only-value".to_vec())
            }

            async fn materialize_process(
                &self,
                reference: &str,
            ) -> Result<Vec<u8>, pc::SandboxError> {
                self.materialize(reference).await
            }

            async fn write_back(
                &self,
                _reference: &str,
                _bytes: Vec<u8>,
            ) -> Result<(), pc::SandboxError> {
                Err(pc::SandboxError::new("not supported"))
            }
        }

        let launch = AcpLaunch {
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf '%s' \"$MODEL_TOKEN\"".into(),
            ],
            env: vec![pc::EnvVar {
                name: "MODEL_TOKEN".into(),
                value: pc::EnvValue::Secret {
                    reference: "lease://acp-exact".into(),
                },
                visibility: pc::EnvVisibility::Process,
            }],
            identity: AcpLaunchIdentity::Managed,
            session_mode: None,
            session_config_options: Vec::new(),
            expected_capability: None,
        };
        let debug = format!("{launch:?}");
        assert!(!debug.contains("lease://acp-exact"));
        assert!(!debug.contains("process-only-value"));

        assert!(
            spawn(&launch, awaken_protocol_acp::Codec::Newline, None)
                .await
                .is_err(),
            "A2 missing broker fails before spawn"
        );

        let broker = Arc::new(Broker(AtomicUsize::new(0)));
        let erased: Arc<dyn pc::SecretBroker> = broker.clone();
        let mut session = spawn(&launch, awaken_protocol_acp::Codec::Newline, Some(&erased))
            .await
            .expect("A1 brokered spawn");
        let mut output = String::new();
        session.channel.read_to_string(&mut output).await.unwrap();
        assert_eq!(output, "process-only-value");
        assert_eq!(broker.0.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn drives_a_real_subprocess_acp_agent_end_to_end() {
        // A tiny ACP agent in shell: read the prompt line, emit a message + turn_end.
        let script = "read _prompt; \
             printf '%s\\n' '{\"type\":\"message\",\"text\":\"from subprocess\"}'; \
             printf '%s\\n' '{\"type\":\"turn_end\",\"reason\":\"natural_end\"}'";
        let launch = AcpLaunch::custom(
            vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()],
            vec![],
        );
        let exec = AcpRunExecutor::new(Arc::new(SubprocessChannelSource::new(launch)));
        let state = exec
            .execute(activation(), Default::default())
            .await
            .unwrap();
        assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    }
}
