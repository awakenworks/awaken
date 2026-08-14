//! Authenticated channels to private processes inside Session Pods.

use awaken_agent_channel::AgentChannel;
use k8s_openapi::api::core::v1::Pod;
use kube::Api;

use crate::RuntimeError;

const POD_CHANNEL_OPEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

pub(super) async fn open_pod_channel(
    pods: &Api<Pod>,
    pod: &str,
    port: u16,
) -> Result<Box<dyn AgentChannel>, RuntimeError> {
    // The resident Hand is intentionally not published through a Service.
    // Kubernetes authenticates this Pod subresource with the Worker's existing
    // kube client; NetworkPolicy and Pod credentials remain orthogonal to the
    // tool wire.
    let mut forwarder =
        tokio::time::timeout(POD_CHANNEL_OPEN_TIMEOUT, pods.portforward(pod, &[port]))
            .await
            .map_err(|_| {
                RuntimeError::Backend(format!(
                    "k8s Pod channel establishment timed out after {} seconds",
                    POD_CHANNEL_OPEN_TIMEOUT.as_secs()
                ))
            })?
            .map_err(super::backend)?;
    let channel = forwarder.take_stream(port).ok_or_else(|| {
        RuntimeError::Backend(format!(
            "k8s Pod channel did not expose requested resident Hand port {port}"
        ))
    })?;
    Ok(Box::new(channel))
}
