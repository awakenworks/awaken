//! Empty hosted ports for composition tests of the open repository.

use awaken_protocol_managed::types::tunnel::{Tunnel, TunnelCertificate, TunnelToken};
use awaken_protocol_managed::{
    ManagedTunnelApplication, ManagedTunnelApplicationError, ManagedTunnelScope,
};

/// Empty hosted Tunnel port for composition tests.
///
/// Open Awaken deliberately leaves the Cloud-owned Tunnel port absent. Tests
/// that qualify the hosted composition seam inject this adapter so the real
/// Control router is mounted without copying Cloud state or business behavior
/// into the open repository.
#[derive(Debug, Default)]
pub struct EmptyManagedTunnelApplication;

#[async_trait::async_trait]
impl ManagedTunnelApplication for EmptyManagedTunnelApplication {
    async fn create_tunnel(
        &self,
        _scope: ManagedTunnelScope,
        _display_name: Option<String>,
    ) -> Result<Tunnel, ManagedTunnelApplicationError> {
        Err(ManagedTunnelApplicationError::NotFound)
    }

    async fn retrieve_tunnel(
        &self,
        _scope: ManagedTunnelScope,
        _tunnel_id: &str,
    ) -> Result<Tunnel, ManagedTunnelApplicationError> {
        Err(ManagedTunnelApplicationError::NotFound)
    }

    async fn list_tunnels(
        &self,
        _scope: ManagedTunnelScope,
        _include_archived: bool,
    ) -> Result<Vec<Tunnel>, ManagedTunnelApplicationError> {
        Ok(Vec::new())
    }

    async fn archive_tunnel(
        &self,
        _scope: ManagedTunnelScope,
        _tunnel_id: &str,
    ) -> Result<Tunnel, ManagedTunnelApplicationError> {
        Err(ManagedTunnelApplicationError::NotFound)
    }

    async fn reveal_token(
        &self,
        _scope: ManagedTunnelScope,
        _tunnel_id: &str,
    ) -> Result<TunnelToken, ManagedTunnelApplicationError> {
        Err(ManagedTunnelApplicationError::NotFound)
    }

    async fn rotate_token(
        &self,
        _scope: ManagedTunnelScope,
        _tunnel_id: &str,
        _reason: Option<String>,
    ) -> Result<TunnelToken, ManagedTunnelApplicationError> {
        Err(ManagedTunnelApplicationError::NotFound)
    }

    async fn create_certificate(
        &self,
        _scope: ManagedTunnelScope,
        _tunnel_id: &str,
        _ca_certificate_pem: String,
    ) -> Result<TunnelCertificate, ManagedTunnelApplicationError> {
        Err(ManagedTunnelApplicationError::NotFound)
    }

    async fn retrieve_certificate(
        &self,
        _scope: ManagedTunnelScope,
        _tunnel_id: &str,
        _certificate_id: &str,
    ) -> Result<TunnelCertificate, ManagedTunnelApplicationError> {
        Err(ManagedTunnelApplicationError::NotFound)
    }

    async fn list_certificates(
        &self,
        _scope: ManagedTunnelScope,
        _tunnel_id: &str,
        _include_archived: bool,
    ) -> Result<Vec<TunnelCertificate>, ManagedTunnelApplicationError> {
        Err(ManagedTunnelApplicationError::NotFound)
    }

    async fn archive_certificate(
        &self,
        _scope: ManagedTunnelScope,
        _tunnel_id: &str,
        _certificate_id: &str,
    ) -> Result<TunnelCertificate, ManagedTunnelApplicationError> {
        Err(ManagedTunnelApplicationError::NotFound)
    }
}
