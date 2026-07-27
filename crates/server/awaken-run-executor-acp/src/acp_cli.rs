//! The **ACP CLI catalog**: one data row per external coding-agent CLI (Claude
//! Code, Codex, Gemini…) describing how to launch it and how the resolved model
//! reaches it. Pure data + a projection function — no `match adapter_kind` anywhere
//! (adding a CLI is a row, not a branch). A row is a *catalog binding* an agent
//! references by id (`Backend::Acp { cli }`), the same category as a model provider
//! — never agent config itself.

use crate::{AcpLaunch, OpenError};
use awaken_provisioning_contract as pc;

/// How the local host obtains the ACP-serving executable. This is the sole local
/// argv authority; discovery and launch both project it instead of inferring an
/// install strategy from a command name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpAcquisition {
    Direct {
        executable: &'static str,
        args: &'static [&'static str],
    },
    PinnedNpmWrapper {
        runner: &'static str,
        package: &'static str,
    },
}

impl AcpAcquisition {
    #[must_use]
    pub fn executable(self) -> &'static str {
        match self {
            Self::Direct { executable, .. } => executable,
            Self::PinnedNpmWrapper { runner, .. } => runner,
        }
    }

    #[must_use]
    pub fn local_argv(self) -> Vec<String> {
        match self {
            Self::Direct { executable, args } => std::iter::once(executable)
                .chain(args.iter().copied())
                .map(str::to_string)
                .collect(),
            Self::PinnedNpmWrapper { runner, package } => {
                vec![runner.to_string(), "-y".into(), package.to_string()]
            }
        }
    }

    #[must_use]
    pub fn is_dynamic_install(self) -> bool {
        matches!(self, Self::PinnedNpmWrapper { .. })
    }
}

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
    /// Env key for the API key (a process-secret broker reference; never plaintext).
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
    /// Written into a config file inside an isolated home for legacy adapters.
    ConfigFileToml { path: &'static str },
}

/// Provider-owned credential artifact codecs supported by the managed launch
/// boundary. The codec is catalog data; generic Host code never branches on a
/// CLI id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialArtifactCodec {
    CodexAuthJson,
    ClaudeCredentialsJson,
}

/// One managed artifact selected from a CLI profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialArtifactSpec {
    pub codec: CredentialArtifactCodec,
    pub relative_path: &'static str,
}

/// How an isolated, Awaken-managed launch receives provider credentials. This
/// does not describe backend-owned local login; that mutually exclusive mode
/// never materializes provider material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedCredentialDelivery {
    ProcessSecret,
    Artifact(CredentialArtifactSpec),
    RefreshArtifactOrProcessSecret(CredentialArtifactSpec),
}

impl ManagedCredentialDelivery {
    /// Select an artifact only when this profile and the pinned credential shape
    /// require one.
    #[must_use]
    pub fn credential_artifact(self, has_refresh: bool) -> Option<CredentialArtifactSpec> {
        match self {
            Self::Artifact(spec) => Some(spec),
            Self::RefreshArtifactOrProcessSecret(spec) if has_refresh => Some(spec),
            Self::ProcessSecret | Self::RefreshArtifactOrProcessSecret(_) => None,
        }
    }

    #[must_use]
    pub fn allows_process_secret(self) -> bool {
        matches!(
            self,
            Self::ProcessSecret | Self::RefreshArtifactOrProcessSecret(_)
        )
    }
}

