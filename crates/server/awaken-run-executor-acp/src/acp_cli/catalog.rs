//! Data-only rows for the external ACP adapter catalog.

use super::*;

// Claude Code does NOT speak ACP natively (there is no `claude --acp`). It is
// fronted by the official adapter package `@agentclientprotocol/claude-agent-acp`,
// installed once into Awaken's data directory. Runtime launches the resolved
// absolute bin and never asks npx to install on demand.
const CLAUDE: AcpCli = AcpCli {
    id: "claude",
    display_name: "Claude Code",
    description: "Claude Code via the pinned ACP adapter. Reads CLAUDE.md.",
    acquisition: AcpAcquisition::PinnedNpmWrapper {
        installer: "npm",
        package: "@agentclientprotocol/claude-agent-acp@0.64.2",
        bin: "claude-agent-acp",
    },
    discovery: AcpDiscoverySpec {
        version: AcpProbeCommand {
            executable: "claude",
            args: &["--version"],
        },
        login: AcpLoginProbe {
            command: AcpProbeCommand {
                executable: "claude",
                args: &["auth", "status", "--json"],
            },
            rules: &[
                AcpLoginRule {
                    predicate: AcpProbePredicate::StdoutJsonBoolean {
                        field: "loggedIn",
                        value: true,
                    },
                    state: CredentialObservationState::Available,
                    reason_code: "acp_login_available",
                },
                AcpLoginRule {
                    predicate: AcpProbePredicate::StdoutJsonBoolean {
                        field: "loggedIn",
                        value: false,
                    },
                    state: CredentialObservationState::LoginRequired,
                    reason_code: "acp_login_required",
                },
            ],
            remediation: "Run `claude auth login` and complete the Claude Code login flow.",
        },
        install_remediation: "Install Claude Code and Node.js/npm, then rerun discovery.",
    },
    image_requirements: &[
        AcpImageRequirement {
            manager: "npm",
            requirement: "@agentclientprotocol/claude-agent-acp@0.64.2",
        },
        AcpImageRequirement {
            manager: "npm",
            requirement: "@anthropic-ai/claude-code@2.1.221",
        },
    ],
    container_argv: &["claude-agent-acp"],
    container_probe_argv: None,
    capability_probe_auth_method_id: None,
    model_delivery: Some(ModelDelivery {
        base_url: "ANTHROPIC_BASE_URL",
        model: "ANTHROPIC_MODEL",
        credential_env: &["ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"],
        aliases: &[
            "ANTHROPIC_SONNET_MODEL",
            "ANTHROPIC_OPUS_MODEL",
            "ANTHROPIC_HAIKU_MODEL",
        ],
    }),
    model_api_dialects: &["anthropic_messages"],
    backend_model_interface: BackendModelInterface::ConfigOverride {
        flag: "-c",
        key: "model",
    },
    managed_model_interface: ManagedModelInterface::Environment,
    managed_credential_delivery: ManagedCredentialDelivery::ProcessSecret,
    managed_provider_config: None,
    auth_method_id: None,
    mcp_interface: McpInterface::AcpSession,
    config_home_env: Some("CLAUDE_CONFIG_DIR"),
    config_home_aliases: &[],
    memory_entrypoint: "CLAUDE.md",
    session_export_excludes: &[".credentials.json", "settings.json"],
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
        installer: "npm",
        package: "@agentclientprotocol/codex-acp@1.1.9",
        bin: "codex-acp",
    },
    discovery: AcpDiscoverySpec {
        version: AcpProbeCommand {
            executable: "codex",
            args: &["--version"],
        },
        login: AcpLoginProbe {
            command: AcpProbeCommand {
                executable: "codex",
                args: &["login", "status"],
            },
            rules: &[
                AcpLoginRule {
                    predicate: AcpProbePredicate::CombinedOutputContains("not logged in"),
                    state: CredentialObservationState::LoginRequired,
                    reason_code: "acp_login_required",
                },
                AcpLoginRule {
                    predicate: AcpProbePredicate::ExitSuccess,
                    state: CredentialObservationState::Available,
                    reason_code: "acp_login_available",
                },
            ],
            remediation: "Run `codex login` and complete the Codex login flow.",
        },
        install_remediation: "Install Codex and Node.js/npm, then rerun discovery.",
    },
    image_requirements: &[
        AcpImageRequirement {
            manager: "npm",
            requirement: "@agentclientprotocol/codex-acp@1.1.9",
        },
        AcpImageRequirement {
            manager: "npm",
            requirement: "@openai/codex@0.146.0",
        },
    ],
    container_argv: &["codex-acp"],
    container_probe_argv: Some(&[
        "/usr/bin/env",
        "OPENAI_API_KEY=awaken-capability-probe",
        "codex-acp",
    ]),
    capability_probe_auth_method_id: Some("api-key"),
    model_delivery: Some(ModelDelivery {
        base_url: "OPENAI_BASE_URL",
        model: "OPENAI_MODEL",
        credential_env: &["OPENAI_API_KEY"],
        aliases: &[],
    }),
    model_api_dialects: &["open_ai_responses"],
    backend_model_interface: BackendModelInterface::SessionConfigOption { config_id: "model" },
    managed_model_interface: ManagedModelInterface::Environment,
    managed_credential_delivery: ManagedCredentialDelivery::Artifact(CredentialArtifactSpec {
        codec: CredentialArtifactCodec::CodexAuthJson,
        relative_path: ".codex/auth.json",
    }),
    managed_provider_config: Some(ManagedProviderConfigDelivery {
        config_env: "CODEX_CONFIG",
        provider_env: "MODEL_PROVIDER",
        provider_id: "awaken-managed",
        wire_api: "responses",
        requires_openai_auth: true,
    }),
    auth_method_id: None,
    mcp_interface: McpInterface::AcpSession,
    config_home_env: None,
    config_home_aliases: &[],
    memory_entrypoint: "AGENTS.md",
    session_export_excludes: &[],
    // Codex writes rollout files under `sessions/`, keyed by an internal id.
    session_persistence: SessionPersistence::None,
    context_window_env: None,
    env: &[],
};

