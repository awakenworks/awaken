//! The **ACP CLI catalog**: one data row per external coding-agent CLI (Claude
//! Code, Codex, Gemini…) describing how to launch it and how the resolved model
//! reaches it. Pure data + a projection function — no `match adapter_kind` anywhere
//! (adding a CLI is a row, not a branch). A row is a *catalog binding* an agent
//! references by id (`Backend::Acp(cli)`), the same category as a model provider
//! — never agent config itself.

use crate::discovery_spec::{
    AcpDiscoverySpec, AcpLoginProbe, AcpLoginRule, AcpProbeCommand, AcpProbePredicate, AcpVersion,
};
use crate::{AcpLaunch, AcpLaunchIdentity, OpenError};
use awaken_provisioning_contract as pc;
use awaken_runtime_contract::{CredentialObservationState, resolved::BackendModelSelection};

mod acquisition;
mod catalog;
mod image_contract;
mod managed_delivery;
mod publication;
pub use acquisition::AcpAcquisition;
pub use catalog::known_acp_clis;
pub use image_contract::{AcpImageRequirement, image_runtime_contract_json};
use managed_delivery::project_acp_session;
pub use managed_delivery::{
    CredentialArtifactCodec, CredentialArtifactSpec, ManagedCredentialDelivery,
    ManagedProviderConfigCodec, ManagedProviderConfigDelivery, ModelDelivery,
};
pub use publication::known_acp_publication_capabilities;

