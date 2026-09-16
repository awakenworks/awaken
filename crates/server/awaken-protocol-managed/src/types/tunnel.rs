//! Research-preview MCP Tunnel wire types (`mcp-tunnels-2026-06-22`).

use serde::{Deserialize, Serialize};

/// `BetaTunnel`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tunnel {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub archived_at: Option<String>,
    pub created_at: String,
    pub display_name: Option<String>,
    pub domain: String,
}

/// `BetaTunnelToken`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelToken {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub tunnel_token: String,
}

/// `BetaTunnelCertificate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelCertificate {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub archived_at: Option<String>,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub fingerprint: String,
    pub tunnel_id: String,
}

/// `TunnelCreateParams`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TunnelCreateParams {
    #[serde(default)]
    pub display_name: Option<String>,
}

/// `TunnelRotateTokenParams`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TunnelRotateTokenParams {
    #[serde(default)]
    pub reason: Option<String>,
}

/// `CertificateCreateParams`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CertificateCreateParams {
    pub ca_certificate_pem: String,
}

/// `TunnelListParams`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct TunnelListQuery {
    #[serde(flatten)]
    pub page: super::PageQuery,
    #[serde(default)]
    pub include_archived: bool,
}