// Gemini CLI speaks ACP natively via `--acp` (no npm wrapper), so it
// is a Direct launch with no dynamic-install step.
const GEMINI: AcpCli = AcpCli {
    id: "gemini",
    display_name: "Gemini CLI",
    description: "Gemini CLI via its native ACP mode. Reads GEMINI.md.",
    acquisition: AcpAcquisition::Direct {
        executable: "gemini",
        args: &["--acp"],
    },
    discovery: AcpDiscoverySpec {
        version: AcpProbeCommand {
            executable: "gemini",
            args: &["--version"],
        },
        login: AcpLoginProbe {
            // Listing local sessions is non-interactive and makes Gemini validate
            // its own selected auth method without issuing a model request.
            command: AcpProbeCommand {
                executable: "gemini",
                args: &["--list-sessions"],
            },
            rules: &[
                AcpLoginRule {
                    // Gemini documents 41 as FatalAuthenticationError.
                    predicate: AcpProbePredicate::ExitCode(41),
                    state: CredentialObservationState::LoginRequired,
                    reason_code: "acp_login_required",
                },
                AcpLoginRule {
                    predicate: AcpProbePredicate::ExitSuccess,
                    state: CredentialObservationState::Available,
                    reason_code: "acp_login_available",
                },
            ],
            remediation: "Run `gemini` and complete the Gemini CLI login flow.",
        },
        install_remediation: "Install Gemini CLI, then rerun discovery.",
    },
    image_requirements: &[AcpImageRequirement {
        manager: "npm",
        requirement: "@google/gemini-cli@0.53.1",
    }],
    container_argv: &["gemini", "--acp"],
    container_probe_argv: Some(&[
        "/usr/bin/env",
        "GEMINI_API_KEY=awaken-capability-probe",
        "gemini",
        "--acp",
    ]),
    // The probe key is already selected before process launch. Gemini 0.53 can
    // deadlock when `authenticate` is sent only after `initialize` completes;
    // opening the prompt-free Session directly uses the exact same key without
    // adding a second protocol-side selection step.
    capability_probe_auth_method_id: None,
    model_delivery: Some(ModelDelivery {
        base_url: "GOOGLE_GEMINI_BASE_URL",
        model: "GEMINI_MODEL",
        credential_env: &["GEMINI_API_KEY"],
        aliases: &[],
    }),
    model_api_dialects: &["gemini"],
    backend_model_interface: BackendModelInterface::Flag { flag: "--model" },
    managed_model_interface: ManagedModelInterface::Environment,
    managed_credential_delivery: ManagedCredentialDelivery::ProcessSecret,
    managed_provider_config: None,
    auth_method_id: None,
    mcp_interface: McpInterface::AcpSession,
    config_home_env: Some("GEMINI_DIR"),
    config_home_aliases: &[],
    memory_entrypoint: "GEMINI.md",
    session_export_excludes: &[],
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
    discovery: AcpDiscoverySpec {
        version: AcpProbeCommand {
            executable: "opencode",
            args: &["--version"],
        },
        login: AcpLoginProbe {
            command: AcpProbeCommand {
                executable: "opencode",
                args: &["auth", "list"],
            },
            rules: &[
                AcpLoginRule {
                    predicate: AcpProbePredicate::CombinedOutputContains("0 credentials"),
                    state: CredentialObservationState::LoginRequired,
                    reason_code: "acp_login_required",
                },
                AcpLoginRule {
                    predicate: AcpProbePredicate::ExitSuccess,
                    state: CredentialObservationState::Available,
                    reason_code: "acp_login_available",
                },
            ],
            remediation: "Run `opencode auth login` and complete the OpenCode login flow.",
        },
        install_remediation: "Install OpenCode, then rerun discovery.",
    },
    image_requirements: &[AcpImageRequirement {
        manager: "npm",
        requirement: "opencode-ai@1.18.12",
    }],
    container_argv: &["opencode", "acp"],
    container_probe_argv: None,
    capability_probe_auth_method_id: None,
    model_delivery: Some(ModelDelivery {
        base_url: "OPENAI_BASE_URL",
        model: "OPENAI_MODEL",
        credential_env: &["OPENAI_API_KEY"],
        aliases: &[],
    }),
    model_api_dialects: &["open_ai_chat"],
    backend_model_interface: BackendModelInterface::Unsupported,
    managed_model_interface: ManagedModelInterface::Environment,
    managed_credential_delivery: ManagedCredentialDelivery::ProcessSecret,
    managed_provider_config: None,
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
    session_export_excludes: &["auth.json"],
    // opencode keeps conversation state in a local store, keyed by an internal id
    // (provisional subtree — confirm the exact path by capability probe).
    session_persistence: SessionPersistence::LocalDir {
        session_subpath: "storage",
        keyed_by: SessionKey::InternalId,
    },
    context_window_env: None,
    env: &[],
};

