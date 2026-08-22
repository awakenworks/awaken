//! Neutral ACP capability negotiation values and port.
//!
//! Worker applications depend on this leaf; protocol adapters implement it.
//! The contract owns no process, repository, discovery or JSON-RPC behavior.

use std::path::Path;

use async_trait::async_trait;
use awaken_agent_channel::AgentChannel;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NegotiatedAcpCapabilities {
    pub protocol_version: String,
    pub load_session: bool,
    pub prompt_image: bool,
    pub prompt_audio: bool,
    pub prompt_embedded_context: bool,
    pub mcp_http: bool,
    pub mcp_sse: bool,
    pub session_list: bool,
    pub modes: Vec<AcpSessionModeDescriptor>,
    pub config_options: Vec<AcpSessionConfigOptionDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpSessionModeDescriptor {
    pub native_id: String,
    pub name: String,
    pub description: Option<String>,
    pub current: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpSessionConfigOptionDescriptor {
    pub native_id: String,
    pub name: String,
    pub description: Option<String>,
    pub category: Option<String>,
    pub current_value: String,
    pub choices: Vec<AcpSessionConfigChoice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpSessionConfigChoice {
    pub native_value: String,
    pub name: String,
    pub description: Option<String>,
    pub group_id: Option<String>,
    pub group_name: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcpCapabilityProbeConfig {
    pub session_cwd: Option<String>,
    pub auth_method_id: Option<String>,
}

#[async_trait]
pub trait AcpCapabilityHandshake: Send + Sync {
    async fn negotiate(
        &self,
        channel: &mut dyn AgentChannel,
        config: &AcpCapabilityProbeConfig,
    ) -> Result<NegotiatedAcpCapabilities, String>;
}

/// Environment-neutral orchestration port for one bounded capability probe.
/// Host-process and Session-environment adapters implement the same contract;
/// the application service consumes it without owning either process boundary.
#[async_trait]
pub trait AcpCapabilityNegotiator: Send + Sync {
    async fn negotiate(
        &self,
        argv: &[String],
        cwd: &Path,
        auth_method_id: Option<&str>,
    ) -> Result<NegotiatedAcpCapabilities, String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpCapabilityObservationState {
    Verified,
    Unavailable,
    ProbeFailed,
}

/// Whether one live observation is sufficient evidence to advertise an ACP
/// runtime as detected. A manifest or failed/missing probe is never evidence.
#[must_use]
pub const fn has_verified_capability_observation(
    state: Option<AcpCapabilityObservationState>,
) -> bool {
    matches!(state, Some(AcpCapabilityObservationState::Verified))
}

#[cfg(kani)]
#[kani::proof]
fn acp_capability_is_detected_exactly_after_a_verified_observation() {
    let present: bool = kani::any();
    let verified: bool = kani::any();
    let probe_failed: bool = kani::any();
    let state = if !present {
        None
    } else if verified {
        Some(AcpCapabilityObservationState::Verified)
    } else if probe_failed {
        Some(AcpCapabilityObservationState::ProbeFailed)
    } else {
        Some(AcpCapabilityObservationState::Unavailable)
    };

    assert_eq!(
        has_verified_capability_observation(state),
        present && verified
    );
}

/// Point-in-time, secret-free capability evidence from one Worker-local ACP.
///
/// ```compile_fail
/// use awaken_acp_contract::{AcpCapabilityObservation, AcpCapabilityObservationState};
/// let _ = AcpCapabilityObservation {
///     backend_ref: "acp:codex".into(),
///     adapter_version: "1".into(),
///     state: AcpCapabilityObservationState::Verified,
///     observed_at_ms: 0,
///     fingerprint: None,
///     negotiated: None,
///     reason_code: None,
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AcpCapabilityObservation {
    backend_ref: String,
    adapter_version: String,
    state: AcpCapabilityObservationState,
    observed_at_ms: u64,
    fingerprint: Option<String>,
    negotiated: Option<NegotiatedAcpCapabilities>,
    reason_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidAcpCapabilityObservation(&'static str);

impl std::fmt::Display for InvalidAcpCapabilityObservation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "invalid ACP capability observation: {}", self.0)
    }
}

impl std::error::Error for InvalidAcpCapabilityObservation {}

#[derive(Deserialize)]
struct AcpCapabilityObservationWire {
    backend_ref: String,
    adapter_version: String,
    state: AcpCapabilityObservationState,
    observed_at_ms: u64,
    fingerprint: Option<String>,
    negotiated: Option<NegotiatedAcpCapabilities>,
    reason_code: Option<String>,
}

impl<'de> Deserialize<'de> for AcpCapabilityObservation {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = AcpCapabilityObservationWire::deserialize(deserializer)?;
        Self::try_from_parts(
            wire.backend_ref,
            wire.adapter_version,
            wire.state,
            wire.observed_at_ms,
            wire.fingerprint,
            wire.negotiated,
            wire.reason_code,
        )
        .map_err(serde::de::Error::custom)
    }
}

impl AcpCapabilityObservation {
    pub fn verified(
        backend_ref: impl Into<String>,
        adapter_version: impl Into<String>,
        observed_at_ms: u64,
        fingerprint: impl Into<String>,
        negotiated: NegotiatedAcpCapabilities,
    ) -> Result<Self, InvalidAcpCapabilityObservation> {
        Self::try_from_parts(
            backend_ref.into(),
            adapter_version.into(),
            AcpCapabilityObservationState::Verified,
            observed_at_ms,
            Some(fingerprint.into()),
            Some(negotiated),
            None,
        )
    }

    pub fn unavailable(
        backend_ref: impl Into<String>,
        adapter_version: impl Into<String>,
        observed_at_ms: u64,
        reason_code: impl Into<String>,
    ) -> Result<Self, InvalidAcpCapabilityObservation> {
        Self::failure(
            backend_ref,
            adapter_version,
            AcpCapabilityObservationState::Unavailable,
            observed_at_ms,
            reason_code,
        )
    }

    pub fn probe_failed(
        backend_ref: impl Into<String>,
        adapter_version: impl Into<String>,
        observed_at_ms: u64,
        reason_code: impl Into<String>,
    ) -> Result<Self, InvalidAcpCapabilityObservation> {
        Self::failure(
            backend_ref,
            adapter_version,
            AcpCapabilityObservationState::ProbeFailed,
            observed_at_ms,
            reason_code,
        )
    }

    fn failure(
        backend_ref: impl Into<String>,
        adapter_version: impl Into<String>,
        state: AcpCapabilityObservationState,
        observed_at_ms: u64,
        reason_code: impl Into<String>,
    ) -> Result<Self, InvalidAcpCapabilityObservation> {
        Self::try_from_parts(
            backend_ref.into(),
            adapter_version.into(),
            state,
            observed_at_ms,
            None,
            None,
            Some(reason_code.into()),
        )
    }

    fn try_from_parts(
        backend_ref: String,
        adapter_version: String,
        state: AcpCapabilityObservationState,
        observed_at_ms: u64,
        fingerprint: Option<String>,
        negotiated: Option<NegotiatedAcpCapabilities>,
        reason_code: Option<String>,
    ) -> Result<Self, InvalidAcpCapabilityObservation> {
        let canonical = |value: &str| !value.is_empty() && value.trim() == value;
        if !backend_ref.strip_prefix("acp:").is_some_and(canonical) || !canonical(&adapter_version)
        {
            return Err(InvalidAcpCapabilityObservation(
                "an exact ACP backend and adapter version are required",
            ));
        }
        let coherent = match state {
            AcpCapabilityObservationState::Verified => {
                fingerprint.as_deref().is_some_and(canonical)
                    && negotiated.is_some()
                    && reason_code.is_none()
            }
            AcpCapabilityObservationState::Unavailable
            | AcpCapabilityObservationState::ProbeFailed => {
                fingerprint.is_none()
                    && negotiated.is_none()
                    && reason_code.as_deref().is_some_and(canonical)
            }
        };
        if !coherent {
            return Err(InvalidAcpCapabilityObservation(
                "state and mutually exclusive evidence disagree",
            ));
        }
        Ok(Self {
            backend_ref,
            adapter_version,
            state,
            observed_at_ms,
            fingerprint,
            negotiated,
            reason_code,
        })
    }

    #[must_use]
    pub fn backend_ref(&self) -> &str {
        &self.backend_ref
    }

    #[must_use]
    pub fn adapter_version(&self) -> &str {
        &self.adapter_version
    }

    #[must_use]
    pub const fn state(&self) -> AcpCapabilityObservationState {
        self.state
    }

    #[must_use]
    pub const fn observed_at_ms(&self) -> u64 {
        self.observed_at_ms
    }

    #[must_use]
    pub fn fingerprint(&self) -> Option<&str> {
        self.fingerprint.as_deref()
    }

    #[must_use]
    pub fn negotiated(&self) -> Option<&NegotiatedAcpCapabilities> {
        self.negotiated.as_ref()
    }

    #[must_use]
    pub fn reason_code(&self) -> Option<&str> {
        self.reason_code.as_deref()
    }

    /// Consume a verified observation into the publication-pinnable evidence.
    /// Failure observations return `None`; coherent construction guarantees a
    /// verified state cannot be missing either value.
    #[must_use]
    pub fn into_verified_evidence(self) -> Option<(String, String, NegotiatedAcpCapabilities)> {
        match (self.state, self.fingerprint, self.negotiated) {
            (AcpCapabilityObservationState::Verified, Some(fingerprint), Some(negotiated)) => {
                Some((self.adapter_version, fingerprint, negotiated))
            }
            _ => None,
        }
    }
}

#[async_trait]
pub trait AcpCapabilityObservationSource: Send + Sync {
    async fn capability_observations(&self) -> Result<Vec<AcpCapabilityObservation>, String>;
}

/// Secret-free static projection of one installed ACP adapter that Control may
/// use while freezing an executable publication. Launch commands and other
/// Worker implementation details never cross this contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpModelSelectionSupport {
    /// The ACP owns model choice. Awaken injects credentials and leaves the
    /// model unset, so the CLI's built-in or Worker-local profile selects it.
    DefaultOnly,
    /// In addition to its own default, the ACP exposes an interface through
    /// which Awaken can require one exact model.
    DefaultAndExact,
}

impl AcpModelSelectionSupport {
    #[must_use]
    pub const fn admits_exact(self) -> bool {
        matches!(self, Self::DefaultAndExact)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpPublicationCapability {
    pub backend_ref: String,
    pub model_api_dialects: Vec<String>,
    pub model_selection: AcpModelSelectionSupport,
    pub model_delivery_credential_environments: Option<Vec<String>>,
}

impl AcpPublicationCapability {
    /// Compile an authored credential environment hint through the adapter's
    /// published allowlist. Absence always means provider-adapter delivery.
    pub fn credential_usage(
        &self,
        environment_hint: Option<&str>,
    ) -> Result<awaken_credential_contract::CredentialUsage, String> {
        let Some(name) = environment_hint else {
            return Ok(awaken_credential_contract::CredentialUsage::ProviderAdapter);
        };
        if !self
            .model_delivery_credential_environments
            .as_ref()
            .is_some_and(|environments| environments.iter().any(|candidate| candidate == name))
        {
            return Err(format!(
                "ACP model delivery does not accept credential environment {name}"
            ));
        }
        Ok(
            awaken_credential_contract::CredentialUsage::EnvironmentVariable {
                name: name.to_string(),
            },
        )
    }
}

/// Canonical executable-capability fingerprint shared by discovery, publication
/// and launch-time handshake verification.
///
/// Session modes, defaults, and config-option catalogues are intentionally
/// excluded: an ACP may derive them from the provisioned provider/model route,
/// so the secret-free Worker probe and the realized Session can legitimately
/// differ. Every requested mode/config selection is still checked against the
/// realized Session before it is sent.
#[must_use]
pub fn capability_fingerprint(
    adapter_id: &str,
    adapter_version: &str,
    capabilities: &NegotiatedAcpCapabilities,
) -> String {
    let mut hash = Sha256::new();
    hash_value(&mut hash, adapter_id);
    hash_value(&mut hash, adapter_version);
    hash_value(&mut hash, &capabilities.protocol_version);
    for flag in [
        capabilities.load_session,
        capabilities.prompt_image,
        capabilities.prompt_audio,
        capabilities.prompt_embedded_context,
        capabilities.mcp_http,
        capabilities.mcp_sse,
        capabilities.session_list,
    ] {
        hash.update([u8::from(flag)]);
    }
    format!("{:x}", hash.finalize())
}

fn hash_value(hash: &mut Sha256, value: &str) {
    hash.update(value.len().to_le_bytes());
    hash.update(value.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capabilities() -> NegotiatedAcpCapabilities {
        NegotiatedAcpCapabilities {
            protocol_version: "1".into(),
            load_session: true,
            prompt_image: true,
            prompt_audio: false,
            prompt_embedded_context: true,
            mcp_http: true,
            mcp_sse: false,
            session_list: true,
            modes: vec![
                AcpSessionModeDescriptor {
                    native_id: "review".into(),
                    name: "Review".into(),
                    description: None,
                    current: false,
                },
                AcpSessionModeDescriptor {
                    native_id: "code".into(),
                    name: "Code".into(),
                    description: Some("write code".into()),
                    current: true,
                },
            ],
            config_options: vec![AcpSessionConfigOptionDescriptor {
                native_id: "model".into(),
                name: "Model".into(),
                description: None,
                category: Some("runtime".into()),
                current_value: "large".into(),
                choices: vec![
                    AcpSessionConfigChoice {
                        native_value: "small".into(),
                        name: "Small".into(),
                        description: None,
                        group_id: Some("size".into()),
                        group_name: Some("Size".into()),
                    },
                    AcpSessionConfigChoice {
                        native_value: "large".into(),
                        name: "Large".into(),
                        description: None,
                        group_id: Some("size".into()),
                        group_name: Some("Size".into()),
                    },
                ],
            }],
        }
    }

    #[test]
    fn capability_fingerprint_decision_table_is_canonical_and_route_stable() {
        // Causes: C1 response order differs; C2 adapter identity differs; C3 a
        // negotiated flag differs; C4 a route-local mode catalogue differs; C5
        // provider routing changes defaults/config catalogues. Effects: E1
        // route-local state is normalized; E2-E3 executable changes differ.
        //
        // | Rule | C1 | C2 | C3 | C4 | Effect |
        // | R1   | T  | F  | F  | F  | E1 same fingerprint |
        // | R2   | F  | T  | F  | F  | E2 different |
        // | R3   | F  | F  | T  | F  | E3 different |
        // | R4   | F  | F  | F  | T  | E1 same fingerprint |
        // | R5 provider/session defaults differ | E1 same fingerprint |
        let original = capabilities();
        let expected = capability_fingerprint("codex", "1.0", &original);

        let mut reordered = original.clone();
        reordered.modes.reverse();
        reordered.config_options[0].choices.reverse();
        assert_eq!(
            capability_fingerprint("codex", "1.0", &reordered),
            expected,
            "R1"
        );
        assert_ne!(
            capability_fingerprint("claude", "1.0", &original),
            expected,
            "R2"
        );
        let mut changed_flag = original.clone();
        changed_flag.mcp_sse = true;
        assert_ne!(
            capability_fingerprint("codex", "1.0", &changed_flag),
            expected,
            "R3"
        );
        let mut changed_mode = original.clone();
        changed_mode.modes[0].native_id = "audit".into();
        assert_eq!(
            capability_fingerprint("codex", "1.0", &changed_mode),
            expected,
            "R4"
        );

        let mut routed = original;
        routed.modes[0].current = true;
        routed.modes[1].current = false;
        routed.modes[0].name = "Provider label".into();
        routed.modes[0].description = Some("route-local presentation".into());
        routed.config_options.clear();
        assert_eq!(
            capability_fingerprint("codex", "1.0", &routed),
            expected,
            "R5"
        );
    }

    #[test]
    fn capability_observation_coherence_decision_table_fails_closed() {
        // Causes: C1 state=Verified; C2 complete positive evidence; C3 failure
        // state; C4 reason present; C5 stale positive evidence is also present.
        // Effect E1: the observation is coherent and may be considered by a
        // selector. Every incomplete or mixed row produces E2 reject.
        //
        // | Rule | C1 | C2 | C3 | C4 | C5 | Effect |
        // | W1   | T  | T  | F  | F  | F  | E1 accept |
        // | W2   | T  | F  | F  | F  | F  | E2 reject |
        // | W3   | F  | F  | T  | T  | F  | E1 accept failure evidence |
        // | W4   | F  | F  | T  | F  | F  | E2 reject |
        // | W5   | F  | F  | T  | T  | T  | E2 reject mixed evidence |
        let failure =
            AcpCapabilityObservation::probe_failed("acp:codex", "1.0", 42, "handshake_timeout")
                .expect("W3 coherent failure evidence");
        let verified = AcpCapabilityObservation::verified(
            "acp:codex",
            "1.0",
            42,
            "sha256:verified",
            capabilities(),
        )
        .expect("W1 coherent verified evidence");

        for (rule, mut wire) in [
            ("W2", serde_json::to_value(&verified).unwrap()),
            ("W4", serde_json::to_value(&failure).unwrap()),
            ("W5", serde_json::to_value(&failure).unwrap()),
        ] {
            match rule {
                "W2" => wire["fingerprint"] = serde_json::Value::Null,
                "W4" => wire["reason_code"] = serde_json::Value::Null,
                _ => {
                    wire["fingerprint"] = "sha256:stale".into();
                    wire["negotiated"] = serde_json::to_value(capabilities()).unwrap();
                }
            }
            assert!(
                serde_json::from_value::<AcpCapabilityObservation>(wire).is_err(),
                "{rule}",
            );
        }
        assert!(
            AcpCapabilityObservation::verified("acp:codex", "1.0", 42, "", capabilities(),)
                .is_err(),
            "W2 construction",
        );
        assert!(
            AcpCapabilityObservation::probe_failed("acp:codex", "1.0", 42, "").is_err(),
            "W4 construction",
        );
    }

    #[test]
    fn publication_credential_usage_is_allowlisted() {
        // Causes: C1 no environment hint; C2 hint is in the projected adapter
        // allowlist; C3 hint is outside it; C4 adapter exposes no environment
        // delivery. Effects: E1 ProviderAdapter, E2 exact EnvironmentVariable,
        // E3 reject. Rules P1=C1 -> E1; P2=C2 -> E2; P3=C3||C4 -> E3.
        let capability = AcpPublicationCapability {
            backend_ref: "acp:test".into(),
            model_api_dialects: vec!["anthropic_messages".into()],
            model_selection: AcpModelSelectionSupport::DefaultAndExact,
            model_delivery_credential_environments: Some(vec!["TEST_API_KEY".into()]),
        };
        assert_eq!(
            capability.credential_usage(None).unwrap(),
            awaken_credential_contract::CredentialUsage::ProviderAdapter,
            "P1/E1"
        );
        assert_eq!(
            capability.credential_usage(Some("TEST_API_KEY")).unwrap(),
            awaken_credential_contract::CredentialUsage::EnvironmentVariable {
                name: "TEST_API_KEY".into()
            },
            "P2/E2"
        );
        assert!(
            capability.credential_usage(Some("OTHER_KEY")).is_err(),
            "P3/E3"
        );
        assert!(
            AcpPublicationCapability {
                model_delivery_credential_environments: None,
                ..capability
            }
            .credential_usage(Some("TEST_API_KEY"))
            .is_err(),
            "P3/E3 no delivery"
        );
    }

    #[test]
    fn model_selection_support_is_a_closed_wire_contract() {
        // Equivalence partitions: a default-only adapter cannot admit an exact
        // publication; an adapter with an exact interface can. The serialized
        // names are assertions on the management-plane contract, preventing a
        // regression to an ambiguous boolean capability.
        for (support, expected_wire, admits_exact) in [
            (
                AcpModelSelectionSupport::DefaultOnly,
                "\"default_only\"",
                false,
            ),
            (
                AcpModelSelectionSupport::DefaultAndExact,
                "\"default_and_exact\"",
                true,
            ),
        ] {
            assert_eq!(
                serde_json::to_string(&support).expect("closed selection support serializes"),
                expected_wire
            );
            assert_eq!(support.admits_exact(), admits_exact);
            assert_eq!(
                serde_json::from_str::<AcpModelSelectionSupport>(expected_wire)
                    .expect("canonical selection support deserializes"),
                support
            );
        }
        assert!(serde_json::from_str::<AcpModelSelectionSupport>("true").is_err());
        assert!(serde_json::from_str::<AcpModelSelectionSupport>("\"unknown\"").is_err());
    }
}
