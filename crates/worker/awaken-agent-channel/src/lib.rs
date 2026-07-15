//! The segregated agent-transport seam (ADR-0041 amendment).
//!
//! An opaque agent process (Claude Code, Codex, …) is driven over a **duplex byte
//! channel**, not through the runtime's tool layer. This crate owns only that
//! seam — one capability port ([`AgentTransport`]) and one duplex marker
//! ([`AgentChannel`]) — kept off `ProcessHandle` so tiers that never host a
//! protocol (the lexical Workdir tier) carry no transport weight (ISP).
//!
//! The neutral provisioning contract stays data-only: it names no stream type.
//! A provider that reports `tool_transparent` capability additionally implements
//! [`AgentTransport`]; local/bwrap back the channel with the spawned process's
//! piped stdio (see [`SplitChannel`]). A distributed provider (Slice 5) backs the
//! same channel with a `awaken-connection` transport — the consumer (an ACP
//! bridge) is written once against [`AgentChannel`].

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};

/// A duplex byte channel to a spawned agent process. A marker over
/// `AsyncRead + AsyncWrite`; the framing/protocol (ACP) is a use-site concern, so
/// the seam stays protocol-agnostic and one bridge spans every tier.
pub trait AgentChannel: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> AgentChannel for T {}

/// The capability a `tool_transparent` provider offers on top of `spawn`: hand a
/// consumer one [`AgentChannel`] to the agent it launched. Segregated from the
/// sandbox/process ports on purpose (a non-transparent tier does not implement it).
#[async_trait]
pub trait AgentTransport: Send + Sync {
    /// Open the duplex channel to the launched agent. Fails closed if the tier
    /// cannot back a protocol stream, or the process is gone.
    async fn open_channel(&self) -> Result<Box<dyn AgentChannel>, ChannelError>;
}

/// Why an [`AgentChannel`] could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    /// The tier does not host opaque agents (`tool_transparent = false`).
    #[error("provider tier is not tool-transparent; cannot open an agent channel")]
    NotTransparent,
    /// The channel could not be established (process gone, dial failed, …).
    #[error("agent channel setup failed: {0}")]
    Setup(String),
}

/// Combine a read half and a write half into one [`AgentChannel`]. This is how a
/// local/bwrap provider presents a spawned process's piped stdout+stdin as a
/// single duplex; the remote tiers substitute a socket-backed channel instead.
#[derive(Debug)]
pub struct SplitChannel<R, W> {
    read: R,
    write: W,
}

impl<R, W> SplitChannel<R, W> {
    /// Pair a reader (agent stdout) with a writer (agent stdin).
    pub fn new(read: R, write: W) -> Self {
        Self { read, write }
    }
}

impl<R, W> AsyncRead for SplitChannel<R, W>
where
    R: AsyncRead + Unpin,
    W: Unpin,
{
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().read).poll_read(cx, buf)
    }
}

