//! The **ACP CLI catalog**: one data row per external coding-agent CLI (Claude
//! Code, Codex, Gemini…) describing how to launch it and how the resolved model
//! reaches it. Pure data + a projection function — no `match adapter_kind` anywhere
//! (adding a CLI is a row, not a branch). A row is a *catalog binding* an agent
//! references by id (`Backend::Acp { cli }`), the same category as a model provider
//! — never agent config itself.

use crate::AcpLaunch;

/// How the resolved model coordinates land in a CLI's environment. Every shipped
/// ACP CLI delivers the model via env keys; only *which* keys differ, so this is a
/// data row, not a behavior — a struct, not an enum. (A future CLI that takes the
/// model as an argv flag would grow this into a sum then, not before.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelDelivery {
    /// Env key for the endpoint base URL (e.g. `ANTHROPIC_BASE_URL`).
    pub base_url: &'static str,
    /// Env key for the model name (e.g. `ANTHROPIC_MODEL`).
    pub model: &'static str,
    /// Env key for the API key (a secret — the host materializes it; never stored).
    pub key: &'static str,
    /// Extra model-name env keys the CLI reads as tier aliases, all set to the same
    /// resolved model (e.g. `ANTHROPIC_SONNET_MODEL`/`OPUS`/`HAIKU`).
    pub aliases: &'static [&'static str],
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
    pub command: &'static str,
    pub args: &'static [&'static str],
    pub model_delivery: ModelDelivery,
    pub mcp_interface: McpInterface,
    /// Env key naming the CLI's isolated config directory (e.g. `CLAUDE_CONFIG_DIR`).
    pub config_home_env: &'static str,
    /// The memory file the CLI reads from its config home (e.g. `CLAUDE.md`).
    pub memory_entrypoint: &'static str,
    /// Paths under the config home that survive across sessions (auth, config).
    pub retained_paths: &'static [&'static str],
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
        env.insert(d.base_url.to_string(), model.base_url.clone());
        env.insert(d.model.to_string(), model.model.clone());
        for alias in d.aliases {
            env.insert((*alias).to_string(), model.model.clone());
        }
        if let (Some(key), Some(window)) = (self.context_window_env, context_window) {
            env.insert(key.to_string(), window.to_string());
        }
        // The secret goes last so no passthrough key can shadow it.
        env.insert(d.key.to_string(), model.api_key.clone());

        let mut argv = vec![self.command.to_string()];
        argv.extend(self.args.iter().map(|s| (*s).to_string()));
        AcpLaunch {
            argv,
            env: env.into_iter().collect(),
        }
    }
}

const CLAUDE: AcpCli = AcpCli {
    id: "claude",
    command: "claude",
    args: &["--acp"],
    model_delivery: ModelDelivery {
        base_url: "ANTHROPIC_BASE_URL",
        model: "ANTHROPIC_MODEL",
        key: "ANTHROPIC_API_KEY",
        aliases: &[
            "ANTHROPIC_SONNET_MODEL",
            "ANTHROPIC_OPUS_MODEL",
            "ANTHROPIC_HAIKU_MODEL",
        ],
    },
    mcp_interface: McpInterface::AcpSession,
    config_home_env: "CLAUDE_CONFIG_DIR",
    memory_entrypoint: "CLAUDE.md",
    retained_paths: &[".credentials.json", "settings.json"],
    context_window_env: Some("CLAUDE_CODE_AUTO_COMPACT_WINDOW"),
    env: &[],
};

const CODEX: AcpCli = AcpCli {
    id: "codex",
    command: "codex",
    args: &["acp"],
    model_delivery: ModelDelivery {
        base_url: "OPENAI_BASE_URL",
        model: "OPENAI_MODEL",
        key: "OPENAI_API_KEY",
        aliases: &[],
    },
    mcp_interface: McpInterface::ConfigFileToml {
        path: "config.toml",
    },
    config_home_env: "CODEX_HOME",
    memory_entrypoint: "AGENTS.md",
    retained_paths: &["auth.json", "config.toml"],
    context_window_env: None,
    env: &[],
};

const GEMINI: AcpCli = AcpCli {
    id: "gemini",
    command: "gemini",
    args: &["--experimental-acp"],
    model_delivery: ModelDelivery {
        base_url: "GOOGLE_GEMINI_BASE_URL",
        model: "GEMINI_MODEL",
        key: "GEMINI_API_KEY",
        aliases: &[],
    },
    mcp_interface: McpInterface::AcpSession,
    config_home_env: "GEMINI_DIR",
    memory_entrypoint: "GEMINI.md",
    retained_paths: &[],
    context_window_env: None,
    env: &[],
};

/// The known ACP CLIs. Adding one is a row here — never a branch elsewhere.
#[must_use]
pub fn known_acp_clis() -> &'static [AcpCli] {
    &[CLAUDE, CODEX, GEMINI]
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
        assert!(acp_cli("no_such_cli").is_none());
    }

    #[test]
    fn claude_projects_model_base_url_tier_aliases_and_compact_window() {
        let cli = acp_cli("claude").unwrap();
        let launch = cli.project(&resolved(), Some(1_000_000), &[]);

        assert_eq!(launch.argv, vec!["claude", "--acp"]);
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
