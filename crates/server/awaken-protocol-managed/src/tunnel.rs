//! Consumer-owned application port for the Cloud-only Tunnel bounded context.

use async_trait::async_trait;

use crate::types::tunnel::{Tunnel, TunnelCertificate, TunnelToken};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedTunnelScope {
    pub workspace_id: String,
    pub operation_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManagedTunnelApplicationError {
    #[error("Tunnel resource was not found")]
    NotFound,
    #[error("Tunnel request is invalid: {0}")]
    Invalid(String),
    #[error("Tunnel command conflicts with current state: {0}")]
    Conflict(String),
    #[error("Tunnel request is forbidden")]
    Forbidden,
    #[error("Tunnel application is unavailable: {0}")]
    Unavailable(String),
}

#[async_trait]
pub trait ManagedTunnelApplication: Send + Sync {
    async fn create_tunnel(
        &self,
        scope: ManagedTunnelScope,
        display_name: Option<String>,
    ) -> Result<Tunnel, ManagedTunnelApplicationError>;
    async fn retrieve_tunnel(
        &self,
        scope: ManagedTunnelScope,
        tunnel_id: &str,
    ) -> Result<Tunnel, ManagedTunnelApplicationError>;
    async fn list_tunnels(
        &self,
        scope: ManagedTunnelScope,
        include_archived: bool,
    ) -> Result<Vec<Tunnel>, ManagedTunnelApplicationError>;
    async fn archive_tunnel(
        &self,
        scope: ManagedTunnelScope,
        tunnel_id: &str,
    ) -> Result<Tunnel, ManagedTunnelApplicationError>;
    async fn reveal_token(
        &self,
        scope: ManagedTunnelScope,
        tunnel_id: &str,
    ) -> Result<TunnelToken, ManagedTunnelApplicationError>;
    async fn rotate_token(
        &self,
        scope: ManagedTunnelScope,
        tunnel_id: &str,
        reason: Option<String>,
    ) -> Result<TunnelToken, ManagedTunnelApplicationError>;
    async fn create_certificate(
        &self,
        scope: ManagedTunnelScope,
        tunnel_id: &str,
        ca_certificate_pem: String,
    ) -> Result<TunnelCertificate, ManagedTunnelApplicationError>;
    async fn retrieve_certificate(
        &self,
        scope: ManagedTunnelScope,
        tunnel_id: &str,
        certificate_id: &str,
    ) -> Result<TunnelCertificate, ManagedTunnelApplicationError>;
    async fn list_certificates(
        &self,
        scope: ManagedTunnelScope,
        tunnel_id: &str,
        include_archived: bool,
    ) -> Result<Vec<TunnelCertificate>, ManagedTunnelApplicationError>;
    async fn archive_certificate(
        &self,
        scope: ManagedTunnelScope,
        tunnel_id: &str,
        certificate_id: &str,
    ) -> Result<TunnelCertificate, ManagedTunnelApplicationError>;
}
