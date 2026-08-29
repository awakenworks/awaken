//! Authenticated channels to private processes inside Session Pods.

use awaken_agent_channel::AgentChannel;
use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::RuntimeError;

const POD_CHANNEL_OPEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

struct PodPortForwardChannel<S, E> {
    stream: S,
    remote_error: Option<std::pin::Pin<Box<E>>>,
    failed: bool,
}

impl<S: Unpin, E> Unpin for PodPortForwardChannel<S, E> {}

impl<S, E> PodPortForwardChannel<S, E> {
    fn new(stream: S, remote_error: E) -> Self {
        Self {
            stream,
            remote_error: Some(Box::pin(remote_error)),
            failed: false,
        }
    }
}

impl<S, E> PodPortForwardChannel<S, E>
where
    E: std::future::Future<Output = Option<String>>,
{
    fn remote_failed(&mut self, context: &mut std::task::Context<'_>) -> bool {
        if self.failed {
            return true;
        }
        if self
            .remote_error
            .as_mut()
            .is_some_and(|error| std::future::Future::poll(error.as_mut(), context).is_ready())
        {
            // Kubernetes resolves the error receiver both for an explicit
            // remote-port error and when its sender disappears. Either means
            // the paired stream is no longer a usable Pod channel. Do not put
            // the server-supplied diagnostic into an I/O error/log.
            self.remote_error = None;
            self.failed = true;
        }
        self.failed
    }

    fn unavailable() -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            "Kubernetes Pod private channel is unavailable",
        )
    }
}

impl<S, E> AsyncRead for PodPortForwardChannel<S, E>
where
    S: AsyncRead + Unpin,
    E: std::future::Future<Output = Option<String>>,
{
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.remote_failed(context) {
            return std::task::Poll::Ready(Err(Self::unavailable()));
        }
        std::pin::Pin::new(&mut self.stream).poll_read(context, buffer)
    }
}

impl<S, E> AsyncWrite for PodPortForwardChannel<S, E>
where
    S: AsyncWrite + Unpin,
    E: std::future::Future<Output = Option<String>>,
{
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if self.remote_failed(context) {
            return std::task::Poll::Ready(Err(Self::unavailable()));
        }
        std::pin::Pin::new(&mut self.stream).poll_write(context, buffer)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.remote_failed(context) {
            return std::task::Poll::Ready(Err(Self::unavailable()));
        }
        std::pin::Pin::new(&mut self.stream).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.remote_failed(context) {
            return std::task::Poll::Ready(Err(Self::unavailable()));
        }
        std::pin::Pin::new(&mut self.stream).poll_shutdown(context)
    }
}

/// Host side of a reverse dial. This remains in the one Kubernetes channel
/// owner beside exact-Pod port-forward establishment.
pub(super) async fn accept_reverse(
    addr: std::net::SocketAddr,
) -> Result<Box<dyn AgentChannel>, RuntimeError> {
    use awaken_connection::ListenSide as _;

    let listen = crate::net::ReverseListen::bind(addr)
        .await
        .map_err(super::backend)?;
    let channel = listen.accept().await.map_err(super::backend)?;
    Ok(Box::new(channel))
}

pub(super) async fn open_pod_channel(
    pods: &Api<Pod>,
    pod: &str,
    port: u16,
) -> Result<Box<dyn AgentChannel>, RuntimeError> {
    // Private resident-process and Sandbox-control channels are intentionally
    // not published through a Service. Kubernetes authenticates this exact Pod
    // subresource with the Worker's existing kube client.
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
            "k8s Pod channel did not expose requested private service port {port}"
        ))
    })?;
    let remote_error = forwarder.take_error(port).ok_or_else(|| {
        RuntimeError::Backend(format!(
            "k8s Pod channel did not expose an error fence for private service port {port}"
        ))
    })?;
    Ok(Box::new(PodPortForwardChannel::new(channel, remote_error)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    #[tokio::test]
    async fn remote_port_error_fences_the_taken_duplex_stream() {
        /* Port-forward error cause/effect table: C1=stream and its paired error
         * future are both live; C2=ordinary bytes arrive; C3=Kubernetes reports
         * a remote connect error (or drops its sender). E1=C2 passes losslessly;
         * E2=C3 makes every later I/O fail without exposing the remote message.
         * Rules PF1 C1+C2=>E1; PF2 C1+C3=>E2.
         */
        let (stream, mut peer) = tokio::io::duplex(64);
        let (error_tx, error_rx) = tokio::sync::oneshot::channel::<String>();
        let mut channel = PodPortForwardChannel::new(stream, async move { error_rx.await.ok() });
        peer.write_all(b"ok").await.unwrap();
        let mut observed = [0_u8; 2];
        channel.read_exact(&mut observed).await.unwrap();
        assert_eq!(&observed, b"ok", "PF1/E1");

        error_tx
            .send("remote diagnostic must stay private".into())
            .unwrap();
        tokio::task::yield_now().await;
        let error = channel.write_all(b"denied").await.unwrap_err();
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::ConnectionAborted,
            "PF2/E2"
        );
        assert!(!error.to_string().contains("remote diagnostic"), "PF2/E2");
    }
}
