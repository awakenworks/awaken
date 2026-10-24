//! The authenticated control client of the Kubernetes API.

use kube::Client;

use awaken_container_contract::RuntimeError;

use crate::backend;

/// One authenticated control client for read-only infrastructure adapters. It
/// creates no Sandbox resources or NetworkPolicies and retains finite deadlines.
pub async fn connect_control_client() -> Result<Client, RuntimeError> {
    let config = kube::Config::infer().await.map_err(backend)?;
    awaken_outbound_http::kubernetes_control_client(config).map_err(backend)
}
