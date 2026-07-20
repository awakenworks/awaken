//! The **ACP CLI catalog**: one data row per external coding-agent CLI (Claude
//! Code, Codex, Gemini…) describing how to launch it and how the resolved model
//! reaches it. Pure data + a projection function — no `match adapter_kind` anywhere
//! (adding a CLI is a row, not a branch). A row is a *catalog binding* an agent
//! references by id (`Backend::Acp { cli }`), the same category as a model provider
//! — never agent config itself.

use crate::AcpLaunch;

/// How resolved model coordinates reach a CLI. Endpoints and secrets use env keys;
/// model selection may additionally use the CLI's generic config override (Codex
/// consumes `-c model=...`). Both remain catalog data rather than adapter branches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelDelivery {
    /// Env key for the endpoint base URL (e.g. `ANTHROPIC_BASE_URL`).
    pub base_url: &'static str,
    /// Env key for the model name (e.g. `ANTHROPIC_MODEL`).
    pub model: &'static str,
    /// Optional Codex-style config key for CLIs that do not consume their model
    /// selection from the ordinary model environment variable.
    pub model_config_key: Option<&'static str>,
    /// Optional JSON environment variable carrying the config object. When paired
    /// with `model_config_key`, the projection merges the resolved model into the
    /// row's static JSON config. `None` retains the legacy `-c key=value` delivery.
    pub model_config_env: Option<&'static str>,
    /// Env key for the API key (a secret — the host materializes it; never stored).
    pub key: &'static str,
    /// Extra model-name env keys the CLI reads as tier aliases, all set to the same
    /// resolved model (e.g. `ANTHROPIC_SONNET_MODEL`/`OPUS`/`HAIKU`).
    pub aliases: &'static [&'static str],
}

/// How a CLI keys its persisted sessions — decides whether cross-directory
/// recovery needs a stable interior working directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKey {
    /// Keyed by the working directory (e.g. Claude Code's `projects/<cwd-slug>/`):
    /// recovery needs the same interior cwd every relaunch.
    Cwd,
    /// Keyed by an internal session id stored in the session data itself
    /// (e.g. Codex rollout files): cwd-independent.
    InternalId,
}

/// Where a CLI keeps its durable session — the axis that decides how (and whether)
/// we recover it across directories and machines. Three classes: a local config
/// dir we harvest/restore as a portable resource, a server-side gateway that owns
/// the session, or none (recovery is the neutral thread history only). Data-driven
/// per row — no `match adapter_kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPersistence {
    /// Session state lives in the CLI's local config home. `session_subpath` is the
    /// portable subtree to harvest (the conversation store); credentials/local
    /// config to exclude are the row's [`AcpCli::retained_paths`].
    LocalDir {
        session_subpath: &'static str,
        keyed_by: SessionKey,
    },
    /// Session state lives server-side (a cloud gateway owns it) — no local harvest;
    /// `session/load` resumes by id against the gateway, so it is cross-machine
    /// already.
    Gateway,
    /// No native session persistence; recovery is the neutral thread history only.
    None,
}

/// How a CLI receives its MCP servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpInterface {
    /// Passed at `session/new` (the ACP `mcpServers` param).
    AcpSession,
    /// Written into a config file inside the config home (e.g. codex `config.toml`).
    ConfigFileToml { path: &'static str },
}

/// The definition of one external ACP CLI: how to launch it and project config onto
/// it. Referenced by `Backend::Acp { cli }` via [`AcpCli::id`].
#[derive(Debug, Clone, Copy)]
pub struct AcpCli {
    pub id: &'static str,
    /// Host/local argv. This may use `npx` for on-demand developer installation.
    pub command: &'static str,
    pub args: &'static [&'static str],
    /// Equivalent argv for a worker image where the adapter is preinstalled. Keeping
    /// this in the catalog row avoids both runtime package downloads and adapter
    /// branches in the container mechanism.
    pub container_argv: &'static [&'static str],
    pub model_delivery: ModelDelivery,
    pub mcp_interface: McpInterface,
    /// Env key naming the CLI's isolated config directory (e.g. `CLAUDE_CONFIG_DIR`).
    pub config_home_env: &'static str,
    /// Native credential file relative to the config home. The host may project an
    /// opaque credential-broker reference to this path as a durable writable Secret;
    /// the CLI owns its JSON format and token refresh behavior.
    pub credential_file: Option<&'static str>,
    /// The memory file the CLI reads from its config home (e.g. `CLAUDE.md`).
    pub memory_entrypoint: &'static str,
    /// Paths under the config home that survive across sessions (auth, config).
    /// Also the exclusion set when harvesting the portable session-home — these are
    /// credential/local-config, never carried into a cross-machine session blob.
    pub retained_paths: &'static [&'static str],
    /// Where this CLI keeps its durable session, deciding cross-directory /
    /// cross-machine recovery (see [`SessionPersistence`]).
    pub session_persistence: SessionPersistence,
    /// Env key for the CLI's own auto-compaction window, if it exposes one.
    pub context_window_env: Option<&'static str>,
    /// Static non-secret env defaults for this CLI (lowest precedence).
    pub env: &'static [(&'static str, &'static str)],
}

