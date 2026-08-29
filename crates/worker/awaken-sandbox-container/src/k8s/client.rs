//! Kubernetes clients separated by request lifetime.

use kube::Client;

use crate::RuntimeError;

use super::backend;

/// Select the process-wide provider before any kube client is built. Workspace
/// feature unification can compile both rustls providers, so relying on rustls'
/// implicit selection is not deterministic.
pub(crate) fn install_rustls_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub(super) struct K8sClients {
    pub(super) control: Client,
    pub(super) streaming: Client,
}

impl K8sClients {
    pub(super) async fn infer() -> Result<Self, RuntimeError> {
        let config = kube::Config::infer().await.map_err(backend)?;
        Self::from_config(config)
    }

    fn from_config(config: kube::Config) -> Result<Self, RuntimeError> {
        let control = Client::try_from(config.clone()).map_err(backend)?;
        let streaming = Client::try_from(streaming_config(config)).map_err(backend)?;
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

fn streaming_config(mut config: kube::Config) -> kube::Config {
    config.read_timeout = None;
    config.write_timeout = None;
    config
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Causal graph: ACP starts a quiet long-running tool -> kube's ordinary
    // 295-second response timer closes attached exec -> protocol observes EOF ->
    // WorkUnit fails while the tool remains alive in the Pod. FMECA S9/O5/D8.
    // Decision table: control request keeps finite read/write ceilings; attached
    // stream has neither ceiling; connect/TLS/cluster coordinates stay identical.
    fn attached_exec_has_no_response_ceiling_without_weakening_control_timeouts() {
        let control = kube::Config::new("https://127.0.0.1:6443/".parse().unwrap());
        assert!(control.read_timeout.is_some());
        assert!(control.write_timeout.is_some());

        let stream = streaming_config(control.clone());
        assert_eq!(stream.cluster_url, control.cluster_url);
        assert_eq!(stream.connect_timeout, control.connect_timeout);
        assert_eq!(stream.default_namespace, control.default_namespace);
        assert_eq!(stream.tls_server_name, control.tls_server_name);
        assert!(stream.read_timeout.is_none());
        assert!(stream.write_timeout.is_none());

        assert!(control.read_timeout.is_some());
        assert!(control.write_timeout.is_some());
    }
}