/// The definition of one external ACP CLI: how to launch it and project config onto
/// it. Referenced by `Backend::Acp { cli }` via [`AcpCli::id`].
#[derive(Debug, Clone, Copy)]
pub struct AcpCli {
    pub id: &'static str,
    /// Operator-facing name projected into the management capability view. The
    /// catalog owns it so authoring cannot advertise an adapter execution cannot
    /// launch.
    pub display_name: &'static str,
    /// Operator-facing launch summary. This is descriptive metadata, never a
    /// second launch/configuration source.
    pub description: &'static str,
    /// Host/local acquisition and argv projection.
    pub acquisition: AcpAcquisition,
    /// Equivalent argv for a worker image where the adapter is preinstalled. Keeping
    /// this in the catalog row avoids both runtime package downloads and adapter
    /// branches in the container mechanism.
    pub container_argv: &'static [&'static str],
    /// Environment projection used only by legacy API-key adapters. `None`
    /// means the adapter requires its provider-specific credential driver.
    pub model_delivery: Option<ModelDelivery>,
    /// Managed provider credential delivery. Local backend-owned login is a
    /// separate provisioning mode and does not consume this field.
    pub managed_credential_delivery: ManagedCredentialDelivery,
    /// ACP authentication method selected after initialize, when the adapter
    /// exposes more than one protocol-level method.
    pub auth_method_id: Option<&'static str>,
    pub mcp_interface: McpInterface,
    /// Env key naming the CLI's isolated config directory (e.g. `CLAUDE_CONFIG_DIR`).
    pub config_home_env: Option<&'static str>,
    /// Additional standard/vendor home variables that point at the same isolated
    /// root. Some CLIs split extensions from global config/data/cache state.
    pub config_home_aliases: &'static [&'static str],
    /// The memory file the CLI reads from its config home (e.g. `CLAUDE.md`).
    pub memory_entrypoint: &'static str,
    /// Non-credential paths under the config home that survive across sessions.
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

/// One already-selected process-secret requirement. The opaque reference is
/// resolved only by the final launch provider through `SecretBroker`; this type
/// carries no material, policy, or credential-selection behavior.
#[derive(Clone, PartialEq, Eq)]
pub struct ProcessSecretRequirement {
    reference: String,
}

/// One claim-fenced provider credential artifact. The executor carries only the
/// broker reference and provider-owned relative path; bytes are materialized by
/// the sandbox immediately before the ACP process starts.
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialArtifactRequirement {
    reference: String,
    relative_path: String,
}

impl CredentialArtifactRequirement {
    #[must_use]
    pub fn new(reference: impl Into<String>, relative_path: impl Into<String>) -> Self {
        Self {
            reference: reference.into(),
            relative_path: relative_path.into(),
        }
    }

    #[must_use]
    pub fn reference(&self) -> &str {
        &self.reference
    }

    #[must_use]
    pub fn relative_path(&self) -> &str {
        &self.relative_path
    }
}

impl std::fmt::Debug for CredentialArtifactRequirement {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CredentialArtifactRequirement(***)")
    }
}

impl ProcessSecretRequirement {
    #[must_use]
    pub fn new(reference: impl Into<String>) -> Self {
        Self {
            reference: reference.into(),
        }
    }

    #[must_use]
    pub fn reference(&self) -> &str {
        &self.reference
    }
}

impl std::fmt::Debug for ProcessSecretRequirement {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ProcessSecretRequirement(***)")
    }
}

/// The host-resolved model coordinates handed to the projection. Base URL and
/// model come from the resolved spec; the credential remains a typed broker
/// requirement until the concrete process-launch boundary.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub base_url: String,
    pub model: String,
    pub process_secret: Option<ProcessSecretRequirement>,
    pub credential_artifact: Option<CredentialArtifactRequirement>,
}

