//! Remote-tier transport (ADR-0041 Slice 5, `connection` feature).
//!
//! The container/pod agent runs as the container's main process; the runtime reaches
//! its stdio over the network, not a local pipe. This module establishes that
//! [`AgentChannel`] via `awaken-connection`:
//! - **direct dial** ([`TcpAgentTransport`]) for a published port (Docker `-p`,
//!   k8s Service);
//! - **reverse dial** ([`bind_reverse`]) for a firewalled/outbound-only Pod, where
//!   the sandbox dials out to a rendezvous the host listens on.
//!
//! `awaken-connection`'s `Channel` is a marker, so we wrap `TcpStream` in
//! [`TcpChannel`] (which adds the `AsyncRead + AsyncWrite` bounds the ACP bridge
//! needs and therefore *is* an `AgentChannel`).

use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use async_trait::async_trait;
use awaken_agent_channel::{AgentChannel, AgentTransport, ChannelError};
use awaken_connection::{Channel, ConnectError, DialEnd, ListenSide, Transport, bind_pair};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};

/// A `TcpStream` presented as an `awaken-connection` [`Channel`] and (via the
/// `AsyncRead + AsyncWrite` blanket) an [`AgentChannel`].
#[derive(Debug)]
pub struct TcpChannel(pub TcpStream);

impl Channel for TcpChannel {}

impl AsyncRead for TcpChannel {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl AsyncWrite for TcpChannel {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}

/// A typed TCP transport: dials `addr`, no handshake material.
pub struct TcpTransport;

#[async_trait]
impl Transport for TcpTransport {
    type Channel = TcpChannel;
    type Address = SocketAddr;
    type HandshakeMaterial = ();

    fn scheme(&self) -> &str {
        "tcp"
    }

    async fn dial(&self, addr: SocketAddr, _material: &()) -> Result<TcpChannel, ConnectError> {
        TcpStream::connect(addr)
            .await
            .map(TcpChannel)
            .map_err(|e| ConnectError::Io(e.to_string()))
    }
}

/// Direct-dial agent transport: reaches a published container port. Implements the
/// neutral [`AgentTransport`] so the ACP bridge is agnostic to local-vs-remote.
pub struct TcpAgentTransport {
    addr: SocketAddr,
}

impl TcpAgentTransport {
    pub fn new(addr: SocketAddr) -> Self {
        Self { addr }
    }
}

#[async_trait]
impl AgentTransport for TcpAgentTransport {
    async fn open_channel(&self) -> Result<Box<dyn AgentChannel>, ChannelError> {
        let transport = TcpTransport;
        let channel = transport
            .dial(self.addr, &())
            .await
            .map_err(|e| ChannelError::Setup(e.to_string()))?;
        Ok(Box::new(channel))
    }
}

// ── Reverse dial (outbound-only Pod) ────────────────────────────────────────────

/// The sandbox side of a reverse connection: it dials out to the host's rendezvous.
pub struct ReverseDial;

/// The host side: it listens for the sandbox's outbound dial.
pub struct ReverseListen {
    listener: TcpListener,
}

impl ReverseListen {
    /// Bind a rendezvous listener the firewalled sandbox will dial back to.
    pub async fn bind(addr: SocketAddr) -> Result<Self, ConnectError> {
        TcpListener::bind(addr)
            .await
            .map(|listener| Self { listener })
            .map_err(|e| ConnectError::Setup(e.to_string()))
    }

    /// The address the sandbox should dial (resolves an ephemeral `:0` port).
    pub fn local_addr(&self) -> Result<SocketAddr, ConnectError> {
        self.listener
            .local_addr()
            .map_err(|e| ConnectError::Setup(e.to_string()))
    }
}

#[async_trait]
impl DialEnd for ReverseDial {
    type Channel = TcpChannel;
    type Address = SocketAddr;
    type DialMaterial = ();
    type Peer = ReverseListen;

    async fn dial(&self, addr: SocketAddr, _material: ()) -> Result<TcpChannel, ConnectError> {
        TcpStream::connect(addr)
            .await
            .map(TcpChannel)
            .map_err(|e| ConnectError::Io(e.to_string()))
    }
}

#[async_trait]
impl ListenSide for ReverseListen {
    type Channel = TcpChannel;
    type Peer = ReverseDial;

    async fn accept(&self) -> Result<TcpChannel, ConnectError> {
        self.listener
            .accept()
            .await
            .map(|(stream, _peer)| TcpChannel(stream))
            .map_err(|e| ConnectError::Io(e.to_string()))
    }
}

/// Pair a firewalled sandbox's outbound dial with the host's rendezvous listener,
/// returning `(host_channel, sandbox_channel)` — the reverse-dial handshake. Binds
/// on `bind_addr` (an ephemeral `:0` is fine) and dials the *resolved* port, so the
/// sandbox always reaches the concrete address the host actually bound.
pub async fn bind_reverse(bind_addr: SocketAddr) -> Result<(TcpChannel, TcpChannel), ConnectError> {
    let listen = ReverseListen::bind(bind_addr).await?;
    let dial_addr = listen.local_addr()?;
    bind_pair(&ReverseDial, &listen, dial_addr, ()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn loopback() -> SocketAddr {
        "127.0.0.1:0".parse().unwrap()
    }

    #[tokio::test]
    async fn direct_dial_opens_a_duplex_agent_channel() {
        // A stand-in "agent" listens; the transport dials it and echoes a byte.
        let listener = TcpListener::bind(loopback()).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut b = [0u8; 4];
            s.read_exact(&mut b).await.unwrap();
            s.write_all(&b).await.unwrap();
        });

        let mut chan = TcpAgentTransport::new(addr).open_channel().await.unwrap();
        chan.write_all(b"ping").await.unwrap();
        chan.flush().await.unwrap();
        let mut got = [0u8; 4];
        chan.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"ping");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn reverse_dial_pairs_host_and_sandbox() {
        // The listener must exist before we compute the dial address, so bind first.
        let listen = ReverseListen::bind(loopback()).await.unwrap();
        let addr = listen.local_addr().unwrap();
        let (mut host, mut sandbox) = bind_pair(&ReverseDial, &listen, addr, ()).await.unwrap();

        // Host → sandbox, then sandbox → host, over the reverse-dialed channel.
        host.write_all(b"prompt\n").await.unwrap();
        host.flush().await.unwrap();
        let mut buf = [0u8; 7];
        sandbox.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"prompt\n");

        sandbox.write_all(b"event\n").await.unwrap();
        sandbox.flush().await.unwrap();
        let mut evt = [0u8; 6];
        host.read_exact(&mut evt).await.unwrap();
        assert_eq!(&evt, b"event\n");
    }

    #[tokio::test]
    async fn bind_reverse_resolves_the_ephemeral_port_and_pairs() {
        let (mut host, mut sandbox) = bind_reverse(loopback()).await.unwrap();
        host.write_all(b"go\n").await.unwrap();
        host.flush().await.unwrap();
        let mut buf = [0u8; 3];
        sandbox.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"go\n");
    }

    #[test]
    fn tcp_transport_reports_its_scheme() {
        assert_eq!(TcpTransport.scheme(), "tcp");
    }

    #[tokio::test]
    async fn direct_dial_to_a_dead_port_fails_closed() {
        // Bind then drop to obtain a certainly-closed port.
        let addr = {
            let l = TcpListener::bind(loopback()).await.unwrap();
            l.local_addr().unwrap()
        };
        assert!(TcpAgentTransport::new(addr).open_channel().await.is_err());
    }
}