/// How a backend-owned CLI accepts an exact model selection. This is distinct
/// from [`ModelDelivery`]: it never carries an endpoint or credential and is
/// consulted only for [`ResolvedModel::BackendOwned`]. Adding an ACP client is a
/// catalog row, not another launch branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendModelInterface {
    /// Generic CLI config override such as `-c model="..."`.
    ConfigOverride {
        flag: &'static str,
        key: &'static str,
    },
    /// Ordinary model flag such as `--model <id>`.
    Flag { flag: &'static str },
    /// ACP `session/set_config_option` after opening the session.
    SessionConfigOption { config_id: &'static str },
    /// The CLI can use its own default, but Awaken cannot guarantee an exact id.
    Unsupported,
}

/// Where a backend-owned CLI writes mutable process state while retaining the
/// host user's login and configuration identity.
///
/// This is deliberately narrower than [`AcpCli::config_home_env`]: moving the
/// whole config home would also move credentials and user configuration. A CLI
/// selects Session isolation only when it exposes a dedicated non-secret state
/// directory such as Codex's SQLite home.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendOwnedStateIsolation {
    /// The CLI has no independently configurable state directory, so its state
    /// remains part of the trusted host identity.
    SharedHost,
    /// Point the declared environment variable at the existing Session-owned
    /// config directory. The Runtime Host owns realization and teardown.
    SessionDirectory { env: &'static str },
}

/// How an ACP CLI receives the managed route's exact model id. Most adapters
/// read it from their catalog-declared environment projection; adapters that
/// own model selection at the protocol layer receive `session/set_model` after
/// the Session is opened. This is catalog data, never an adapter branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedModelInterface {
    Environment,
    SessionModel,
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
    /// config to exclude are the row's [`AcpCli::session_export_excludes`].
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

/// Pure admission kernel used by [`AcpCli::admits_mcp_client_credential`]. The
/// adapter declaration comes from its authoritative catalog row; accepting an
/// ordinary secret-free MCP route is intentionally insufficient.
#[must_use]
const fn mcp_client_credential_admitted(
    delivery: Option<awaken_credential_contract::McpCredentialDelivery>,
    adapter_declared: bool,
    http_transport: bool,
) -> bool {
    matches!(
        delivery,
        Some(awaken_credential_contract::McpCredentialDelivery::ClientInjection)
    ) && adapter_declared
        && http_transport
}

#[cfg(kani)]
#[kani::proof]
fn mcp_client_credential_admission_has_no_gateway_or_adapter_fallback() {
    // Causes: C1 delivery is ClientInjection rather than None/Gateway;
    // C2 the catalog row declares client injection; C3 transport is HTTP.
    // Effects: E1 admits iff C1+C2+C3; E2 rejects every other combination.
    // Constraint: ACP Session is the sole production MCP delivery path, so
    // adapter identity and a legacy config-file fallback are not inputs.
    // Decision rule: exhaust the 3*2*2 input product and prove E1's exact
    // conjunction, which covers every rejecting rule as its complement.
    let delivery = match kani::any::<u8>() % 3 {
        0 => None,
        1 => Some(awaken_credential_contract::McpCredentialDelivery::ClientInjection),
        _ => Some(awaken_credential_contract::McpCredentialDelivery::GatewayMediation),
    };
    let adapter_declared: bool = kani::any();
    let http_transport: bool = kani::any();
    let admitted = mcp_client_credential_admitted(delivery, adapter_declared, http_transport);
    assert_eq!(
        admitted,
        delivery == Some(awaken_credential_contract::McpCredentialDelivery::ClientInjection)
            && adapter_declared
            && http_transport
    );
}

/// The definition of one external ACP CLI: how to launch it and project config onto
/// it. Referenced by `Backend::Acp(cli)` via [`AcpCli::id`].
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
    /// Host installation, version, and provider-owned login probes.
    pub discovery: AcpDiscoverySpec,
    /// Exact packages required by the production execution image. The image
    /// contract generator combines this with the row's canonical argv/auth data.
    pub image_requirements: &'static [AcpImageRequirement],
    /// Equivalent argv for a worker image where the adapter is preinstalled. Keeping
    /// this in the catalog row avoids both runtime package downloads and adapter
    /// branches in the container mechanism.
    pub container_argv: &'static [&'static str],
    /// Optional credential-free argv used only by the prompt-free image
    /// capability probe. It must contain no real credential material and never
    /// participates in an Agent launch.
    pub container_probe_argv: Option<&'static [&'static str]>,
    /// Probe-only ACP authentication method. This is separate from the launch
    /// method because a real session may already have an Awaken-materialized
    /// credential artifact while a capability probe uses a non-secret sentinel.
    pub capability_probe_auth_method_id: Option<&'static str>,
    /// Environment projection used only by legacy API-key adapters. `None`
    /// means the adapter requires its provider-specific credential driver.
    pub model_delivery: Option<ModelDelivery>,
    /// Model API dialects this executor can consume when Awaken supplies a
    /// provider route. Stable protocol tokens keep this runtime catalog
    /// independent from the control-plane catalog crate.
    pub model_api_dialects: &'static [&'static str],
    /// Exact-model interface for backend-owned local login. Default-model
    /// selection never consumes it.
    pub backend_model_interface: BackendModelInterface,
    /// Mutable-state isolation for backend-owned local login. This never moves
    /// the CLI's credential or user configuration home.
    pub backend_owned_state_isolation: BackendOwnedStateIsolation,
    /// Exact-model interface for an Awaken-managed provider route.
    pub managed_model_interface: ManagedModelInterface,
    /// Managed provider credential delivery. Local backend-owned login is a
    /// separate provisioning mode and does not consume this field.
    pub managed_credential_delivery: ManagedCredentialDelivery,
    /// Optional non-secret adapter config needed to select a managed provider.
    /// This is route projection, not credential delivery.
    pub managed_provider_config: Option<ManagedProviderConfigDelivery>,
    /// ACP authentication method selected after initialize, when the adapter
    /// exposes more than one protocol-level method.
    pub auth_method_id: Option<&'static str>,
    /// Whether this adapter has been verified to accept an HTTP MCP credential
    /// through the process-private ACP `session/new` field. Ordinary MCP route
    /// support does not imply this capability.
    pub mcp_client_credential_injection: bool,
    /// Env key naming the CLI's isolated config directory (e.g. `CLAUDE_CONFIG_DIR`).
    pub config_home_env: Option<&'static str>,
    /// Additional standard/vendor home variables that point at the same isolated
    /// root. Some CLIs split extensions from global config/data/cache state.
    pub config_home_aliases: &'static [&'static str],
    /// The memory file the CLI reads from its config home (e.g. `CLAUDE.md`).
    pub memory_entrypoint: &'static str,
    /// Credential and machine-local config paths excluded when exporting the
    /// portable session-home. This is not a persistence/include list.
    pub session_export_excludes: &'static [&'static str],
    /// Where this CLI keeps its durable session, deciding cross-directory /
    /// cross-machine recovery (see [`SessionPersistence`]).
    pub session_persistence: SessionPersistence,
    /// Env key for the CLI's own auto-compaction window, if it exposes one.
    pub context_window_env: Option<&'static str>,
    /// Static non-secret env defaults for this CLI (lowest precedence).
    pub env: &'static [(&'static str, &'static str)],
}

