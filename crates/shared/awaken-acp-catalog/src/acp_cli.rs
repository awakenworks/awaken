// @formal entrypoint credential.mcp_client_admission_requires_exact_delivery_adapter_and_http_transport
// @formal entrypoint credential.mcp_delivery_mode_is_frozen_and_never_falls_back
//! The **ACP CLI catalog**: one data row per external coding-agent CLI (Claude
//! Code, Codex, Gemini…) describing how to launch it and how the resolved model
//! reaches it. Pure data + a projection function — no `match adapter_kind` anywhere
//! (adding a CLI is a row, not a branch). A row is a *catalog binding* an agent
//! references by id (`Backend::Acp(cli)`), the same category as a model provider
//! — never agent config itself.

use crate::discovery_spec::{
    AcpDiscoverySpec, AcpLoginProbe, AcpLoginRule, AcpProbeCommand, AcpProbePredicate, AcpVersion,
};
use awaken_credential_contract::CredentialObservationState;

mod acquisition;
mod catalog;
mod image_contract;
mod managed_delivery;
mod publication;
pub use acquisition::AcpAcquisition;
pub use catalog::known_acp_clis;
pub use image_contract::{AcpImageRequirement, image_runtime_contract_json};
pub use managed_delivery::{
    CredentialArtifactCodec, CredentialArtifactRequirement, CredentialArtifactSpec,
    ManagedCredentialDelivery, ManagedProviderConfigCodec, ManagedProviderConfigDelivery,
    ModelDelivery,
};
pub use publication::known_acp_publication_capabilities;

/// How a backend-owned CLI accepts an exact model selection. This is distinct
/// from [`ModelDelivery`]: it never carries an endpoint or credential and is
/// consulted only for `ResolvedModel::BackendOwned`. Adding an ACP client is a
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
/// @obligation credential.mcp_client_admission_requires_exact_delivery_adapter_and_http_transport
/// @obligation credential.mcp_delivery_mode_is_frozen_and_never_falls_back
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
    /// Pinned adapter accepts Claude SDK toolAliases via session metadata.
    pub supports_tool_aliases: bool,
    /// Adapter-owned routes that make the frozen Agent instructions part of
    /// its system prompt (see [`crate::InstructionsDelivery`]).
    pub instructions: crate::InstructionsDelivery,
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
    ) -> Result<(), ProviderServerToolRefusal> {
        for tool in tools {
            if tool.provider_kind() != provider_kind {
                return Err(ProviderServerToolRefusal(format!(
                    "provider_server_tool_mismatch: tool requires `{}` but model route is `{provider_kind}`",
                    tool.provider_kind()
                )));
            }
            if !self.realizes_provider_server_tool(tool) {
                return Err(ProviderServerToolRefusal(format!(
                    "provider_server_tool_unsupported: ACP `{}` has no verified launch projection for `{}`",
                    self.id,
                    tool.provider_kind()
                )));
            }
            if api_dialect != "open_ai_responses" {
                return Err(ProviderServerToolRefusal(format!(
                    "provider_server_tool_dialect_mismatch: `{api_dialect}` cannot carry the selected ACP provider tool"
                )));
            }
        }
        Ok(())
    }
}

/// Why an adapter row cannot carry a selected provider-owned tool on a model
/// route. The message starts with a stable reason code; the executor reports it
/// as the Session open failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderServerToolRefusal(pub String);

impl std::fmt::Display for ProviderServerToolRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProviderServerToolRefusal {}

/// Resolve an ACP CLI by id (`Backend::Acp(cli)`); `None` is a fail-closed
/// "unknown CLI" the caller rejects (never a silent default).
#[must_use]
pub fn acp_cli(id: &str) -> Option<&'static AcpCli> {
    known_acp_clis().iter().find(|c| c.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_client_credential_admission_is_exact_and_has_no_gateway_fallback() {
        use awaken_credential_contract::McpCredentialDelivery;
        // Cause/effect graph: C1 delivery is exact ClientInjection; C2 the
        // selected catalog row declares its private field; C3 transport is
        // HTTP. Effect E1 admits credential material iff C1+C2+C3; otherwise
        // E2 rejects before launch. Constraint: all production MCP delivery is
        // the official ACP Session path, so no interface/fallback cause exists.
        // Decision rules: R1=111=>E1; R2=!C1 (None or Gateway)=>E2;
        // R3=1!1=>E2; R4=11!=>E2. These rules give MC/DC for each cause.
        for (delivery, declared, http, admitted) in [
            (
                Some(McpCredentialDelivery::ClientInjection),
                true,
                true,
                true,
            ),
            (
                Some(McpCredentialDelivery::ClientInjection),
                false,
                true,
                false,
            ),
            (
                Some(McpCredentialDelivery::ClientInjection),
                true,
                false,
                false,
            ),
            (None, true, true, false),
            (
                Some(McpCredentialDelivery::GatewayMediation),
                true,
                true,
                false,
            ),
        ] {
            assert_eq!(
                mcp_client_credential_admitted(delivery, declared, http),
                admitted
            );
        }
    }
}
