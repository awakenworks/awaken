//! Kubernetes clients separated by request lifetime.

use kube::Client;

use awaken_sandbox_container::RuntimeError;

use super::backend;

/// One authenticated control client for read-only infrastructure adapters. It
/// creates no Sandbox resources or NetworkPolicies and retains finite deadlines.
pub async fn connect_control_client() -> Result<Client, RuntimeError> {
    let config = kube::Config::infer().await.map_err(backend)?;
    awaken_outbound_http::kubernetes_control_client(config).map_err(backend)
}

pub(super) struct K8sClients {
    pub(super) control: Client,
    pub(super) streaming: Client,
}

impl K8sClients {
    pub(super) async fn infer() -> Result<Self, RuntimeError> {
        Self::from_config(kube::Config::infer().await.map_err(backend)?)
    }

    fn from_config(config: kube::Config) -> Result<Self, RuntimeError> {
        let control =
            awaken_outbound_http::kubernetes_control_client(config.clone()).map_err(backend)?;
        let streaming =
            awaken_outbound_http::kubernetes_streaming_client(config).map_err(backend)?;
        Ok(Self { control, streaming })
    }

    #[cfg(test)]
    pub(super) fn for_test() -> Self {
        Self::from_config(kube::Config::new(
            "http://127.0.0.1:1/".parse().expect("test URI"),
        ))
        .expect("lazy kube clients build without a cluster")
    }
}

impl super::K8sRuntime {
    /// Probe the apiserver (for tests / health checks): `Ok` iff it responds.
    pub async fn ping(&self) -> Result<(), RuntimeError> {
        self.pods()
            .list(&kube::api::ListParams::default().limit(1))
            .await
            .map(|_| ())
            .map_err(backend)
    }
}