impl<R, W> AsyncWrite for SplitChannel<R, W>
where
    R: Unpin,
    W: AsyncWrite + Unpin,
{
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().write).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().write).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().write).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A half that always faults, to prove `SplitChannel` surfaces underlying I/O
    /// errors (not just EOF) from each direction.
    struct ErrHalf;
    impl AsyncRead for ErrHalf {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "read boom",
            )))
        }
    }
    impl AsyncWrite for ErrHalf {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "write boom",
            )))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn split_channel_propagates_read_and_write_errors_not_just_eof() {
        let mut chan = SplitChannel::new(ErrHalf, ErrHalf);
        let mut buf = [0u8; 4];
        assert!(
            chan.read(&mut buf).await.is_err(),
            "a read-half fault surfaces through the channel"
        );
        assert!(
            chan.write(b"x").await.is_err(),
            "a write-half fault surfaces through the channel"
        );
    }

    /// A split channel round-trips bytes in both directions over piped halves.
    #[tokio::test]
    async fn split_channel_reads_and_writes_as_one_duplex() {
        // agent stdout → we read; we write → agent stdin.
        let (agent_stdout_w, agent_stdout_r) = tokio::io::duplex(64);
        let (agent_stdin_w, mut agent_stdin_r) = tokio::io::duplex(64);
        let mut chan: Box<dyn AgentChannel> =
            Box::new(SplitChannel::new(agent_stdout_r, agent_stdin_w));

        // The "agent" emits a line on its stdout.
        {
            let mut w = agent_stdout_w;
            w.write_all(b"hello from agent\n").await.unwrap();
            w.flush().await.unwrap();
        }
        let mut buf = vec![0u8; 17];
        chan.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello from agent\n");

        // The consumer writes a prompt to the agent's stdin.
        chan.write_all(b"do the task\n").await.unwrap();
        chan.flush().await.unwrap();
        let mut got = vec![0u8; 12];
        agent_stdin_r.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"do the task\n");
    }

    #[tokio::test]
    async fn split_channel_flush_and_shutdown_delegate_to_the_writer() {
        let (writer, mut peer) = tokio::io::duplex(64);
        let (reader, _unused) = tokio::io::duplex(64);
        let mut chan = SplitChannel::new(reader, writer);
        chan.write_all(b"bye").await.unwrap();
        chan.flush().await.unwrap();
        chan.shutdown().await.unwrap();
        // The peer sees EOF after shutdown, having received the bytes.
        let mut got = Vec::new();
        peer.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"bye");
    }

    struct FakeTransport {
        transparent: bool,
    }

    #[async_trait]
    impl AgentTransport for FakeTransport {
        async fn open_channel(&self) -> Result<Box<dyn AgentChannel>, ChannelError> {
            if !self.transparent {
                return Err(ChannelError::NotTransparent);
            }
            let (a, _b) = tokio::io::duplex(8);
            Ok(Box::new(a))
        }
    }

    #[tokio::test]
    async fn transport_opens_when_transparent_and_fails_closed_otherwise() {
        assert!(
            FakeTransport { transparent: true }
                .open_channel()
                .await
                .is_ok()
        );
        // `Box<dyn AgentChannel>` is not `Debug`, so match rather than `unwrap_err`.
        let err = match (FakeTransport { transparent: false }).open_channel().await {
            Err(e) => e,
            Ok(_) => panic!("a non-transparent tier must refuse to open a channel"),
        };
        assert!(matches!(err, ChannelError::NotTransparent));
        assert!(ChannelError::Setup("x".into()).to_string().contains("x"));
    }

    /// Each `ChannelError` arm maps to a distinct, human-readable message — the
    /// `Setup` arm's was asserted above, but `NotTransparent`'s cause was not.
    #[test]
    fn channel_error_display_names_each_cause() {
        assert!(
            ChannelError::NotTransparent
                .to_string()
                .contains("not tool-transparent"),
            "the not-transparent arm names why the channel was refused"
        );
        assert_eq!(
            ChannelError::Setup("dial failed".into()).to_string(),
            "agent channel setup failed: dial failed"
        );
    }

    /// `poll_shutdown` delegates only to the write half, so shutting the channel
    /// closes the outbound direction while the read half keeps delivering inbound
    /// bytes. The earlier flush/shutdown test only checked the write peer's EOF.
    #[tokio::test]
    async fn split_channel_shutdown_closes_only_the_write_half() {
        let (agent_stdout_w, agent_stdout_r) = tokio::io::duplex(64);
        let (writer, mut write_peer) = tokio::io::duplex(64);
        let mut chan = SplitChannel::new(agent_stdout_r, writer);

        // Shut down the outbound (write) direction.
        chan.shutdown().await.unwrap();
        // The write peer observes EOF with nothing buffered.
        let mut drained = Vec::new();
        write_peer.read_to_end(&mut drained).await.unwrap();
        assert!(drained.is_empty(), "no bytes were written before shutdown");

        // The read half is untouched by shutdown and still delivers inbound bytes.
        {
            let mut w = agent_stdout_w;
            w.write_all(b"still-up").await.unwrap();
            w.flush().await.unwrap();
        }
        let mut got = vec![0u8; 8];
        chan.read_exact(&mut got).await.unwrap();
        assert_eq!(
            &got, b"still-up",
            "shutdown is one-directional; the read half survives it"
        );
    }
}
