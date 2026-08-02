//! Neutral ACP capability negotiation values and port.
//!
//! Worker applications depend on this leaf; protocol adapters implement it.
//! The contract owns no process, repository, discovery or JSON-RPC behavior.

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpCapabilityObservationState {
    Verified,
    Unavailable,
    ProbeFailed,
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

/// Canonical fingerprint shared by discovery, publication and launch-time
/// handshake verification. Sorting makes adapter response order irrelevant.
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
    let mut modes = capabilities.modes.iter().collect::<Vec<_>>();
    modes.sort_by_key(|mode| &mode.native_id);
    for mode in modes {
        hash_value(&mut hash, &mode.native_id);
        hash_value(&mut hash, &mode.name);
        hash_optional(&mut hash, mode.description.as_deref());
        hash.update([u8::from(mode.current)]);
    }
    let mut options = capabilities.config_options.iter().collect::<Vec<_>>();
    options.sort_by_key(|option| &option.native_id);
    for option in options {
        hash_value(&mut hash, &option.native_id);
        hash_value(&mut hash, &option.name);
        hash_optional(&mut hash, option.description.as_deref());
        hash_optional(&mut hash, option.category.as_deref());
        hash_value(&mut hash, &option.current_value);
        let mut choices = option.choices.iter().collect::<Vec<_>>();
        choices.sort_by_key(|choice| (&choice.group_id, &choice.native_value));
        for choice in choices {
            hash_value(&mut hash, &choice.native_value);
            hash_value(&mut hash, &choice.name);
            hash_optional(&mut hash, choice.description.as_deref());
            hash_optional(&mut hash, choice.group_id.as_deref());
            hash_optional(&mut hash, choice.group_name.as_deref());
        }
    }
    format!("{:x}", hash.finalize())
}

fn hash_value(hash: &mut Sha256, value: &str) {
    hash.update(value.len().to_le_bytes());
    hash.update(value.as_bytes());
}

fn hash_optional(hash: &mut Sha256, value: Option<&str>) {
    hash.update([u8::from(value.is_some())]);
    if let Some(value) = value {
        hash_value(hash, value);
    }
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
    fn capability_fingerprint_decision_table_is_canonical_and_complete() {
        // Causes: C1 response order differs; C2 adapter identity differs; C3 a
        // negotiated flag differs; C4 a nested choice differs. Effects: E1 order
        // is normalized; E2-E4 semantic changes produce a new fingerprint.
        //
        // | Rule | C1 | C2 | C3 | C4 | Effect |
        // | R1   | T  | F  | F  | F  | E1 same fingerprint |
        // | R2   | F  | T  | F  | F  | E2 different |
        // | R3   | F  | F  | T  | F  | E3 different |
        // | R4   | F  | F  | F  | T  | E4 different |
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
        let mut changed_choice = original;
        changed_choice.config_options[0].choices[0].native_value = "tiny".into();
        assert_ne!(
            capability_fingerprint("codex", "1.0", &changed_choice),
            expected,
            "R4"
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
}