/// The host-resolved model coordinates handed to the projection: base URL and model
/// come from the resolved spec + model catalog; `api_key` is materialized from the
/// vault (a secret — it enters only the launched process env, never the catalog).
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

impl ResolvedModel {
    /// Cloud-managed egress (D-R2, ADR-0021 §9/R2). An ACP CLI runs inside the
    /// untrusted sandbox, so it must never hold a raw provider key: its egress is
    /// mediated by the gateway. `base_url` is the **gateway**, and the CLI's "API
    /// key" is a short-lived **lease token** — the gateway injects the real provider
    /// credential out of the sandbox's address space. Build the ACP model this way
    /// from a resolved cloud-managed gateway (base URL + lease) so the raw key never
    /// enters the launch env.
    #[must_use]
    pub fn cloud_managed_gateway(
        gateway_base_url: impl Into<String>,
        model: impl Into<String>,
        lease_token: impl Into<String>,
    ) -> Self {
        Self {
            base_url: gateway_base_url.into(),
            model: model.into(),
            api_key: lease_token.into(),
        }
    }
}

impl AcpCli {
    /// Project the resolved model + optional compaction window + per-agent env
    /// overrides onto a concrete [`AcpLaunch`]. Precedence (low→high):
    /// static defaults < per-agent passthrough < typed model delivery < the secret
    /// API key (host-controlled, never overridable by passthrough). So a modeled
    /// fact (model/base_url) always wins over a stray passthrough key, and the key
    /// is always the host's.
    #[must_use]
    pub fn project(
        &self,
        model: &ResolvedModel,
        context_window: Option<u64>,
        extra_env: &[(String, String)],
    ) -> AcpLaunch {
        use std::collections::BTreeMap;
        let mut env: BTreeMap<String, String> = BTreeMap::new();
        for (k, v) in self.env {
            env.insert((*k).to_string(), (*v).to_string());
        }
        for (k, v) in extra_env {
            env.insert(k.clone(), v.clone());
        }
        let d = &self.model_delivery;
        if !model.base_url.is_empty() {
            env.insert(d.base_url.to_string(), model.base_url.clone());
        }
        if !model.model.is_empty() {
            env.insert(d.model.to_string(), model.model.clone());
            for alias in d.aliases {
                env.insert((*alias).to_string(), model.model.clone());
            }
        }
        if let (Some(key), Some(window)) = (self.context_window_env, context_window) {
            env.insert(key.to_string(), window.to_string());
        }
        // The secret goes last so no passthrough key can shadow it.
        if !model.api_key.is_empty() {
            env.insert(d.key.to_string(), model.api_key.clone());
        }

        let mut argv = vec![self.command.to_string()];
        argv.extend(self.args.iter().map(|s| (*s).to_string()));
        if let Some(key) = d.model_config_key
            && !model.model.is_empty()
        {
            if let Some(config_env) = d.model_config_env {
                let mut config = env
                    .get(config_env)
                    .and_then(|value| {
                        serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(value)
                            .ok()
                    })
                    .unwrap_or_default();
                config.insert(
                    key.to_string(),
                    serde_json::Value::String(model.model.clone()),
                );
                env.insert(
                    config_env.to_string(),
                    serde_json::Value::Object(config).to_string(),
                );
            } else {
                argv.push("-c".to_string());
                argv.push(format!("{key}={:?}", model.model));
            }
        }
        AcpLaunch {
            argv,
            env: env.into_iter().collect(),
        }
    }
}

/// A neutral MCP server a run wants an ACP CLI to reach. Serializable so it rides the
/// config plane into `ResolvedSpec.plugin_config` (the seam `mcp_servers_of` reads back).
/// The `credential` models the trust boundary explicitly (α vs β) — see [`McpCredential`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransport,
    #[serde(default, skip_serializing_if = "McpCredential::is_none")]
    pub credential: McpCredential,
}

impl McpServerConfig {
    /// Safe to hand to a **sandboxed** CLI: it carries no raw secret (α or none). A
    /// `TrustedInline` (β) credential is rejected — a raw secret must never enter a
    /// sandboxed delivery (G3/D-R2). The host asserts this before a sandboxed launch.
    #[must_use]
    pub fn is_sandbox_safe(&self) -> bool {
        !matches!(self.credential, McpCredential::TrustedInline { .. })
    }
}

