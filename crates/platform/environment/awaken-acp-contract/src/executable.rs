//! Secret-free identity of the executable combination behind an ACP session.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpExecutableSource {
    Host,
    AdapterManaged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpExecutableIdentity {
    pub cli_id: String,
    /// Version reported by the actual executable, not a separately installed CLI.
    pub cli_version: String,
    /// Exact catalog package@version, absent for a direct ACP executable.
    pub adapter_package: Option<String>,
    pub source: AcpExecutableSource,
}

/// Frozen with the existing Thread ACP session reference after a live handshake.
/// Private CLI session formats are resumed only by the same runtime combination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpSessionBinding {
    pub identity: AcpExecutableIdentity,
    pub protocol_version: String,
    pub capability_fingerprint: String,
}

impl AcpSessionBinding {
    #[must_use]
    pub fn verified(
        identity: AcpExecutableIdentity,
        capabilities: &crate::NegotiatedAcpCapabilities,
    ) -> Self {
        Self {
            protocol_version: capabilities.protocol_version.clone(),
            capability_fingerprint: crate::capability_fingerprint(
                &identity.cli_id,
                &identity.cli_version,
                capabilities,
            ),
            identity,
        }
    }
}