impl AcpCli {
    /// Whether this exact adapter row admits a process-private MCP credential
    /// on its typed ACP `session/new` HTTP field.
    #[must_use]
    pub fn admits_mcp_client_credential(
        &self,
        delivery: Option<awaken_credential_contract::McpCredentialDelivery>,
        http_transport: bool,
    ) -> bool {
        mcp_client_credential_admitted(
            delivery,
            self.mcp_client_credential_injection,
            http_transport,
        )
    }

    /// Operator remediation for one canonical discovery reason. The catalog row
    /// owns adapter-specific commands; diagnostics and capabilities merely
    /// project them and therefore cannot drift.
    #[must_use]
    pub fn remediation(self, reason_code: Option<&str>) -> Option<&'static str> {
        match reason_code? {
            "acp_agent_missing" => Some(self.discovery.install_remediation),
            "acp_login_required" => Some(self.discovery.login.remediation),
            "acp_version_probe_failed"
            | "acp_version_unsupported"
            | "acp_login_probe_failed"
            | "acp_login_probe_unrecognized"
            | "acp_wrapper_install_failed" => {
                Some("Run `awaken doctor acp` after checking the CLI installation and login.")
            }
            _ => None,
        }
    }
}

/// One already-selected process-secret requirement. The opaque reference is
/// resolved only by the final launch provider through `SecretBroker`; this type
/// carries no material, policy, or credential-selection behavior.
#[derive(Clone, PartialEq, Eq)]
pub struct ProcessSecretRequirement {
    reference: String,
    environment_variable: Option<String>,
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
            environment_variable: None,
        }
    }

    #[must_use]
    pub fn for_environment(
        reference: impl Into<String>,
        environment_variable: impl Into<String>,
    ) -> Self {
        Self {
            reference: reference.into(),
            environment_variable: Some(environment_variable.into()),
        }
    }

    #[must_use]
    pub fn reference(&self) -> &str {
        &self.reference
    }

    #[must_use]
    pub fn environment_variable(&self) -> Option<&str> {
        self.environment_variable.as_deref()
    }
}

impl std::fmt::Debug for ProcessSecretRequirement {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ProcessSecretRequirement(***)")
    }
}