/// The server's auth, modeling the trust boundary of *how* the ACP CLI gets it:
/// - **α — [`Reference`](McpCredential::Reference)**: secretless (G3/D-R2). A
///   broker/gateway reference the host resolves out of the sandbox's address space —
///   the same rule the model key follows via [`ResolvedModel::cloud_managed_gateway`].
///   The only credential form valid for a **sandboxed** CLI.
/// - **β — [`TrustedInline`](McpCredential::TrustedInline)**: a raw secret, valid ONLY
///   on a **non-sandboxed trusted** launch (a local trusted CLI). Never emitted into a
///   sandboxed delivery — [`McpServerConfig::is_sandbox_safe`] fails closed on it.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "auth", rename_all = "snake_case")]
pub enum McpCredential {
    /// No credential — an unauthenticated server.
    #[default]
    None,
    /// α: a secretless broker/gateway reference.
    Reference { reference: String },
    /// β: a raw secret, trusted-launch-only.
    TrustedInline { secret: String },
}

impl McpCredential {
    #[must_use]
    fn is_none(&self) -> bool {
        matches!(self, McpCredential::None)
    }
}

/// How an MCP server is reached — a stdio child or an HTTP endpoint.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpTransport {
    Stdio { command: String, args: Vec<String> },
    Http { url: String },
}

/// How the projected MCP servers are handed to a launched CLI — the realization of the
/// row's [`McpInterface`]. A `ConfigFileToml` CLI (codex) gets a file to write into its
/// config home before launch; an `AcpSession` CLI (claude/gemini/opencode) gets the
/// servers to pass at `session/new`. Data, not a `match adapter_kind`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpDelivery {
    /// Write `contents` to `<config_home>/<path>` before launch.
    ConfigFile {
        path: &'static str,
        contents: String,
    },
    /// Pass these servers in the ACP `session/new` `mcpServers` param.
    SessionServers(Vec<McpServerConfig>),
}

/// Render an MCP server set as a codex `config.toml` fragment (`[mcp_servers.<name>]`).
/// Only the transport and a credential *reference* are written — never a secret.
fn render_mcp_config_toml(servers: &[McpServerConfig]) -> String {
    let mut out = String::new();
    for s in servers {
        out.push_str(&format!("[mcp_servers.{}]\n", s.name));
        match &s.transport {
            McpTransport::Stdio { command, args } => {
                out.push_str(&format!("command = {command:?}\n"));
                let rendered: Vec<String> = args.iter().map(|a| format!("{a:?}")).collect();
                out.push_str(&format!("args = [{}]\n", rendered.join(", ")));
            }
            McpTransport::Http { url } => {
                out.push_str(&format!("url = {url:?}\n"));
            }
        }
        match &s.credential {
            // α: a broker reference the host resolves out-of-band; never the bytes.
            McpCredential::Reference { reference } => {
                out.push_str(&format!("credential_ref = {reference:?}\n"));
            }
            // β: a raw secret — only ever reached on a trusted (non-sandboxed) launch;
            // a sandboxed delivery is refused upstream by `is_sandbox_safe`.
            McpCredential::TrustedInline { secret } => {
                out.push_str(&format!("credential = {secret:?}\n"));
            }
            McpCredential::None => {}
        }
        out.push('\n');
    }
    out
}

impl AcpCli {
    /// Project the MCP servers a run needs onto this CLI's delivery mechanism, branching
    /// on the row's [`McpInterface`] — not on an adapter kind. This is what finally
    /// consumes `mcp_interface`: the host writes the [`McpDelivery::ConfigFile`] before
    /// launch, or threads [`McpDelivery::SessionServers`] into `session/new`.
    #[must_use]
    pub fn project_mcp(&self, servers: &[McpServerConfig]) -> McpDelivery {
        match self.mcp_interface {
            McpInterface::ConfigFileToml { path } => McpDelivery::ConfigFile {
                path,
                contents: render_mcp_config_toml(servers),
            },
            McpInterface::AcpSession => McpDelivery::SessionServers(servers.to_vec()),
        }
    }
}

// Claude Code does NOT speak ACP natively (there is no `claude --acp`). It is
// fronted by the official adapter package `@agentclientprotocol/claude-agent-acp`,
// launched via `npx`. The version is pinned to a MAJOR.MINOR (never `@latest`,
// mirroring oversight-next's `is_floating_version` guardrail) so a launch is
// reproducible and the wire codec stays a known quantity.
const CLAUDE: AcpCli = AcpCli {
    id: "claude",
    command: "npx",
    args: &["-y", "@agentclientprotocol/claude-agent-acp@0.44"],
    container_argv: &["claude-agent-acp"],
    model_delivery: ModelDelivery {
        base_url: "ANTHROPIC_BASE_URL",
        model: "ANTHROPIC_MODEL",
        model_config_key: None,
        model_config_env: None,
        key: "ANTHROPIC_API_KEY",
        aliases: &[
            "ANTHROPIC_SONNET_MODEL",
            "ANTHROPIC_OPUS_MODEL",
            "ANTHROPIC_HAIKU_MODEL",
        ],
    },
    mcp_interface: McpInterface::AcpSession,
    config_home_env: "CLAUDE_CONFIG_DIR",
    credential_file: Some(".credentials.json"),
    memory_entrypoint: "CLAUDE.md",
    retained_paths: &[".credentials.json", "settings.json"],
    // Claude Code stores conversations under `projects/<cwd-slug>/`, keyed by cwd.
    session_persistence: SessionPersistence::LocalDir {
        session_subpath: "projects",
        keyed_by: SessionKey::Cwd,
    },
    context_window_env: Some("CLAUDE_CODE_AUTO_COMPACT_WINDOW"),
    env: &[],
};

