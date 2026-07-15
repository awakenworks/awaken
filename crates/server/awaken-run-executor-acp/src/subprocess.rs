//! A subprocess-backed [`AgentChannelSource`]: launch an ACP CLI as a child
//! process and pipe its stdio into an [`AgentChannel`] (R4).
//!
//! This is the *local* (unsandboxed) launch path — the composition root uses it
//! for a trusted CLI or a test agent; a sandbox provider is the isolated
//! production counterpart behind the same [`AgentChannelSource`] trait, so the
//! executor is unchanged either way. Per-CLI config projection is data
//! ([`AcpLaunch`]): the host resolves the model (config plane) and hands this
//! module already-materialized strings — no config or secret types cross here.

use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_channel::{AgentChannel, SplitChannel};
use awaken_provisioning_contract::{ExitStatus, ProcessHandle, SandboxError, Signal};
use awaken_runtime_contract::activation::RunActivation;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::{AcpCli, AgentChannelSource, AgentSession, OpenError, ResolvedModel};

/// A resolved launch for an ACP CLI: the argv plus the env to set (model, base
/// URL, key). Every runtime-specific env-key name lives in a constructor here —
/// the per-CLI projection, kept as data (G12).
#[derive(Debug, Clone)]
pub struct AcpLaunch {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl AcpLaunch {
    /// The Claude Code launch projected from resolved model config (R4): base URL,
    /// model name, and key land in the Anthropic env the adapter reads. Claude Code
    /// has no native ACP flag; it is fronted by `@agentclientprotocol/claude-agent-acp`
    /// via `npx` (see the [`AcpCli`] catalog — this helper mirrors that CLI row).
    /// `api_key` is the already-materialized secret the host injects.
    #[must_use]
    pub fn claude(base_url: &str, model: &str, api_key: &str) -> Self {
        Self {
            argv: vec![
                "npx".to_string(),
                "-y".to_string(),
                "@agentclientprotocol/claude-agent-acp@0.44".to_string(),
            ],
            env: vec![
                ("ANTHROPIC_BASE_URL".to_string(), base_url.to_string()),
                ("ANTHROPIC_MODEL".to_string(), model.to_string()),
                ("ANTHROPIC_API_KEY".to_string(), api_key.to_string()),
            ],
        }
    }

    /// A custom launch (argv + env) — another CLI, or a test ACP agent.
    #[must_use]
    pub fn custom(argv: Vec<String>, env: Vec<(String, String)>) -> Self {
        Self { argv, env }
    }
}

/// Host env keys passed through into the otherwise-cleared child env (the
/// `["PATH","HOME"]` allowlist awaken-next uses): `PATH` so the launcher resolves
/// `npx`/`node`/the CLI binary, `HOME` so `npx` finds its package cache and the CLI
/// its user config. Everything else stays cleared — no ambient leak (G22). A key
/// already set by the projection (a modeled value) is never overridden.
const HOST_PASSTHROUGH_ENV: &[&str] = &["PATH", "HOME"];

/// Add the [`HOST_PASSTHROUGH_ENV`] allowlist to a launch's env, reading each from
/// `lookup` (the host env at spawn). Pure so the allowlist is unit-testable without
/// spawning; a projected key is not shadowed.
fn with_host_passthrough(
    mut env: Vec<(String, String)>,
    lookup: impl Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    for key in HOST_PASSTHROUGH_ENV {
        if !env.iter().any(|(k, _)| k == key)
            && let Some(value) = lookup(key)
        {
            env.push(((*key).to_string(), value));
        }
    }
    env
}

/// Launches an ACP CLI as a local child (`env_clear` + only the projected env, so
/// no ambient leak), piping its stdio into an [`AgentChannel`].
pub struct SubprocessChannelSource {
    launch: AcpLaunch,
    codec: awaken_protocol_acp::Codec,
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
        }
    }

    /// Override the wire this source's launched agent speaks.
    #[must_use]
    pub fn with_codec(mut self, codec: awaken_protocol_acp::Codec) -> Self {
        self.codec = codec;
        self
    }
}