/// Mutually exclusive model ownership handed to the launch projection. Managed
/// launches carry endpoint and broker requirements; backend-owned launches carry
/// only an explicit default/exact policy and never have fields for material.
#[derive(Debug, Clone)]
pub enum ResolvedModel {
    Managed {
        base_url: String,
        model: String,
        process_secret: Option<ProcessSecretRequirement>,
        credential_artifact: Option<CredentialArtifactRequirement>,
        acp: Option<awaken_runtime_contract::resolved::AcpExecutionProfile>,
        provider_server_tools: Vec<awaken_runtime_contract::resolved::ProviderServerTool>,
    },
    BackendOwned {
        model_selection: BackendModelSelection,
        model: String,
        capability: awaken_protocol_acp::AcpCapabilityExpectation,
        session_configuration: awaken_runtime_contract::resolved::AcpSessionConfiguration,
    },
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
        Self::Managed {
            base_url: gateway_base_url.into(),
            model: model.into(),
            process_secret: Some(ProcessSecretRequirement::new(lease_reference)),
            credential_artifact: None,
            acp: None,
            provider_server_tools: Vec::new(),
        }
    }

    #[must_use]
    pub fn managed(
        base_url: impl Into<String>,
        model: impl Into<String>,
        process_secret: Option<ProcessSecretRequirement>,
        credential_artifact: Option<CredentialArtifactRequirement>,
    ) -> Self {
        Self::Managed {
            base_url: base_url.into(),
            model: model.into(),
            process_secret,
            credential_artifact,
            acp: None,
            provider_server_tools: Vec::new(),
        }
    }

    #[must_use]
    pub fn managed_with_acp(
        base_url: impl Into<String>,
        model: impl Into<String>,
        process_secret: Option<ProcessSecretRequirement>,
        credential_artifact: Option<CredentialArtifactRequirement>,
        acp: awaken_runtime_contract::resolved::AcpExecutionProfile,
    ) -> Self {
        Self::Managed {
            base_url: base_url.into(),
            model: model.into(),
            process_secret,
            credential_artifact,
            acp: Some(acp),
            provider_server_tools: Vec::new(),
        }
    }

    #[must_use]
    pub fn backend_owned(
        model_selection: BackendModelSelection,
        model: impl Into<String>,
        adapter_id: impl Into<String>,
        adapter_version: impl Into<String>,
        fingerprint: impl Into<String>,
        session_configuration: awaken_runtime_contract::resolved::AcpSessionConfiguration,
    ) -> Self {
        Self::BackendOwned {
            model_selection,
            model: model.into(),
            capability: awaken_protocol_acp::AcpCapabilityExpectation {
                adapter_id: adapter_id.into(),
                adapter_version: adapter_version.into(),
                fingerprint: fingerprint.into(),
            },
            session_configuration,
        }
    }

    #[must_use]
    pub fn credential_artifact(&self) -> Option<&CredentialArtifactRequirement> {
        match self {
            Self::Managed {
                credential_artifact,
                ..
            } => credential_artifact.as_ref(),
            Self::BackendOwned { .. } => None,
        }
    }

    /// Attach the exact provider-owned tools selected by the immutable Agent
    /// publication. The ACP adapter still validates that its CLI, API dialect,
    /// and provider route can realize every entry before launch.
    #[must_use]
    pub fn with_provider_server_tools(
        mut self,
        tools: impl IntoIterator<Item = awaken_runtime_contract::resolved::ProviderServerTool>,
    ) -> Self {
        if let Self::Managed {
            provider_server_tools,
            ..
        } = &mut self
        {
            provider_server_tools.extend(tools);
        }
        self
    }
}

impl AcpCli {
    /// Closed ACP delivery matrix for provider-owned tools. A compatible wire
    /// protocol is insufficient: the adapter must also have an explicit launch
    /// projection below. Today only Codex Responses WebSearch has that path.
    #[must_use]
    pub fn realizes_provider_server_tool(
        &self,
        tool: &awaken_runtime_contract::resolved::ProviderServerTool,
    ) -> bool {
        matches!(
            (self.id, tool),
            (
                "codex",
                awaken_runtime_contract::resolved::ProviderServerTool::OpenAiWebSearch
                    | awaken_runtime_contract::resolved::ProviderServerTool::DeepSeekResponsesWebSearch
            )
        )
    }