// Codex is likewise fronted by the official adapter package
// (`@agentclientprotocol/codex-acp`),
// not a native `codex acp` subcommand. `CODEX_CONFIG` disables Codex's own approval
// prompts and sets it to workspace-write — our layer owns the gate and the jail, so
// the CLI must not block on its own confirmations.
const CODEX: AcpCli = AcpCli {
    id: "codex",
    command: "npx",
    args: &["-y", "@agentclientprotocol/codex-acp@1.1"],
    container_argv: &["codex-acp"],
    model_delivery: ModelDelivery {
        base_url: "OPENAI_BASE_URL",
        model: "OPENAI_MODEL",
        model_config_key: Some("model"),
        model_config_env: Some("CODEX_CONFIG"),
        key: "OPENAI_API_KEY",
        aliases: &[],
    },
    mcp_interface: McpInterface::ConfigFileToml {
        path: "config.toml",
    },
    config_home_env: "CODEX_HOME",
    credential_file: Some("auth.json"),
    memory_entrypoint: "AGENTS.md",
    retained_paths: &["auth.json", "config.toml"],
    // Codex writes rollout files under `sessions/`, keyed by an internal id.
    session_persistence: SessionPersistence::LocalDir {
        session_subpath: "sessions",
        keyed_by: SessionKey::InternalId,
    },
    context_window_env: None,
    env: &[(
        "CODEX_CONFIG",
        r#"{"approval_policy":"never","sandbox_mode":"workspace-write"}"#,
    )],
};

// Gemini CLI speaks ACP natively via `--experimental-acp` (no npm wrapper), so it
// is a Direct launch with no dynamic-install step.
const GEMINI: AcpCli = AcpCli {
    id: "gemini",
    command: "gemini",
    args: &["--experimental-acp"],
    container_argv: &["gemini", "--experimental-acp"],
    model_delivery: ModelDelivery {
        base_url: "GOOGLE_GEMINI_BASE_URL",
        model: "GEMINI_MODEL",
        model_config_key: None,
        model_config_env: None,
        key: "GEMINI_API_KEY",
        aliases: &[],
    },
    mcp_interface: McpInterface::AcpSession,
    config_home_env: "GEMINI_DIR",
    credential_file: None,
    memory_entrypoint: "GEMINI.md",
    retained_paths: &[],
    // Gemini keeps chat state under `tmp/<hash>/`, keyed by an internal id
    // (provisional — confirm the exact subtree by capability probe).
    session_persistence: SessionPersistence::LocalDir {
        session_subpath: "tmp",
        keyed_by: SessionKey::InternalId,
    },
    context_window_env: None,
    env: &[],
};

// opencode (sst/opencode) is a native, provider-agnostic coding agent that exposes an
// ACP server. Modeled as a Direct launch (no npm wrapper) reading an OpenAI-compatible
// endpoint — the most common opencode provider shape. The exact ACP invocation flag and
// the session subtree are PROVISIONAL (confirm by capability probe, same discipline as
// the Gemini row) — the projection/session/egress contract below is exercised regardless.
const OPENCODE: AcpCli = AcpCli {
    id: "opencode",
    command: "opencode",
    args: &["acp"],
    container_argv: &["opencode", "acp"],
    model_delivery: ModelDelivery {
        base_url: "OPENAI_BASE_URL",
        model: "OPENAI_MODEL",
        model_config_key: None,
        model_config_env: None,
        key: "OPENAI_API_KEY",
        aliases: &[],
    },
    mcp_interface: McpInterface::AcpSession,
    config_home_env: "OPENCODE_CONFIG_DIR",
    credential_file: Some("auth.json"),
    memory_entrypoint: "AGENTS.md",
    retained_paths: &["auth.json"],
    // opencode keeps conversation state in a local store, keyed by an internal id
    // (provisional subtree — confirm the exact path by capability probe).
    session_persistence: SessionPersistence::LocalDir {
        session_subpath: "storage",
        keyed_by: SessionKey::InternalId,
    },
    context_window_env: None,
    env: &[],
};

/// Whether a launch command dynamically installs its agent on first run (an `npx`
/// wrapper pulls the pinned package into the npm cache), so a caller can surface an
/// "installing…" phase before the process is usable. A native CLI (Gemini) is not.
#[must_use]
pub fn is_dynamic_install(cli: &AcpCli) -> bool {
    cli.command == "npx"
}

