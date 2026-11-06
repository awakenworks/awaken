// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Deployment-owned public trust coordinate for hosted Repository Git; a validated value with
// @formal reason: no credential or Repository authority, consumed only by the Sandbox control projection.
//! The hosted Repository Gateway coordinate a Kubernetes Sandbox Pod trusts.

use awaken_container_contract::RuntimeError;
use awaken_sandbox_container::normalize_hostname;

/// Deployment-owned public trust and exact network coordinate for hosted
/// Repository Git. It carries no credential or Repository authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct K8sRepositoryGatewayTrust {
    pub(crate) host: String,
    pub(crate) address: std::net::IpAddr,
    pub(crate) port: u16,
    pub(crate) ca_secret_name: String,
    pub(crate) ca_secret_key: String,
}

impl K8sRepositoryGatewayTrust {
    pub fn new(
        host: impl Into<String>,
        address: std::net::IpAddr,
        port: u16,
        ca_secret_name: impl Into<String>,
        ca_secret_key: impl Into<String>,
    ) -> Result<Self, RuntimeError> {
        let host = host.into();
        let ca_secret_name = ca_secret_name.into();
        let ca_secret_key = ca_secret_key.into();
        if normalize_hostname(&host).ok().as_deref() != Some(host.as_str())
            || port == 0
            || ca_secret_name.trim().is_empty()
            || ca_secret_name.contains('/')
            || ca_secret_key.trim().is_empty()
            || ca_secret_key.contains('/')
        {
            return Err(RuntimeError::Backend(
                "invalid Kubernetes Repository Gateway trust configuration".into(),
            ));
        }
        Ok(Self {
            host,
            address,
            port,
            ca_secret_name,
            ca_secret_key,
        })
    }
}
