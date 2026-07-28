//! Neutral ACP capability negotiation values and port.
//!
//! Worker applications depend on this leaf; protocol adapters implement it.
//! The contract owns no process, repository, discovery or JSON-RPC behavior.

use async_trait::async_trait;
use awaken_agent_channel::AgentChannel;
use serde::{Deserialize, Serialize};

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