impl ResolvedModel {
    /// Cloud-managed egress (D-R2, ADR-0021 §9/R2). An ACP CLI runs inside the
    /// untrusted sandbox, so it must never hold a raw provider key: its egress is
    /// mediated by the gateway. `base_url` is the **gateway**, and the CLI's "API
    /// key" is a brokered requirement for a short-lived **lease token** — the gateway
    /// injects the real provider credential out of the sandbox's address space. Build
    /// the ACP model this way from a resolved cloud-managed gateway (base URL + lease
    /// reference) so neither the lease nor raw key enters planning state as plaintext.
    #[must_use]
    pub fn cloud_managed_gateway(
        gateway_base_url: impl Into<String>,
        model: impl Into<String>,
        lease_reference: impl Into<String>,
    ) -> Self {
        Self {
            base_url: gateway_base_url.into(),
            model: model.into(),
            process_secret: Some(ProcessSecretRequirement::new(lease_reference)),
            credential_artifact: None,
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
    pub fn try_project(
        &self,
        model: &ResolvedModel,
        context_window: Option<u64>,
        extra_env: &[(String, String)],
    ) -> Result<AcpLaunch, OpenError> {
        use std::collections::BTreeMap;
        let mut env: BTreeMap<String, pc::EnvVar> = BTreeMap::new();
        let inline = |name: &str, value: String| pc::EnvVar {
            name: name.to_string(),
            value: pc::EnvValue::Inline { value },
            visibility: pc::EnvVisibility::Process,
        };
        for (k, v) in self.env {
            env.insert((*k).to_string(), inline(k, (*v).to_string()));
        }
        for (k, v) in extra_env {
            env.insert(k.clone(), inline(k, v.clone()));
        }
        let d = self.model_delivery.as_ref();
        if d.is_none() && model.credential_artifact.is_none() {
            return Err(OpenError(format!(
                "credential_driver_required: {}",
                self.id
            )));
        }
        let Some(d) = d else {
            let argv = self.acquisition.local_argv();
            return Ok(AcpLaunch {
                argv,
                env: env.into_values().collect(),
            });
        };
        if !model.base_url.is_empty() {
            env.insert(
                d.base_url.to_string(),
                inline(d.base_url, model.base_url.clone()),
            );
        }
        if !model.model.is_empty() {
            env.insert(d.model.to_string(), inline(d.model, model.model.clone()));
            for alias in d.aliases {
                env.insert((*alias).to_string(), inline(alias, model.model.clone()));
            }
        }
        if let (Some(key), Some(window)) = (self.context_window_env, context_window) {
            env.insert(key.to_string(), inline(key, window.to_string()));
        }
        // The typed secret goes last so no passthrough key can shadow it.
        if let Some(secret) = &model.process_secret {
            env.insert(
                d.key.to_string(),
                pc::EnvVar {
                    name: d.key.to_string(),
                    value: pc::EnvValue::Secret {
                        reference: secret.reference().to_string(),
                    },
                    visibility: pc::EnvVisibility::Process,
                },
            );
        }

        let mut argv = self.acquisition.local_argv();
        if let Some(key) = d.model_config_key
            && !model.model.is_empty()
        {
            if let Some(config_env) = d.model_config_env {
                let mut config = env
                    .get(config_env)
                    .and_then(|var| match &var.value {
                        pc::EnvValue::Inline { value } => serde_json::from_str::<
                            serde_json::Map<String, serde_json::Value>,
                        >(value)
                        .ok(),
                        pc::EnvValue::Secret { .. } => None,
                    })
                    .unwrap_or_default();
                config.insert(
                    key.to_string(),
                    serde_json::Value::String(model.model.clone()),
                );
                env.insert(
                    config_env.to_string(),
                    inline(config_env, serde_json::Value::Object(config).to_string()),
                );
            } else {
                argv.push("-c".to_string());
                argv.push(format!("{key}={:?}", model.model));
            }
        }
        Ok(AcpLaunch {
            argv,
            env: env.into_values().collect(),
        })
    }

    #[cfg(test)]
    fn project(
        &self,
        model: &ResolvedModel,
        context_window: Option<u64>,
        extra_env: &[(String, String)],
    ) -> AcpLaunch {
        self.try_project(model, context_window, extra_env)
            .expect("legacy environment projection")
    }
}

/// A neutral MCP server a run wants an ACP CLI to reach. Serializable so it rides the
/// config plane into `ResolvedSpec.plugin_config` (the seam `mcp_servers_of` reads back).
/// Authentication is deliberately absent: the Runtime Host projects either an
/// anonymous endpoint or an already-mediated generation route. ACP never receives
/// a real or virtual credential through this transport DTO.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransport,
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
    display_name: "Claude Code",
    description: "Claude Code via the pinned ACP adapter. Reads CLAUDE.md.",
    acquisition: AcpAcquisition::PinnedNpmWrapper {
        runner: "npx",
        package: "@agentclientprotocol/claude-agent-acp@0.44",
    },
    container_argv: &["claude-agent-acp"],
    model_delivery: Some(ModelDelivery {
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
    }),
    managed_credential_delivery: ManagedCredentialDelivery::RefreshArtifactOrProcessSecret(
        CredentialArtifactSpec {
            codec: CredentialArtifactCodec::ClaudeCredentialsJson,
            relative_path: ".credentials.json",
        },
    ),
    auth_method_id: None,
    mcp_interface: McpInterface::AcpSession,
    config_home_env: Some("CLAUDE_CONFIG_DIR"),
    config_home_aliases: &[],
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
// not a native `codex acp` subcommand. Credential and model materialization are
// provider-driver responsibilities; the generic environment/file projection is
// deliberately unavailable for this row.
const CODEX: AcpCli = AcpCli {
    id: "codex",
    display_name: "Codex",
    description: "OpenAI Codex via the pinned ACP adapter. Reads AGENTS.md.",
    acquisition: AcpAcquisition::PinnedNpmWrapper {
        runner: "npx",
        package: "@agentclientprotocol/codex-acp@1.1",
    },
    container_argv: &["codex-acp"],
    model_delivery: None,
    managed_credential_delivery: ManagedCredentialDelivery::Artifact(CredentialArtifactSpec {
        codec: CredentialArtifactCodec::CodexAuthJson,
        relative_path: ".codex/auth.json",
    }),
    auth_method_id: None,
    mcp_interface: McpInterface::AcpSession,
    config_home_env: None,
    config_home_aliases: &[],
    memory_entrypoint: "AGENTS.md",
    retained_paths: &[],
    // Codex writes rollout files under `sessions/`, keyed by an internal id.
    session_persistence: SessionPersistence::None,
    context_window_env: None,
    env: &[],
};

// Gemini CLI speaks ACP natively via `--experimental-acp` (no npm wrapper), so it
// is a Direct launch with no dynamic-install step.
const GEMINI: AcpCli = AcpCli {
    id: "gemini",
    display_name: "Gemini CLI",
    description: "Gemini CLI via its native ACP mode. Reads GEMINI.md.",
    acquisition: AcpAcquisition::Direct {
        executable: "gemini",
        args: &["--experimental-acp"],
    },
    container_argv: &["gemini", "--experimental-acp"],
    model_delivery: Some(ModelDelivery {
        base_url: "GOOGLE_GEMINI_BASE_URL",
        model: "GEMINI_MODEL",
        model_config_key: None,
        model_config_env: None,
        key: "GEMINI_API_KEY",
        aliases: &[],
    }),
    managed_credential_delivery: ManagedCredentialDelivery::ProcessSecret,
    auth_method_id: None,
    mcp_interface: McpInterface::AcpSession,
    config_home_env: Some("GEMINI_DIR"),
    config_home_aliases: &[],
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
    display_name: "OpenCode",
    description: "OpenCode via its native ACP mode. Reads AGENTS.md.",
    acquisition: AcpAcquisition::Direct {
        executable: "opencode",
        args: &["acp"],
    },
    container_argv: &["opencode", "acp"],
    model_delivery: Some(ModelDelivery {
        base_url: "OPENAI_BASE_URL",
        model: "OPENAI_MODEL",
        model_config_key: None,
        model_config_env: None,
        key: "OPENAI_API_KEY",
        aliases: &[],
    }),
    managed_credential_delivery: ManagedCredentialDelivery::ProcessSecret,
    auth_method_id: None,
    mcp_interface: McpInterface::AcpSession,
    config_home_env: Some("OPENCODE_CONFIG_DIR"),
    config_home_aliases: &[
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
    ],
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

/// Canonical test fixture for the retired config-file delivery mechanism. Keeping
/// it outside the production catalog proves legacy behavior without assigning it
/// to Codex or creating multiple synthetic rows across test modules.
#[cfg(test)]
pub(crate) fn legacy_config_file_cli() -> AcpCli {
    let mut cli = *acp_cli("claude").expect("claude test fixture");
    cli.mcp_interface = McpInterface::ConfigFileToml {
        path: "config.toml",
    };
    cli.config_home_env = Some("TEST_CONFIG_HOME");
    cli.retained_paths = &["config.toml"];
    cli
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved() -> ResolvedModel {
        ResolvedModel {
            base_url: "https://api.minimaxi.com/anthropic".to_string(),
            model: "MiniMax-M3[1m]".to_string(),
            process_secret: Some(ProcessSecretRequirement::new("lease://test-model")),
            credential_artifact: None,
        }
    }

    fn env_of(launch: &AcpLaunch, key: &str) -> Option<String> {
        launch
            .env
            .iter()
            .find(|var| var.name == key)
            .and_then(|var| match &var.value {
                pc::EnvValue::Inline { value } => Some(value.clone()),
                pc::EnvValue::Secret { .. } => None,
            })
    }

    fn secret_ref_of<'a>(launch: &'a AcpLaunch, key: &str) -> Option<&'a str> {
        launch
            .env
            .iter()
            .find(|var| var.name == key)
            .and_then(|var| match &var.value {
                pc::EnvValue::Secret { reference } => Some(reference.as_str()),
                pc::EnvValue::Inline { .. } => None,
            })
    }

    #[test]
    fn registry_holds_the_known_clis_and_unknown_fails_closed() {
        assert!(acp_cli("claude").is_some());
        assert!(acp_cli("codex").is_some());
        assert!(acp_cli("gemini").is_some());
        assert!(acp_cli("opencode").is_some());
        assert!(acp_cli("no_such_cli").is_none());
    }

    #[test]
    fn managed_credential_delivery_is_catalog_data() {
        // Cause graph: catalog profile + pinned credential shape -> exactly one
        // managed delivery mechanism. Host code never reclassifies by CLI id.
        //
        // | Rule | Profile | Refresh metadata | Artifact | Process secret |
        // | C1 | Codex | no/yes | auth.json | no |
        // | C2 | Claude | yes | .credentials.json | no |
        // | C3 | Claude | no | no | yes |
        // | C4 | Gemini/OpenCode | no/yes | no | yes |
        let codex = acp_cli("codex").unwrap().managed_credential_delivery;
        let codex_artifact = codex.credential_artifact(false).expect("C1");
        assert_eq!(codex_artifact.relative_path, ".codex/auth.json", "C1");
        assert!(!codex.allows_process_secret(), "C1");

        let claude = acp_cli("claude").unwrap().managed_credential_delivery;
        assert_eq!(
            claude.credential_artifact(true).expect("C2").relative_path,
            ".credentials.json",
            "C2"
        );
        assert_eq!(claude.credential_artifact(false), None, "C3");
        assert!(claude.allows_process_secret(), "C3");

        for rule in ["gemini", "opencode"] {
            let delivery = acp_cli(rule).unwrap().managed_credential_delivery;
            assert_eq!(delivery.credential_artifact(false), None, "C4 {rule}");
            assert_eq!(delivery.credential_artifact(true), None, "C4 {rule}");
            assert!(delivery.allows_process_secret(), "C4 {rule}");
        }
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
            assert!(
                !cli.acquisition.executable().is_empty(),
                "{}: acquisition executable is set",
                cli.id
            );
            assert_eq!(
                cli.config_home_env.is_some(),
                cli.model_delivery.is_some(),
                "{}: only driver-managed adapters omit a generic config home",
                cli.id
            );
        }
    }