/// The known ACP CLIs. Adding one is a row here — never a branch elsewhere.
#[must_use]
pub fn known_acp_clis() -> &'static [AcpCli] {
    &[CLAUDE, CODEX, GEMINI, OPENCODE]
}

/// Resolve an ACP CLI by id (`Backend::Acp { cli }`); `None` is a fail-closed
/// "unknown CLI" the caller rejects (never a silent default).
#[must_use]
pub fn acp_cli(id: &str) -> Option<&'static AcpCli> {
    known_acp_clis().iter().find(|c| c.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved() -> ResolvedModel {
        ResolvedModel {
            base_url: "https://api.minimaxi.com/anthropic".to_string(),
            model: "MiniMax-M3[1m]".to_string(),
            api_key: "test-materialized-key".to_string(), // awaken-allow: secret
        }
    }

    fn env_of(launch: &AcpLaunch, key: &str) -> Option<String> {
        launch
            .env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }

    #[test]
    fn registry_holds_the_known_clis_and_unknown_fails_closed() {
        assert!(acp_cli("claude").is_some());
        assert!(acp_cli("codex").is_some());
        assert!(acp_cli("gemini").is_some());
        assert!(acp_cli("opencode").is_some());
        assert!(acp_cli("no_such_cli").is_none());
    }

    // ── Property tests over EVERY catalog row ────────────────────────────────────
    // These hold for every current and future CLI, so adding a row (opencode, …) is
    // covered by construction — the invariants a new agent must satisfy, not a
    // per-agent copy of the same assertions.

    #[test]
    fn every_cli_has_a_unique_id_and_a_nonempty_launch() {
        let mut ids: Vec<&str> = known_acp_clis().iter().map(|c| c.id).collect();
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n, "catalog has a duplicate CLI id");
        for cli in known_acp_clis() {
            assert!(!cli.command.is_empty(), "{}: command is set", cli.id);
            assert!(
                !cli.config_home_env.is_empty(),
                "{}: config_home_env is set",
                cli.id
            );
        }
    }

    #[test]
    fn every_cli_injects_the_resolved_model_base_url_and_secret_key() {
        let m = resolved();
        for cli in known_acp_clis() {
            let launch = cli.project(&m, None, &[]);
            let d = &cli.model_delivery;
            assert_eq!(
                env_of(&launch, d.base_url).as_deref(),
                Some(m.base_url.as_str()),
                "{}: base_url",
                cli.id
            );
            assert_eq!(
                env_of(&launch, d.model).as_deref(),
                Some(m.model.as_str()),
                "{}: model",
                cli.id
            );
            assert_eq!(
                env_of(&launch, d.key).as_deref(),
                Some(m.api_key.as_str()),
                "{}: key",
                cli.id
            );
            for alias in d.aliases {
                assert_eq!(
                    env_of(&launch, alias).as_deref(),
                    Some(m.model.as_str()),
                    "{}: alias {alias}",
                    cli.id
                );
            }
        }
    }

    #[test]
    fn native_credential_launch_does_not_inject_empty_provider_credentials() {
        let cli = acp_cli("codex").unwrap();
        let launch = cli.project(
            &ResolvedModel {
                base_url: String::new(),
                model: "gpt-5-codex".into(),
                api_key: String::new(),
            },
            None,
            &[],
        );
        assert!(env_of(&launch, "OPENAI_BASE_URL").is_none());
        assert!(env_of(&launch, "OPENAI_API_KEY").is_none());
        assert_eq!(
            env_of(&launch, "OPENAI_MODEL").as_deref(),
            Some("gpt-5-codex")
        );
    }

    #[test]
    fn every_cli_keeps_the_secret_unshadowable_by_passthrough() {
        let m = resolved();
        for cli in known_acp_clis() {
            let d = cli.model_delivery;
            // A hostile passthrough tries to override the modeled model + the secret.
            let extra = vec![
                (d.model.to_string(), "attacker-model".to_string()),
                (d.key.to_string(), "attacker-key".to_string()),
            ];
            let launch = cli.project(&m, None, &extra);
            assert_eq!(
                env_of(&launch, d.model).as_deref(),
                Some(m.model.as_str()),
                "{}: typed model wins",
                cli.id
            );
            assert_eq!(
                env_of(&launch, d.key).as_deref(),
                Some(m.api_key.as_str()),
                "{}: secret unshadowable",
                cli.id
            );
        }
    }

    #[test]
    fn every_cli_egresses_through_the_gateway_with_a_lease_never_a_raw_key() {
        // D-R2 for the whole catalog: no matter which CLI runs in the sandbox, a
        // cloud-managed launch carries only a lease token, never a raw provider key.
        let raw = "sk-RAW-PROVIDER-SECRET"; // awaken-allow: secret
        for cli in known_acp_clis() {
            let model = ResolvedModel::cloud_managed_gateway(
                "https://gateway.awaken.internal",
                "some-model",
                "lease-tok-123", // awaken-allow: secret
            );
            let launch = cli.project(&model, None, &[]);
            assert!(
                launch.env.iter().all(|(_, v)| v != raw),
                "{}: raw key must never appear",
                cli.id
            );
            assert_eq!(
                env_of(&launch, cli.model_delivery.key).as_deref(),
                Some("lease-tok-123"),
                "{}: key env holds the lease token",
                cli.id
            );
            assert_eq!(
                env_of(&launch, cli.model_delivery.base_url).as_deref(),
                Some("https://gateway.awaken.internal"),
                "{}: base_url is the gateway",
                cli.id
            );
        }
    }

    #[test]
    fn every_local_dir_cli_declares_a_subtree_and_never_harvests_its_credentials() {
        // A LocalDir CLI must name a harvestable session subtree; its credentials
        // (retained_paths) must live OUTSIDE that subtree, so a cross-machine session
        // blob never carries an auth file.
        for cli in known_acp_clis() {
            if let SessionPersistence::LocalDir {
                session_subpath, ..
            } = cli.session_persistence
            {
                assert!(
                    !session_subpath.is_empty(),
                    "{}: LocalDir needs a session subpath",
                    cli.id
                );
                for cred in cli.retained_paths {
                    assert!(
                        !cred.starts_with(session_subpath),
                        "{}: credential {cred} must not live under the harvested session subtree {session_subpath}",
                        cli.id
                    );
                }
            }
        }
    }

    #[test]
    fn every_cli_declares_a_coherent_mcp_interface() {
        // The built side of the MCP-delivery capability: every CLI declares HOW it
        // receives MCP servers. A `ConfigFileToml` CLI writes them into a file in its
        // config home, so that file must be a retained path (it lives across sessions
        // alongside the CLI's own config); an `AcpSession` CLI takes them at
        // `session/new`, so no config file is named.
        for cli in known_acp_clis() {
            match cli.mcp_interface {
                McpInterface::AcpSession => {}
                McpInterface::ConfigFileToml { path } => {
                    assert!(
                        !path.is_empty(),
                        "{}: ConfigFileToml needs a config path",
                        cli.id
                    );
                    assert!(
                        cli.retained_paths.contains(&path),
                        "{}: MCP config file {path} must be a retained config-home path",
                        cli.id
                    );
                }
            }
        }
    }

    #[test]
    fn project_mcp_writes_a_config_file_for_a_config_toml_cli() {
        let codex = acp_cli("codex").unwrap();
        let servers = vec![McpServerConfig {
            name: "github".into(),
            transport: McpTransport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "@mcp/github".into()],
            },
            credential: McpCredential::Reference {
                reference: "broker://gh-token".into(),
            },
        }];
        match codex.project_mcp(&servers) {
            McpDelivery::ConfigFile { path, contents } => {
                assert_eq!(path, "config.toml");
                assert!(contents.contains("[mcp_servers.github]"));
                assert!(contents.contains("command = \"npx\""));
                assert!(contents.contains("\"@mcp/github\""));
                assert!(contents.contains("broker://gh-token"));
            }
            other => panic!("codex delivers MCP via a config file, got {other:?}"),
        }
    }

    #[test]
    fn sandbox_safety_admits_alpha_and_none_but_rejects_beta_inline() {
        // α (Reference) and None are sandbox-safe; β (TrustedInline, a raw secret) is not.
        let server = |cred: McpCredential| McpServerConfig {
            name: "s".into(),
            transport: McpTransport::Http {
                url: "https://s".into(),
            },
            credential: cred,
        };
        assert!(server(McpCredential::None).is_sandbox_safe());
        assert!(
            server(McpCredential::Reference {
                reference: "broker://x".into()
            })
            .is_sandbox_safe()
        );
        assert!(
            !server(McpCredential::TrustedInline {
                secret: "sk-RAW".into()
            })
            .is_sandbox_safe(),
            "a raw inline secret must never be sandbox-safe"
        );
    }

    #[test]
    fn a_beta_inline_credential_renders_a_raw_secret_only_on_the_trusted_path() {
        // β is reachable only on a trusted (non-sandboxed) launch; the config-file render
        // emits the raw secret then. Sandboxed callers are gated by `is_sandbox_safe`.
        let servers = vec![McpServerConfig {
            name: "local".into(),
            transport: McpTransport::Stdio {
                command: "mcp".into(),
                args: vec![],
            },
            credential: McpCredential::TrustedInline {
                secret: "sk-trusted".into(), // awaken-allow: secret
            },
        }];
        let McpDelivery::ConfigFile { contents, .. } =
            acp_cli("codex").unwrap().project_mcp(&servers)
        else {
            panic!("codex is a config-file CLI");
        };
        assert!(contents.contains("credential = \"sk-trusted\""));
        // Serde round-trips the trust boundary (rides the config plane).
        let wire = serde_json::to_string(&servers[0]).unwrap();
        assert!(wire.contains("trusted_inline"));
        assert_eq!(
            serde_json::from_str::<McpServerConfig>(&wire).unwrap(),
            servers[0]
        );
    }

    #[test]
    fn project_mcp_passes_session_servers_for_an_acp_session_cli() {
        let claude = acp_cli("claude").unwrap();
        let servers = vec![McpServerConfig {
            name: "fs".into(),
            transport: McpTransport::Http {
                url: "https://mcp.internal/fs".into(),
            },
            credential: McpCredential::None,
        }];
        match claude.project_mcp(&servers) {
            McpDelivery::SessionServers(s) => assert_eq!(s, servers),
            other => panic!("claude delivers MCP at session/new, got {other:?}"),
        }
    }

    #[test]
    fn a_projected_mcp_config_carries_a_reference_never_a_raw_secret() {
        // D-R2 for MCP: even a config-file delivery holds only the broker reference.
        let raw = "sk-RAW-MCP-TOKEN"; // awaken-allow: secret
        let servers = vec![McpServerConfig {
            name: "x".into(),
            transport: McpTransport::Http {
                url: "https://x".into(),
            },
            credential: McpCredential::Reference {
                reference: "broker://x".into(),
            },
        }];
        let McpDelivery::ConfigFile { contents, .. } =
            acp_cli("codex").unwrap().project_mcp(&servers)
        else {
            panic!("codex is a config-file CLI");
        };
        assert!(
            !contents.contains(raw),
            "a raw secret must never enter the MCP config"
        );
        assert!(contents.contains("broker://x"));
    }

    #[test]
    fn every_cli_projects_mcp_consistently_with_its_declared_interface() {
        // Property over the catalog: a ConfigFileToml CLI yields a ConfigFile at its
        // declared path; an AcpSession CLI passes the servers through. Adding a CLI is
        // covered by construction.
        let servers = vec![McpServerConfig {
            name: "s".into(),
            transport: McpTransport::Http {
                url: "https://s".into(),
            },
            credential: McpCredential::None,
        }];
        for cli in known_acp_clis() {
            match (cli.mcp_interface, cli.project_mcp(&servers)) {
                (
                    McpInterface::ConfigFileToml { path },
                    McpDelivery::ConfigFile { path: p, .. },
                ) => {
                    assert_eq!(p, path, "{}", cli.id)
                }
                (McpInterface::AcpSession, McpDelivery::SessionServers(s)) => {
                    assert_eq!(s, servers, "{}", cli.id)
                }
                (_, d) => panic!("{}: delivery {d:?} disagrees with its interface", cli.id),
            }
        }
    }

    #[test]
    fn opencode_resolves_and_projects_like_a_native_openai_compatible_cli() {
        let cli = acp_cli("opencode").unwrap();
        assert_eq!(cli.command, "opencode");
        assert!(
            !is_dynamic_install(cli),
            "a native CLI has no npm install step"
        );
        let launch = cli.project(&resolved(), None, &[]);
        assert_eq!(
            env_of(&launch, "OPENAI_BASE_URL").as_deref(),
            Some("https://api.minimaxi.com/anthropic")
        );
        assert_eq!(
            env_of(&launch, "OPENAI_API_KEY").as_deref(),
            Some("test-materialized-key")
        );
    }

    #[test]
    fn session_persistence_is_declared_per_cli_with_the_right_keying() {
        // Claude keys sessions by cwd (recovery needs a stable interior cwd); Codex
        // keys by an internal id (cwd-independent). Both are LocalDir → harvestable.
        assert_eq!(
            acp_cli("claude").unwrap().session_persistence,
            SessionPersistence::LocalDir {
                session_subpath: "projects",
                keyed_by: SessionKey::Cwd,
            }
        );
        assert_eq!(
            acp_cli("codex").unwrap().session_persistence,
            SessionPersistence::LocalDir {
                session_subpath: "sessions",
                keyed_by: SessionKey::InternalId,
            }
        );
    }

    #[test]
    fn cloud_managed_gateway_puts_the_lease_token_not_a_raw_key_in_the_env() {
        // D-R2: an ACP CLI in the untrusted sandbox must egress through the gateway
        // with a lease token, never a raw provider key.
        let raw_provider_key = "sk-REAL-PROVIDER-SECRET"; // awaken-allow: secret
        let model = ResolvedModel::cloud_managed_gateway(
            "https://gateway.awaken.internal",
            "claude-opus-4-8",
            "lease-abc123", // awaken-allow: secret
        );
        let cli = acp_cli("claude").unwrap();
        let launch = cli.project(&model, None, &[]);

        // The CLI's base_url is the gateway and its "key" env is the lease token.
        assert_eq!(
            env_of(&launch, "ANTHROPIC_BASE_URL").as_deref(),
            Some("https://gateway.awaken.internal")
        );
        assert_eq!(
            env_of(&launch, "ANTHROPIC_API_KEY").as_deref(),
            Some("lease-abc123")
        );
        // The raw provider key never appears in any launch env value.
        assert!(
            launch.env.iter().all(|(_, v)| v != raw_provider_key),
            "raw provider key must never enter the ACP launch env"
        );
    }

    #[test]
    fn claude_launches_via_pinned_npx_adapter_not_a_native_flag() {
        // Claude Code has no native ACP mode; it is fronted by the npx adapter,
        // pinned to a MAJOR.MINOR (never `@latest`).
        let cli = acp_cli("claude").unwrap();
        assert_eq!(cli.command, "npx");
        assert_eq!(
            cli.args,
            &["-y", "@agentclientprotocol/claude-agent-acp@0.44"]
        );
        assert!(
            is_dynamic_install(cli),
            "an npx wrapper installs on first run"
        );
    }

    #[test]
    fn codex_launches_via_the_pinned_official_adapter() {
        let cli = acp_cli("codex").unwrap();
        assert_eq!(cli.command, "npx");
        assert!(cli.args.contains(&"@agentclientprotocol/codex-acp@1.1"));
        assert!(is_dynamic_install(cli));
    }

    #[test]
    fn codex_projects_model_and_noninteractive_policy_through_codex_config() {
        let cli = acp_cli("codex").unwrap();
        let model = resolved();
        let launch = cli.project(&model, None, &[]);
        let config: serde_json::Value =
            serde_json::from_str(&env_of(&launch, "CODEX_CONFIG").unwrap()).unwrap();
        assert_eq!(config["model"], model.model);
        assert_eq!(config["approval_policy"], "never");
        assert_eq!(config["sandbox_mode"], "workspace-write");
        assert!(!launch.argv.iter().any(|arg| arg == "-c"));
    }

    #[test]
    fn gemini_speaks_acp_natively_and_needs_no_install() {
        let cli = acp_cli("gemini").unwrap();
        assert_eq!(cli.command, "gemini");
        assert_eq!(cli.args, &["--experimental-acp"]);
        assert!(
            !is_dynamic_install(cli),
            "a native CLI has no dynamic-install step"
        );
    }

    #[test]
    fn claude_projects_model_base_url_tier_aliases_and_compact_window() {
        let cli = acp_cli("claude").unwrap();
        let launch = cli.project(&resolved(), Some(1_000_000), &[]);

        assert_eq!(
            launch.argv,
            vec!["npx", "-y", "@agentclientprotocol/claude-agent-acp@0.44"]
        );
        assert_eq!(
            env_of(&launch, "ANTHROPIC_BASE_URL").as_deref(),
            Some("https://api.minimaxi.com/anthropic")
        );
        assert_eq!(
            env_of(&launch, "ANTHROPIC_MODEL").as_deref(),
            Some("MiniMax-M3[1m]")
        );
        // Tier aliases all carry the same resolved model.
        for alias in [
            "ANTHROPIC_SONNET_MODEL",
            "ANTHROPIC_OPUS_MODEL",
            "ANTHROPIC_HAIKU_MODEL",
        ] {
            assert_eq!(env_of(&launch, alias).as_deref(), Some("MiniMax-M3[1m]"));
        }
        // Compact window from the run's context config.
        assert_eq!(
            env_of(&launch, "CLAUDE_CODE_AUTO_COMPACT_WINDOW").as_deref(),
            Some("1000000")
        );
        // The secret is injected into the launch env (host-materialized).
        assert_eq!(
            env_of(&launch, "ANTHROPIC_API_KEY").as_deref(),
            Some("test-materialized-key")
        );
    }

    #[test]
    fn typed_model_delivery_wins_over_passthrough_and_secret_is_unshadowable() {
        let cli = acp_cli("claude").unwrap();
        // A stray passthrough tries to override the modeled model + the secret.
        let extra = vec![
            ("ANTHROPIC_MODEL".to_string(), "attacker-model".to_string()),
            ("ANTHROPIC_API_KEY".to_string(), "attacker-key".to_string()),
            ("EXTRA_FLAG".to_string(), "1".to_string()),
        ];
        let launch = cli.project(&resolved(), None, &extra);
        // Typed model wins; secret stays the host's; unmodeled passthrough survives.
        assert_eq!(
            env_of(&launch, "ANTHROPIC_MODEL").as_deref(),
            Some("MiniMax-M3[1m]")
        );
        assert_eq!(
            env_of(&launch, "ANTHROPIC_API_KEY").as_deref(),
            Some("test-materialized-key")
        );
        assert_eq!(env_of(&launch, "EXTRA_FLAG").as_deref(), Some("1"));
    }

    #[test]
    fn a_cli_without_a_compact_window_omits_it() {
        let cli = acp_cli("codex").unwrap();
        let launch = cli.project(&resolved(), Some(999), &[]);
        assert!(env_of(&launch, "CLAUDE_CODE_AUTO_COMPACT_WINDOW").is_none());
        assert_eq!(
            env_of(&launch, "OPENAI_MODEL").as_deref(),
            Some("MiniMax-M3[1m]")
        );
    }
}