    /// Validate an already-selected provider-tool plan against this adapter's
    /// exact model route. Provider identity, wire dialect, and launch support
    /// form one capability contract; OpenAI-compatible naming alone grants
    /// nothing.
    pub fn validate_provider_server_tool_route(
        &self,
        provider_kind: &str,
        api_dialect: &str,
        tools: &[awaken_runtime_contract::resolved::ProviderServerTool],
    ) -> Result<(), OpenError> {
        for tool in tools {
            if tool.provider_kind() != provider_kind {
                return Err(OpenError(format!(
                    "provider_server_tool_mismatch: tool requires `{}` but model route is `{provider_kind}`",
                    tool.provider_kind()
                )));
            }
            if !self.realizes_provider_server_tool(tool) {
                return Err(OpenError(format!(
                    "provider_server_tool_unsupported: ACP `{}` has no verified launch projection for `{}`",
                    self.id,
                    tool.provider_kind()
                )));
            }
            if api_dialect != "open_ai_responses" {
                return Err(OpenError(format!(
                    "provider_server_tool_dialect_mismatch: `{api_dialect}` cannot carry the selected ACP provider tool"
                )));
            }
        }
        Ok(())
    }

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
        self.try_project_with_argv(model, context_window, extra_env, None)
    }

    /// Project with an acquisition-resolved base argv. Product startup uses
    /// this for an absolute, already-installed wrapper path; callers without an
    /// override use the profile's preinstalled binary name.
    pub fn try_project_with_argv(
        &self,
        model: &ResolvedModel,
        context_window: Option<u64>,
        extra_env: &[(String, String)],
        resolved_argv: Option<&[String]>,
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
        let mut argv = resolved_argv
            .map(<[String]>::to_vec)
            .unwrap_or_else(|| self.acquisition.local_argv());
        if argv.is_empty() || argv[0].trim().is_empty() {
            return Err(OpenError(
                "resolved ACP launch argv must not be empty".into(),
            ));
        }
        let mut session_config_options = BTreeMap::new();
        let mut session_mode = None;
        let mut session_working_directory = None;
        if let ResolvedModel::BackendOwned {
            model_selection,
            model,
            capability,
            session_configuration,
        } = model
        {
            session_configuration
                .validate_working_directory()
                .map_err(|reason| OpenError(format!("invalid ACP working directory: {reason}")))?;
            session_mode.clone_from(&session_configuration.mode);
            session_working_directory.clone_from(&session_configuration.working_directory);
            for (config_id, value) in &session_configuration.options {
                session_config_options.insert(config_id.clone(), value.clone());
            }
            // A retained managed projection cannot shadow the CLI-owned account,
            // endpoint, model, or home. Production supplies no extra env for this
            // mode; stripping catalog-known keys makes the boundary fail safe for
            // alternate resolvers and stale snapshots too.
            if let Some(delivery) = self.model_delivery {
                for key in std::iter::once(delivery.base_url)
                    .chain(std::iter::once(delivery.model))
                    .chain(delivery.credential_env.iter().copied())
                    .chain(delivery.aliases.iter().copied())
                {
                    env.remove(key);
                }
            }
            if let Some(delivery) = self.managed_provider_config {
                env.remove(delivery.config_env);
                if !delivery.provider_env.is_empty() {
                    env.remove(delivery.provider_env);
                }
            }
            for key in self
                .config_home_env
                .into_iter()
                .chain(self.config_home_aliases.iter().copied())
                .chain(self.context_window_env)
            {
                env.remove(key);
            }
            match model_selection {
                BackendModelSelection::Default if !model.is_empty() => {
                    return Err(OpenError(
                        "backend_default_model_must_not_carry_an_exact_id".into(),
                    ));
                }
                BackendModelSelection::Default => {}
                BackendModelSelection::Exact if model.trim().is_empty() => {
                    return Err(OpenError("backend_exact_model_id_required".into()));
                }
                BackendModelSelection::Exact => match self.backend_model_interface {
                    BackendModelInterface::ConfigOverride { flag, key } => {
                        argv.push(flag.to_string());
                        argv.push(format!("{key}={model:?}"));
                    }
                    BackendModelInterface::Flag { flag } => {
                        argv.push(flag.to_string());
                        argv.push(model.clone());
                    }
                    BackendModelInterface::SessionConfigOption { config_id } => {
                        match session_config_options.get(config_id) {
                            Some(value) if value != model => {
                                return Err(OpenError(format!(
                                    "backend model conflicts with ACP option `{config_id}`"
                                )));
                            }
                            Some(_) => {}
                            None => {
                                session_config_options.insert(config_id.to_string(), model.clone());
                            }
                        }
                    }
                    BackendModelInterface::Unsupported => {
                        return Err(OpenError(format!(
                            "backend_exact_model_unsupported: {}",
                            self.id
                        )));
                    }
                },
            }
            return Ok(AcpLaunch {
                argv,
                env: env.into_values().collect(),
                identity: AcpLaunchIdentity::BackendOwned,
                session_model: None,
                session_mode,
                session_config_options: session_config_options
                    .into_iter()
                    .map(
                        |(config_id, value)| awaken_protocol_acp::SessionConfigOptionSelection {
                            config_id,
                            value,
                        },
                    )
                    .collect(),
                session_working_directory,
                expected_capability: Some(capability.clone()),
            });
        }

        let ResolvedModel::Managed {
            base_url,
            model,
            process_secret,
            credential_artifact,
            acp,
            provider_server_tools,
        } = model
        else {
            unreachable!("backend-owned launch returned above")
        };
        let d = self.model_delivery.as_ref();
        if let Some(unsupported) = provider_server_tools
            .iter()
            .find(|tool| !self.realizes_provider_server_tool(tool))
        {
            return Err(OpenError(format!(
                "provider_server_tool_unsupported: {} cannot realize {}",
                self.id,
                unsupported.provider_kind()
            )));
        }
        let native_web_search = provider_server_tools.iter().any(|tool| {
            matches!(
                tool,
                awaken_runtime_contract::resolved::ProviderServerTool::OpenAiWebSearch
                    | awaken_runtime_contract::resolved::ProviderServerTool::DeepSeekResponsesWebSearch
            )
        });
        let (session_mode, session_config_options, expected_capability) =
            project_acp_session(self.id, acp.as_ref());
        if let Some(profile) = acp.as_ref() {
            profile
                .session_configuration
                .validate_working_directory()
                .map_err(|reason| OpenError(format!("invalid ACP working directory: {reason}")))?;
        }
        let session_working_directory = acp
            .as_ref()
            .and_then(|profile| profile.session_configuration.working_directory.clone());
        let session_model = match self.managed_model_interface {
            ManagedModelInterface::Environment => None,
            ManagedModelInterface::SessionModel if model.trim().is_empty() => {
                return Err(OpenError("managed_session_model_id_required".into()));
            }
            ManagedModelInterface::SessionModel => Some(model.clone()),
        };
        if process_secret.is_some() && !self.managed_credential_delivery.allows_process_secret() {
            return Err(OpenError(format!(
                "credential_delivery_mismatch: {} requires a managed artifact",
                self.id
            )));
        }
        if credential_artifact.is_some() && self.managed_credential_delivery.allows_process_secret()
        {
            return Err(OpenError(format!(
                "credential_delivery_mismatch: {} requires a process secret",
                self.id
            )));
        }
        if d.is_none() && credential_artifact.is_none() {
            return Err(OpenError(format!(
                "credential_driver_required: {}",
                self.id
            )));
        }
        let Some(d) = d else {
            return Ok(AcpLaunch {
                argv,
                env: env.into_values().collect(),
                identity: AcpLaunchIdentity::Managed,
                session_model,
                session_mode,
                session_config_options,
                session_working_directory,
                expected_capability,
            });
        };
        if !base_url.is_empty() {
            env.insert(d.base_url.to_string(), inline(d.base_url, base_url.clone()));
        }
        if !model.is_empty() {
            env.insert(d.model.to_string(), inline(d.model, model.clone()));
            for alias in d.aliases {
                env.insert((*alias).to_string(), inline(alias, model.clone()));
            }
        }
        if let Some(config) = self.managed_provider_config {
            let document = match config.codec {
                ManagedProviderConfigCodec::Codex => {
                    let mut providers = serde_json::Map::new();
                    providers.insert(
                        config.provider_id.to_string(),
                        serde_json::json!({
                            "name": "Awaken managed provider",
                            "base_url": base_url,
                            "wire_api": config.wire_api,
                            "requires_openai_auth": config.requires_openai_auth,
                            "supports_standalone_web_search": native_web_search,
                        }),
                    );
                    let mut document = serde_json::json!({
                        "model": model,
                        "model_provider": config.provider_id,
                        "model_providers": providers,
                    });
                    if native_web_search {
                        document["web_search"] = serde_json::json!("live");
                    }
                    document
                }
                ManagedProviderConfigCodec::OpenCode {
                    provider_package,
                    credential_env,
                } => {
                    let mut models = serde_json::Map::new();
                    models.insert(model.clone(), serde_json::json!({"name": model}));
                    let mut providers = serde_json::Map::new();
                    providers.insert(
                        config.provider_id.to_string(),
                        serde_json::json!({
                            "npm": provider_package,
                            "name": "Awaken managed provider",
                            "options": {
                                "baseURL": base_url,
                                "apiKey": format!("{{env:{credential_env}}}"),
                            },
                            "models": models,
                        }),
                    );
                    serde_json::json!({
                        "model": format!("{}/{}", config.provider_id, model),
                        "provider": providers,
                    })
                }
            };
            let serialized = serde_json::to_string(&document)
                .map_err(|error| OpenError(format!("managed_provider_config_invalid: {error}")))?;
            env.insert(
                config.config_env.to_string(),
                inline(config.config_env, serialized),
            );
            if !config.provider_env.is_empty() {
                env.insert(
                    config.provider_env.to_string(),
                    inline(config.provider_env, config.provider_id.to_string()),
                );
            }
        }
        if let (Some(key), Some(window)) = (self.context_window_env, context_window) {
            env.insert(key.to_string(), inline(key, window.to_string()));
        }
        // The typed secret goes last so no passthrough key can shadow it.
        if let Some(secret) = process_secret {
            let key = match secret.environment_variable() {
                Some(key) if d.supports_credential_env(key) => key,
                Some(key) => {
                    return Err(OpenError(format!(
                        "credential_environment_unsupported: {} does not accept {key}",
                        self.id
                    )));
                }
                None => d.default_credential_env().ok_or_else(|| {
                    OpenError(format!(
                        "credential_environment_missing: {} has no process-secret environment",
                        self.id
                    ))
                })?,
            };
            env.insert(
                key.to_string(),
                pc::EnvVar {
                    name: key.to_string(),
                    value: pc::EnvValue::Secret {
                        reference: secret.reference().to_string(),
                    },
                    visibility: pc::EnvVisibility::Process,
                },
            );
        }

        Ok(AcpLaunch {
            argv,
            env: env.into_values().collect(),
            identity: AcpLaunchIdentity::Managed,
            session_model,
            session_mode,
            session_config_options,
            session_working_directory,
            expected_capability,
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

/// Resolve an ACP CLI by id (`Backend::Acp(cli)`); `None` is a fail-closed
/// "unknown CLI" the caller rejects (never a silent default).
#[must_use]
pub fn acp_cli(id: &str) -> Option<&'static AcpCli> {
    known_acp_clis().iter().find(|c| c.id == id)
}

#[cfg(test)]
#[path = "acp_cli/tests.rs"]
mod tests;
