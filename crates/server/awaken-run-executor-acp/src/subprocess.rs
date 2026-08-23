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
    pub session_model: Option<String>,
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
            session_model: None,
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
        session_model: launch.session_model.clone(),
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

/// The wire a real native or adapter-backed ACP CLI speaks: official JSON-RPC
/// when the `real-acp` codec is compiled in, else the newline stand-in.
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
#[async_trait]
pub trait LaunchResolver: Send + Sync {
    async fn model(
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

/// Attempt-time materializer for an externally brokered ACP model route.
///
/// The immutable candidate remains secret-free. Implementations exchange its
/// exact run/binding authority for a short-lived model-access grant and return
/// only the launch projection plus an opaque last-mile reference. Provider
/// credentials are never materialized through this port.
#[async_trait]
pub trait BrokeredAcpModelAccessMaterializer: Send + Sync {
    async fn materialize(
        &self,
        cli: AcpCli,
        activation: &RunActivation,
        context: &awaken_runtime_contract::runtime_context::RuntimeRunContext,
    ) -> std::result::Result<ResolvedModel, OpenError>;

    fn secret_broker(&self) -> Arc<dyn pc::SecretBroker>;

    fn credential_realization_capabilities(
        &self,
        cli: AcpCli,
    ) -> awaken_runtime_contract::CredentialRealizationCapabilities;
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
    pub(crate) async fn plan(
        &self,
        activation: &RunActivation,
        context: &awaken_runtime_contract::runtime_context::RuntimeRunContext,
    ) -> std::result::Result<AcpLaunch, OpenError> {
        project_launch(&self.cli, self.resolver.as_ref(), activation, context).await
    }
}

/// Project a run onto a concrete [`AcpLaunch`] (no spawn): resolve the model + per-run
/// env through `resolver`, read the compaction window from the run's config, and hand
/// all of it to the CLI's [`AcpCli::try_project`] row. The reusable projection core — a
/// sandboxed / containerized ACP source uses it to launch the **per-agent** CLI (the
/// run's `acp:<cli>` backend_ref) inside its isolation, not a fixed argv.
pub async fn project_launch(
    cli: &AcpCli,
    resolver: &dyn LaunchResolver,
    activation: &RunActivation,
    context: &awaken_runtime_contract::runtime_context::RuntimeRunContext,
) -> std::result::Result<AcpLaunch, OpenError> {
    let model = resolver.model(activation, context).await?;
    let extra_env = resolver.extra_env(activation)?;
    let window = awaken_runtime_contract::resolved::AcpSpec::from_plugin_config(
        &activation.snapshot.resolved_spec.plugin_config,
    )
    .compact_window;
    cli.try_project(&model, window, &extra_env)
}

/// Project `plugin_config`'s declared secret-free MCP routes onto the one ACP
/// delivery authority: the typed `session/new` server list.
#[must_use]
fn mcp_session_servers_from_plugin_config(
    plugin_config: &BTreeMap<String, serde_json::Value>,
) -> Vec<awaken_protocol_acp::SessionMcpServer> {
    let servers =
        awaken_runtime_contract::resolved::AcpSpec::from_plugin_config(plugin_config).mcp_servers;
    mcp_session_servers_from_routes(&servers)
}

/// Project an already-decoded, host-staged route set onto official ACP Session
/// server values. Session execution uses this seam instead of rewriting an
/// immutable Agent publication.
#[must_use]
pub fn mcp_session_servers_from_routes(
    servers: &[crate::McpServerConfig],
) -> Vec<awaken_protocol_acp::SessionMcpServer> {
    servers.iter().map(to_session_mcp_server).collect()
}

/// Admit an already-realized, process-local MCP Session projection for one exact
/// ACP adapter. Credential material may remain only in the typed in-band HTTP
/// form whose capability is declared by the selected catalog row.
pub fn admit_mcp_session_servers(
    cli: &AcpCli,
    servers: &[awaken_protocol_acp::SessionMcpServer],
) -> std::result::Result<Vec<awaken_protocol_acp::SessionMcpServer>, OpenError> {
    for server in servers.iter().filter(|server| server.auth.is_some()) {
        let admitted = cli.admits_mcp_client_credential(
            Some(awaken_credential_contract::McpCredentialDelivery::ClientInjection),
            server.url.is_some() && server.command.is_none() && server.args.is_empty(),
        );
        if !admitted {
            return Err(OpenError(format!(
                "mcp_client_injection_unsupported: {}",
                cli.id
            )));
        }
    }
    Ok(servers.to_vec())
}

#[cfg(test)]
mod mcp_wiring_tests {
    use super::*;

    fn plugin_config_with_mcp() -> std::collections::BTreeMap<String, serde_json::Value> {
        let mut pc = std::collections::BTreeMap::new();
        pc.insert(
            "acp".to_string(),
            serde_json::json!({
                "mcp_servers": [{
                    "name": "github",
                    "transport": { "kind": "stdio", "command": "npx", "args": ["-y", "@mcp/github"] },
                    "credential": { "auth": "trusted_inline", "secret": "sk-retained" } // awaken-allow: secret
                }]
            }),
        );
        pc
    }

    #[test]
    fn config_plane_projects_only_secret_free_session_servers() {
        // Causes: C1 plugin_config contains one valid MCP route; C2 a retained
        // legacy credential field is also present. Effects: E1 exactly one typed
        // Session server preserves route identity/transport; E2 no auth or raw
        // secret enters the projection. Constraint: `session/new` is the only
        // delivery authority and the tolerant decoder may ignore, never revive,
        // retired credential fields. Decision rule R1=C1+C2=>E1+E2.
        let pc = plugin_config_with_mcp();
        let projected = mcp_session_servers_from_plugin_config(&pc);
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].name, "github");
        assert_eq!(projected[0].command.as_deref(), Some("npx"));
        assert!(projected[0].url.is_none());
        assert!(projected[0].auth.is_none());
        assert!(!format!("{projected:?}").contains("sk-retained"));
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
    fn process_private_mcp_auth_requires_an_explicit_http_session_adapter() {
        // Causes: C1 server has process-private auth; C2 selected row declares
        // client injection; C3 transport is exact HTTP (URL, no command/args).
        // Effects: E1 preserve the typed server or E2 reject before process
        // spawn. Constraint: no config-file or implicit adapter-id fallback.
        // Decision rules: R1=C1+C2+C3=>E1; R2=C1+!C2+C3=>E2;
        // R3=C1+C2+!C3 (stdio or HTTP-with-args)=>E2.
        let authenticated = awaken_protocol_acp::SessionMcpServer {
            name: "github".into(),
            command: None,
            args: Vec::new(),
            url: Some("https://mcp.example".into()),
            auth: Some(("Authorization".into(), "Bearer raw-secret".into())),
        };
        let admitted = admit_mcp_session_servers(
            crate::acp_cli("claude").unwrap(),
            std::slice::from_ref(&authenticated),
        )
        .expect("declared ACP Session auth adapter");
        assert_eq!(admitted.as_slice(), std::slice::from_ref(&authenticated));

        let unsupported = *crate::acp_cli("codex").expect("codex fixture");
        assert!(
            admit_mcp_session_servers(&unsupported, std::slice::from_ref(&authenticated)).is_err()
        );

        let mut stdio = authenticated.clone();
        stdio.url = None;
        stdio.command = Some("mcp-server".into());
        assert!(admit_mcp_session_servers(crate::acp_cli("claude").unwrap(), &[stdio]).is_err());

        let mut http_with_args = authenticated;
        http_with_args.args.push("unexpected".into());
        assert!(
            admit_mcp_session_servers(crate::acp_cli("claude").unwrap(), &[http_with_args],)
                .is_err()
        );
    }

    #[test]
    fn no_declared_servers_yields_no_delivery() {
        // Cause C1: plugin_config declares no MCP servers. Effect E1: the
        // canonical Session list is empty. Constraint: absence never creates a
        // config artifact. Decision rule R1=!C1=>E1.
        let empty = std::collections::BTreeMap::new();
        assert!(mcp_session_servers_from_plugin_config(&empty).is_empty());
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
        let launch = self.plan(activation, context).await?;
        let plugin_config = &activation.snapshot.resolved_spec.plugin_config;
        let mcp_session_servers = mcp_session_servers_from_plugin_config(plugin_config);
        let broker = self.resolver.secret_broker();
        let mut session = spawn(&launch, CLI_CODEC, broker.as_ref()).await?;
        // Every production adapter carries MCP through official `session/new`;
        // the driver reads this typed list from the Session into the Run config.
        session.mcp_session_servers = mcp_session_servers;
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
    use awaken_runtime_contract::{RuntimeRunContext, execution::RunExecutor};

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
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Select one ambient variable that really exists but is not in the
        // backend allow-list. The test never mutates process-global state and
        // never logs its value.
        let ambient_key = std::env::vars()
            .map(|(key, _)| key)
            .find(|key| {
                key != "PATH"
                    && key != "HOME"
                    && key != "MY_PROJECTED"
                    && key
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            })
            .expect("test process has a non-allowlisted ambient variable");

        // Dump only the child's final environment after receiving one prompt.
        // It may contain the backend allow-list and projected values, but no
        // arbitrary variable selected above.
        let script = "read _prompt; env";
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
        let mut output = String::new();
        channel
            .read_to_string(&mut output)
            .await
            .expect("read the child env dump");

        assert!(
            !output
                .lines()
                .any(|line| line.starts_with(&format!("{ambient_key}="))),
            "ambient variable {ambient_key} leaked into the env_clear'd child"
        );
        assert!(
            output.lines().any(|line| line.starts_with("PATH=")),
            "PATH must pass through the allowlist so the launcher resolves binaries"
        );
        assert!(
            output
                .lines()
                .any(|line| line == "MY_PROJECTED=projected-value"),
            "the projected launch env must reach the child"
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
            session_model: None,
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
        use awaken_agent_contract::thread::commit::coordinator::{
            Coordinator, Error as CommitError,
        };
        use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};

        struct Commit;

        #[async_trait]
        impl Coordinator for Commit {
            async fn commit(&self, _commit: ThreadCommit) -> Result<CommitRecord, CommitError> {
                Ok(CommitRecord { sequence: 1 })
            }
        }

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
            .execute(
                activation(),
                RuntimeRunContext::new().with_commit(Arc::new(Commit)),
            )
            .await
            .unwrap();
        assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    }
}