    #[test]
    fn every_cli_injects_the_resolved_model_base_url_and_secret_key() {
        let m = resolved();
        for cli in known_acp_clis() {
            let Some(d) = cli.model_delivery.as_ref() else {
                assert!(cli.try_project(&m, None, &[]).is_err(), "{}", cli.id);
                continue;
            };
            let launch = cli.project(&m, None, &[]);
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
                secret_ref_of(&launch, d.key),
                Some("lease://test-model"),
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
    fn every_cli_keeps_the_secret_unshadowable_by_passthrough() {
        let m = resolved();
        for cli in known_acp_clis() {
            let Some(d) = cli.model_delivery else {
                assert!(cli.try_project(&m, None, &[]).is_err(), "{}", cli.id);
                continue;
            };
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
                secret_ref_of(&launch, d.key),
                Some("lease://test-model"),
                "{}: secret unshadowable",
                cli.id
            );
        }
    }

    #[test]
    fn every_cli_egresses_through_the_gateway_with_a_lease_never_a_raw_key() {
        // D-R2 for the whole catalog: no matter which CLI runs in the sandbox, a
        // cloud-managed launch carries only a lease requirement, never a raw provider key.
        let raw = "sk-RAW-PROVIDER-SECRET"; // awaken-allow: secret
        for cli in known_acp_clis() {
            let Some(delivery) = cli.model_delivery else {
                assert!(
                    cli.try_project(&resolved(), None, &[]).is_err(),
                    "{}",
                    cli.id
                );
                continue;
            };
            let model = ResolvedModel::cloud_managed_gateway(
                "https://gateway.awaken.internal",
                "some-model",
                "lease://gateway-123",
            );
            let launch = cli.project(&model, None, &[]);
            assert!(
                !format!("{launch:?}").contains(raw),
                "{}: raw key must never appear",
                cli.id
            );
            assert_eq!(
                secret_ref_of(&launch, delivery.key),
                Some("lease://gateway-123"),
                "{}: key env holds only the broker reference",
                cli.id
            );
            assert_eq!(
                env_of(&launch, delivery.base_url).as_deref(),
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
        let cli = legacy_config_file_cli();
        let servers = vec![McpServerConfig {
            name: "github".into(),
            transport: McpTransport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "@mcp/github".into()],
            },
        }];
        match cli.project_mcp(&servers) {
            McpDelivery::ConfigFile { path, contents } => {
                assert_eq!(path, "config.toml");
                assert!(contents.contains("[mcp_servers.github]"));
                assert!(contents.contains("command = \"npx\""));
                assert!(contents.contains("\"@mcp/github\""));
                assert!(!contents.to_ascii_lowercase().contains("credential"));
            }
            other => panic!("config-file fixture must deliver a file, got {other:?}"),
        }
    }

    #[test]
    fn project_mcp_passes_session_servers_for_an_acp_session_cli() {
        let claude = acp_cli("claude").unwrap();
        let servers = vec![McpServerConfig {
            name: "fs".into(),
            transport: McpTransport::Http {
                url: "https://mcp.internal/fs".into(),
            },
        }];
        match claude.project_mcp(&servers) {
            McpDelivery::SessionServers(s) => assert_eq!(s, servers),
            other => panic!("claude delivers MCP at session/new, got {other:?}"),
        }
    }

    #[test]
    fn a_retained_credential_field_cannot_enter_projected_mcp_config() {
        let raw = "sk-RAW-MCP-TOKEN"; // awaken-allow: secret
        let server: McpServerConfig = serde_json::from_value(serde_json::json!({
            "name": "x",
            "transport": {"kind": "http", "url": "https://x"},
            "credential": {"auth": "trusted_inline", "secret": raw}
        }))
        .expect("retained unknown field remains decode-compatible");
        let McpDelivery::ConfigFile { contents, .. } =
            legacy_config_file_cli().project_mcp(&[server])
        else {
            panic!("config-file fixture must deliver a file");
        };
        assert!(
            !contents.contains(raw),
            "a raw secret must never enter the MCP config"
        );
        assert!(!contents.to_ascii_lowercase().contains("credential"));
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
        let launch = cli.project(&resolved(), None, &[]);
        assert_eq!(
            env_of(&launch, "OPENAI_BASE_URL").as_deref(),
            Some("https://api.minimaxi.com/anthropic")
        );
        assert_eq!(
            secret_ref_of(&launch, "OPENAI_API_KEY"),
            Some("lease://test-model")
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
            SessionPersistence::None
        );
    }

    #[test]
    fn cloud_managed_gateway_puts_a_lease_requirement_not_a_raw_key_in_the_plan() {
        // D-R2: an ACP CLI in the untrusted sandbox must egress through the gateway
        // with a brokered lease token, never a raw provider key.
        let raw_provider_key = "sk-REAL-PROVIDER-SECRET"; // awaken-allow: secret
        let model = ResolvedModel::cloud_managed_gateway(
            "https://gateway.awaken.internal",
            "claude-opus-4-8",
            "lease://gateway-abc123",
        );
        let cli = acp_cli("claude").unwrap();
        let launch = cli.project(&model, None, &[]);

        // The CLI's base_url is the gateway and its key requirement is opaque.
        assert_eq!(
            env_of(&launch, "ANTHROPIC_BASE_URL").as_deref(),
            Some("https://gateway.awaken.internal")
        );
        assert_eq!(
            secret_ref_of(&launch, "ANTHROPIC_API_KEY"),
            Some("lease://gateway-abc123")
        );
        // The raw provider key never appears in any launch env value.
        assert!(
            !format!("{launch:?}").contains(raw_provider_key),
            "raw provider key must never enter the ACP launch env"
        );
    }

    #[test]
    fn acquisition_kind_is_the_single_source_for_local_argv_and_install_phase() {
        // Acquisition cause graph:
        // catalog kind ──> exact local argv ──> launch + discovery executable
        //              └─> dynamic-install phase
        //
        // Decision table:
        // D1 pinned wrapper | runner,-y,pinned package | dynamic
        // D2 direct native  | executable,native args   | not dynamic
        let cases: [(&str, &[&str], bool); 4] = [
            (
                "claude",
                &["npx", "-y", "@agentclientprotocol/claude-agent-acp@0.44"],
                true,
            ),
            (
                "codex",
                &["npx", "-y", "@agentclientprotocol/codex-acp@1.1"],
                true,
            ),
            ("gemini", &["gemini", "--experimental-acp"], false),
            ("opencode", &["opencode", "acp"], false),
        ];

        for (id, expected_argv, expected_dynamic) in cases {
            let acquisition = acp_cli(id).unwrap().acquisition;
            assert_eq!(acquisition.executable(), expected_argv[0], "{id}");
            assert_eq!(acquisition.local_argv(), expected_argv, "{id}");
            assert_eq!(acquisition.is_dynamic_install(), expected_dynamic, "{id}");
            assert!(
                acquisition
                    .local_argv()
                    .iter()
                    .all(|arg| !arg.ends_with("@latest")),
                "{id}: acquisition must be reproducibly pinned"
            );
        }
    }

    #[test]
    fn codex_requires_its_provider_credential_driver() {
        let cli = acp_cli("codex").unwrap();
        assert!(cli.model_delivery.is_none());
        assert!(cli.config_home_env.is_none());
        assert!(cli.retained_paths.is_empty());
        let error = cli.try_project(&resolved(), None, &[]).unwrap_err();
        assert_eq!(error.0, "credential_driver_required: codex");
    }

    #[test]
    fn codex_artifact_launch_has_no_model_or_credential_environment_projection() {
        let cli = acp_cli("codex").unwrap();
        let mut model = resolved();
        model.process_secret = None;
        model.credential_artifact = Some(CredentialArtifactRequirement::new(
            "awaken-credential-artifact://one-shot",
            ".codex/auth.json",
        ));
        let launch = cli.try_project(&model, None, &[]).expect("artifact launch");
        assert!(
            launch
                .env
                .iter()
                .all(|var| { !var.name.starts_with("OPENAI_") && !var.name.starts_with("CODEX_") })
        );
    }

    #[test]
    fn claude_projects_model_base_url_tier_aliases_and_compact_window() {
        // Launch-projection cause graph (the catalog is the sole constructor):
        // C1 catalog row has model delivery + C2 resolved coordinates
        //   -> E1 catalog argv + E2 typed model env + E3 optional window env.
        // A CLI without model delivery instead requires its typed artifact driver;
        // no adapter-specific AcpLaunch constructor can bypass these branches.
        //
        // Decision table (covered by this test and the Codex tests above):
        // | Rule | delivery | artifact | coordinates | result |
        // | L1 | present | - | present | catalog argv + typed env |
        // | L2 | absent | absent | any | fail closed |
        // | L3 | absent | present | any | catalog argv, no provider env |
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
        // Projection carries only the one-shot requirement. Materialization is
        // deferred until the concrete process boundary.
        assert_eq!(
            secret_ref_of(&launch, "ANTHROPIC_API_KEY"),
            Some("lease://test-model")
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
        // Typed model wins; the opaque secret requirement cannot be shadowed by
        // ordinary env; unmodeled passthrough survives.
        assert_eq!(
            env_of(&launch, "ANTHROPIC_MODEL").as_deref(),
            Some("MiniMax-M3[1m]")
        );
        assert_eq!(
            secret_ref_of(&launch, "ANTHROPIC_API_KEY"),
            Some("lease://test-model")
        );
        assert_eq!(env_of(&launch, "EXTRA_FLAG").as_deref(), Some("1"));
    }
}
