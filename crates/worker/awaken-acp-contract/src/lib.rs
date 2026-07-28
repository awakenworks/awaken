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
