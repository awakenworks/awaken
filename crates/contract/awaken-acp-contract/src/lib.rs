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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpCapabilityObservation {
    pub backend_ref: String,
    pub adapter_version: String,
    pub state: AcpCapabilityObservationState,
    pub observed_at_ms: u64,
    pub fingerprint: Option<String>,
    pub negotiated: Option<NegotiatedAcpCapabilities>,
    pub reason_code: Option<String>,
}

impl AcpCapabilityObservation {
    /// Whether the state and its mutually exclusive evidence fields agree.
    ///
    /// Callers must reject incoherent observations rather than inferring a
    /// verified capability from whichever optional fields happen to be set.
    #[must_use]
    pub fn is_coherent(&self) -> bool {
        match self.state {
            AcpCapabilityObservationState::Verified => {
                self.fingerprint
                    .as_deref()
                    .is_some_and(|fingerprint| !fingerprint.trim().is_empty())
                    && self.negotiated.is_some()
                    && self.reason_code.is_none()
            }
            AcpCapabilityObservationState::Unavailable
            | AcpCapabilityObservationState::ProbeFailed => {
                self.fingerprint.is_none()
                    && self.negotiated.is_none()
                    && self
                        .reason_code
                        .as_deref()
                        .is_some_and(|reason| !reason.trim().is_empty())
            }
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpPublicationCapability {
    pub backend_ref: String,
    pub model_api_dialects: Vec<String>,
    pub supports_exact_model_selection: bool,
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
        let failure = AcpCapabilityObservation {
            backend_ref: "acp:codex".into(),
            adapter_version: "1.0".into(),
            state: AcpCapabilityObservationState::ProbeFailed,
            observed_at_ms: 42,
            fingerprint: None,
            negotiated: None,
            reason_code: Some("handshake_timeout".into()),
        };
        let verified = AcpCapabilityObservation {
            state: AcpCapabilityObservationState::Verified,
            fingerprint: Some("sha256:verified".into()),
            negotiated: Some(capabilities()),
            reason_code: None,
            ..failure.clone()
        };
        assert!(verified.is_coherent(), "W1");

        let missing_fingerprint = AcpCapabilityObservation {
            fingerprint: None,
            ..verified.clone()
        };
        assert!(!missing_fingerprint.is_coherent(), "W2");
        assert!(failure.is_coherent(), "W3");

        let missing_reason = AcpCapabilityObservation {
            reason_code: None,
            ..failure.clone()
        };
        assert!(!missing_reason.is_coherent(), "W4");
        let mixed_failure = AcpCapabilityObservation {
            fingerprint: verified.fingerprint,
            negotiated: verified.negotiated,
            ..failure
        };
        assert!(!mixed_failure.is_coherent(), "W5");
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
            supports_exact_model_selection: true,
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
}