/// Spawn an ACP CLI child from a resolved [`AcpLaunch`] and pipe its stdio into an
/// [`AgentChannel`]. `env_clear` + only the projected env, so no ambient leak.
fn spawn(
    launch: &AcpLaunch,
    codec: awaken_protocol_acp::Codec,
) -> std::result::Result<AgentSession, OpenError> {
    let (program, args) = launch
        .argv
        .split_first()
        .ok_or_else(|| OpenError("empty argv".to_string()))?;
    let mut command = Command::new(program);
    command
        .args(args)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    // Projected model/secret env plus the PATH/HOME allowlist, so `npx`/`node`/the
    // CLI resolve and `npx` finds its cache — everything else stays cleared.
    let env = with_host_passthrough(launch.env.clone(), |k| std::env::var(k).ok());
    for (key, value) in &env {
        command.env(key, value);
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
    let process: Arc<dyn ProcessHandle> = Arc::new(ChildProcess {
        child: Mutex::new(child),
    });
    Ok(AgentSession {
        channel,
        process,
        codec,
        // Unsandboxed local launch runs in the process cwd; the sandboxed source is
        // what pins a stable interior workspace path for cross-directory recovery.
        workspace_cwd: None,
        // Populated by `ProjectingChannelSource::open` for an `AcpSession` CLI.
        mcp_session_servers: Vec::new(),
    })
}

/// Map a neutral [`crate::McpServerConfig`] onto the protocol's `session/new` shape: the
/// α/β credential becomes an `Authorization: Bearer …` header (α: a broker reference the
/// gateway resolves; β: a raw secret on a trusted launch — the trust decision is made
/// upstream by the host's projection). Pure, so the mapping is unit-testable.
fn to_session_mcp_server(cfg: &crate::McpServerConfig) -> awaken_protocol_acp::SessionMcpServer {
    use crate::{McpCredential, McpTransport};
    let (command, args, url) = match &cfg.transport {
        McpTransport::Stdio { command, args } => (Some(command.clone()), args.clone(), None),
        McpTransport::Http { url } => (None, Vec::new(), Some(url.clone())),
    };
    let auth = match &cfg.credential {
        McpCredential::Reference { reference } => {
            Some(("Authorization".to_string(), format!("Bearer {reference}")))
        }
        McpCredential::TrustedInline { secret } => {
            Some(("Authorization".to_string(), format!("Bearer {secret}")))
        }
        McpCredential::None => None,
    };
    awaken_protocol_acp::SessionMcpServer {
        name: cfg.name.clone(),
        command,
        args,
        url,
        auth,
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
    ) -> std::result::Result<AgentSession, OpenError> {
        spawn(&self.launch, self.codec)
    }
}

/// Resolves the host-provided inputs for one ACP run: the model coordinates (base
/// URL, model name, materialized key — the config-plane + vault lookup the executor
/// must not do itself) and any per-run non-secret env (e.g. the thread's config-home
/// path, which depends on `activation.thread_id` and so cannot be fixed up front).
/// The one seam between the neutral projection and the host's config/secret world.
pub trait LaunchResolver: Send + Sync {
    fn model(&self, activation: &RunActivation) -> std::result::Result<ResolvedModel, OpenError>;

    /// Host-provided non-secret env for this run (config-home path, passthrough).
    /// Merged under the typed model delivery — it can never shadow the model or key.
    fn extra_env(&self, _activation: &RunActivation) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// An [`AgentChannelSource`] that projects a run onto a launch via its [`AcpCli`]
/// row (R4): it reads the run's inputs through a host [`LaunchResolver`] and hands
/// the data to [`AcpCli::project`], so *which* CLI and *how* the model is delivered
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

    /// Project this run onto a concrete [`AcpLaunch`] (no spawn): resolve the model +
    /// per-run env, read the compaction window from the run's config, and hand all of
    /// it to the CLI's [`AcpCli::project`] row.
    pub(crate) fn plan(
        &self,
        activation: &RunActivation,
    ) -> std::result::Result<AcpLaunch, OpenError> {
        let model = self.resolver.model(activation)?;
        let extra_env = self.resolver.extra_env(activation);
        let window = compact_window(&activation.snapshot.resolved_spec);
        Ok(self.cli.project(&model, window, &extra_env))
    }
}

/// The launched CLI's own auto-compaction window, from the run's config. Read from
/// the neutral `plugin_config["acp"]["compact_window"]` (a token count) — an
/// ACP-scoped setting, kept separate from the native compactor's message-count
/// [`ContextPolicy`](awaken_runtime_contract::resolved::ContextPolicy) since a CLI's
/// window is tokens, not messages. Absent → the CLI keeps its own default.
fn compact_window(spec: &awaken_runtime_contract::resolved::ResolvedSpec) -> Option<u64> {
    spec.plugin_config
        .get("acp")?
        .get("compact_window")?
        .as_u64()
}

/// The MCP servers a run declares for its ACP CLI, read from the config plane
/// (`plugin_config.acp.mcp_servers`). The host binds these from a session's
/// `mcp_servers`; empty when none declared or the shape is unrecognized.
fn mcp_servers_of(
    plugin_config: &std::collections::BTreeMap<String, serde_json::Value>,
) -> Vec<crate::McpServerConfig> {
    plugin_config
        .get("acp")
        .and_then(|v| v.get("mcp_servers"))
        .and_then(|v| serde_json::from_value::<Vec<crate::McpServerConfig>>(v.clone()).ok())
        .unwrap_or_default()
}

/// Project the run's declared MCP servers onto its CLI's delivery mechanism — a config
/// file for a `ConfigFileToml` CLI (codex), `session/new` params for an `AcpSession`
/// CLI (claude/gemini/opencode). `None` when the run declares none. The host consumes
/// this before/at launch: writing [`crate::McpDelivery::ConfigFile`] into the config
/// home, or threading [`crate::McpDelivery::SessionServers`] into `session/new`. This
/// is the seam that finally carries config-plane MCP servers to the projection core.
pub(crate) fn mcp_delivery(
    cli: &AcpCli,
    plugin_config: &std::collections::BTreeMap<String, serde_json::Value>,
) -> Option<crate::McpDelivery> {
    let servers = mcp_servers_of(plugin_config);
    (!servers.is_empty()).then(|| cli.project_mcp(&servers))
}

/// Write a `ConfigFileToml` CLI's MCP config into its config home before launch (codex
/// `config.toml`). The config-home dir is the value the projection put in the launch env
/// under the CLI's `config_home_env`. No-op when the run declares no MCP servers, the CLI
/// uses the `session/new` interface, or no config home was resolved.
fn write_mcp_config(
    cli: &AcpCli,
    plugin_config: &std::collections::BTreeMap<String, serde_json::Value>,
    launch_env: &[(String, String)],
) -> std::result::Result<(), OpenError> {
    let Some(crate::McpDelivery::ConfigFile { path, contents }) = mcp_delivery(cli, plugin_config)
    else {
        return Ok(());
    };
    let Some(dir) = launch_env
        .iter()
        .find(|(k, _)| k == cli.config_home_env)
        .map(|(_, v)| v.as_str())
    else {
        return Ok(());
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
    use crate::{McpDelivery, acp_cli};

    fn plugin_config_with_mcp() -> std::collections::BTreeMap<String, serde_json::Value> {
        let mut pc = std::collections::BTreeMap::new();
        pc.insert(
            "acp".to_string(),
            serde_json::json!({
                "mcp_servers": [{
                    "name": "github",
                    "transport": { "kind": "stdio", "command": "npx", "args": ["-y", "@mcp/github"] },
                    "credential": { "auth": "reference", "reference": "broker://gh" }
                }]
            }),
        );
        pc
    }

    #[test]
    fn mcp_delivery_reads_the_config_plane_and_projects_to_the_cli() {
        let pc = plugin_config_with_mcp();
        // codex (ConfigFileToml) → a config.toml carrying the server + a secretless ref.
        match mcp_delivery(acp_cli("codex").unwrap(), &pc) {
            Some(McpDelivery::ConfigFile { path, contents }) => {
                assert_eq!(path, "config.toml");
                assert!(contents.contains("[mcp_servers.github]"));
                assert!(contents.contains("broker://gh"));
            }
            other => panic!("codex should deliver a config file, got {other:?}"),
        }
        // claude (AcpSession) → the servers pass through for session/new.
        match mcp_delivery(acp_cli("claude").unwrap(), &pc) {
            Some(McpDelivery::SessionServers(s)) => {
                assert_eq!(s.len(), 1);
                assert_eq!(s[0].name, "github");
            }
            other => panic!("claude should deliver session servers, got {other:?}"),
        }
    }

    #[test]
    fn to_session_mcp_server_maps_transport_and_carries_the_credential_as_a_bearer() {
        use crate::{McpCredential, McpServerConfig, McpTransport};
        // α reference over stdio → command/args pass through, ref becomes a bearer header.
        let alpha = to_session_mcp_server(&McpServerConfig {
            name: "github".into(),
            transport: McpTransport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "@mcp/github".into()],
            },
            credential: McpCredential::Reference {
                reference: "broker://gh".into(),
            },
        });
        assert_eq!(alpha.name, "github");
        assert_eq!(alpha.command.as_deref(), Some("npx"));
        assert_eq!(
            alpha.args,
            vec!["-y".to_string(), "@mcp/github".to_string()]
        );
        assert!(alpha.url.is_none());
        assert_eq!(
            alpha.auth,
            Some(("Authorization".into(), "Bearer broker://gh".into()))
        );

        // β trusted-inline over http → url set, raw secret becomes the bearer.
        let beta = to_session_mcp_server(&McpServerConfig {
            name: "internal".into(),
            transport: McpTransport::Http {
                url: "https://mcp.internal/sse".into(),
            },
            credential: McpCredential::TrustedInline {
                secret: "sk-raw".into(),
            },
        });
        assert!(beta.command.is_none());
        assert_eq!(beta.url.as_deref(), Some("https://mcp.internal/sse"));
        assert_eq!(
            beta.auth,
            Some(("Authorization".into(), "Bearer sk-raw".into()))
        );

        // no credential → no auth header.
        let none = to_session_mcp_server(&McpServerConfig {
            name: "open".into(),
            transport: McpTransport::Http {
                url: "https://mcp.open/sse".into(),
            },
            credential: McpCredential::None,
        });
        assert!(none.auth.is_none());
    }

    #[test]
    fn no_declared_servers_yields_no_delivery() {
        let empty = std::collections::BTreeMap::new();
        assert!(mcp_delivery(acp_cli("codex").unwrap(), &empty).is_none());
    }

    #[test]
    fn write_mcp_config_writes_codex_config_toml_into_the_config_home() {
        let dir = std::env::temp_dir().join(format!("awaken-mcpcfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let env = vec![("CODEX_HOME".to_string(), dir.to_string_lossy().to_string())];

        write_mcp_config(acp_cli("codex").unwrap(), &plugin_config_with_mcp(), &env).unwrap();

        let written = std::fs::read_to_string(dir.join("config.toml")).unwrap();
        assert!(written.contains("[mcp_servers.github]"));
        assert!(written.contains("broker://gh"));
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
        let env = vec![(
            "CODEX_HOME".to_string(),
            isolated.to_string_lossy().to_string(),
        )];
        write_mcp_config(acp_cli("codex").unwrap(), &plugin_config_with_mcp(), &env).unwrap();

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

        // No CODEX_HOME in the launch env → no isolated home.
        write_mcp_config(acp_cli("codex").unwrap(), &plugin_config_with_mcp(), &[]).unwrap();

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
        let env = vec![(
            "CLAUDE_CONFIG_DIR".to_string(),
            dir.to_string_lossy().to_string(),
        )];
        write_mcp_config(acp_cli("claude").unwrap(), &plugin_config_with_mcp(), &env).unwrap();
        assert!(!dir.join("config.toml").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[async_trait]
impl AgentChannelSource for ProjectingChannelSource {
    async fn open(
        &self,
        activation: &RunActivation,
    ) -> std::result::Result<AgentSession, OpenError> {
        // A real CLI (`claude --acp`, `codex acp`) speaks official ACP JSON-RPC.
        let launch = self.plan(activation)?;
        // A `ConfigFileToml` CLI (codex) gets its MCP servers written into the config
        // home before launch; an `AcpSession` CLI carries them at `session/new` instead.
        let plugin_config = &activation.snapshot.resolved_spec.plugin_config;
        write_mcp_config(&self.cli, plugin_config, &launch.env)?;
        let mut session = spawn(&launch, CLI_CODEC)?;
        // An `AcpSession` CLI (claude/gemini/opencode) carries its MCP servers at
        // `session/new`; the driver reads them off the session into the turn config.
        if let Some(crate::McpDelivery::SessionServers(servers)) =
            mcp_delivery(&self.cli, plugin_config)
        {
            session.mcp_session_servers = servers.iter().map(to_session_mcp_server).collect();
        }
        Ok(session)
    }
}

/// A [`ProcessHandle`] over a tokio child. Local best-effort reaping: tokio's
/// `Child` exposes SIGKILL; the SIGTERM→grace→SIGKILL ladder is a sandbox-provider
/// concern (`kill_on_drop` covers the drop path).
struct ChildProcess {
    child: Mutex<Child>,
}

fn exit(status: std::process::ExitStatus) -> ExitStatus {
    ExitStatus {
        code: status.code(),
        signaled: status.code().is_none(),
    }
}

#[async_trait]
impl ProcessHandle for ChildProcess {
    fn id(&self) -> &str {
        "acp-subprocess"
    }
    async fn wait(&self) -> std::result::Result<ExitStatus, SandboxError> {
        let status = self
            .child
            .lock()
            .await
            .wait()
            .await
            .map_err(|e| SandboxError::new(e.to_string()))?;
        Ok(exit(status))
    }
    async fn poll(&self) -> std::result::Result<Option<ExitStatus>, SandboxError> {
        Ok(self
            .child
            .lock()
            .await
            .try_wait()
            .map_err(|e| SandboxError::new(e.to_string()))?
            .map(exit))
    }
    async fn signal(&self, _signal: Signal) -> std::result::Result<(), SandboxError> {
        self.child
            .lock()
            .await
            .start_kill()
            .map_err(|e| SandboxError::new(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AcpRunExecutor;
    use awaken_agent_contract::agent::run::{EndCause, Phase};
    use awaken_runtime_contract::execution::RunExecutor;

    // Reuse the fixture activation from the crate tests.
    use crate::tests::activation;

    #[test]
    fn host_passthrough_adds_path_and_home_without_shadowing_projected_env() {
        let projected = vec![
            ("ANTHROPIC_API_KEY".to_string(), "secret".to_string()),
            ("HOME".to_string(), "/projected/home".to_string()),
        ];
        let env = with_host_passthrough(projected, |k| match k {
            "PATH" => Some("/usr/local/bin:/usr/bin".to_string()),
            "HOME" => Some("/host/home".to_string()),
            _ => None,
        });
        // PATH added from the host (so npx/node resolve).
        assert!(
            env.iter()
                .any(|(k, v)| k == "PATH" && v == "/usr/local/bin:/usr/bin")
        );
        // HOME already set by the projection is NOT overridden by the host value.
        let homes: Vec<&str> = env
            .iter()
            .filter(|(k, _)| k == "HOME")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(homes, vec!["/projected/home"], "projected HOME wins");
        // The secret survives untouched.
        assert!(
            env.iter()
                .any(|(k, v)| k == "ANTHROPIC_API_KEY" && v == "secret")
        );
    }

    #[test]
    fn host_passthrough_omits_a_key_absent_from_the_host() {
        let env = with_host_passthrough(Vec::new(), |k| (k == "PATH").then(|| "/bin".to_string()));
        assert!(env.iter().any(|(k, _)| k == "PATH"));
        assert!(
            !env.iter().any(|(k, _)| k == "HOME"),
            "absent host key omitted"
        );
    }

    #[test]
    fn claude_projection_puts_model_and_base_in_anthropic_env() {
        let l = AcpLaunch::claude("https://gw/v1/", "kimi-k2", "vault-key");
        assert_eq!(
            l.argv,
            vec!["npx", "-y", "@agentclientprotocol/claude-agent-acp@0.44"]
        );
        assert!(
            l.env
                .iter()
                .any(|(k, v)| k == "ANTHROPIC_BASE_URL" && v == "https://gw/v1/")
        );
        assert!(
            l.env
                .iter()
                .any(|(k, v)| k == "ANTHROPIC_MODEL" && v == "kimi-k2")
        );
        assert!(
            l.env
                .iter()
                .any(|(k, v)| k == "ANTHROPIC_API_KEY" && v == "vault-key")
        );
    }

    #[tokio::test]
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
        let phase = exec
            .execute(activation(), Default::default())
            .await
            .unwrap();
        assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    }
}