// Hermes exposes a native ACP stdio entrypoint. Its DeepSeek provider accepts
// the same managed endpoint, model, and process-secret projection as the other
// provider-agnostic ACP rows, without requiring interactive profile setup.
const HERMES: AcpCli = AcpCli {
    id: "hermes",
    display_name: "Hermes Agent",
    description: "Hermes Agent via its native ACP mode. Reads AGENTS.md.",
    acquisition: AcpAcquisition::Direct {
        executable: "hermes-acp",
        args: &[],
    },
    discovery: AcpDiscoverySpec {
        version: AcpProbeCommand {
            executable: "hermes",
            args: &["version"],
        },
        login: AcpLoginProbe {
            command: AcpProbeCommand {
                executable: "hermes",
                args: &["config", "check"],
            },
            rules: &[AcpLoginRule {
                predicate: AcpProbePredicate::ExitSuccess,
                state: CredentialObservationState::Available,
                reason_code: "acp_login_available",
            }],
            remediation: "Configure a Hermes provider or attach a managed model credential.",
        },
        install_remediation: "Install hermes-agent with its ACP extra, then rerun discovery.",
    },
    image_requirements: &[AcpImageRequirement {
        manager: "pip",
        requirement: "hermes-agent[acp,bedrock]==0.19.0",
    }],
    container_argv: &["hermes-acp"],
    container_probe_argv: Some(&[
        "/usr/bin/env",
        "DEEPSEEK_API_KEY=awaken-capability-probe",
        // Capability discovery is prompt-free and credential-free. Point live
        // model-list discovery at a closed local port so a fresh or egress-
        // fenced Worker falls back to the image-owned catalog immediately.
        "DEEPSEEK_BASE_URL=http://127.0.0.1:9/v1",
        "HERMES_MODEL=deepseek-v4-flash",
        "hermes-acp",
    ]),
    capability_probe_auth_method_id: None,
    model_delivery: Some(ModelDelivery {
        base_url: "DEEPSEEK_BASE_URL",
        model: "HERMES_MODEL",
        credential_env: &["DEEPSEEK_API_KEY"],
        aliases: &[],
    }),
    model_api_dialects: &["open_ai_chat"],
    backend_model_interface: BackendModelInterface::Unsupported,
    managed_model_interface: ManagedModelInterface::SessionModel,
    managed_credential_delivery: ManagedCredentialDelivery::ProcessSecret,
    managed_provider_config: None,
    auth_method_id: None,
    mcp_interface: McpInterface::AcpSession,
    config_home_env: Some("HERMES_HOME"),
    config_home_aliases: &[],
    memory_entrypoint: "AGENTS.md",
    session_export_excludes: &[".env"],
    session_persistence: SessionPersistence::None,
    context_window_env: Some("HERMES_CONTEXT_WINDOW"),
    env: &[],
};

/// The known ACP CLIs. Adding one is a row here — never a branch elsewhere.
#[must_use]
pub fn known_acp_clis() -> &'static [AcpCli] {
    &[CLAUDE, CODEX, GEMINI, OPENCODE, HERMES]
}
