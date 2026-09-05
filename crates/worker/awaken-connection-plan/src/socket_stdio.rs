//! One attempt-local Unix rendezvous over an existing attached process channel.
//! Carries opaque bytes; MCP/ACP, tools, permissions and credentials stay above it.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const READY: &[u8] = b"AWAKEN_SOCKET_STDIO_READY\n";

/// Copy a channel in both directions. EOF half-closes its peer writer so a
/// request can finish while the other direction drains its final response.
async fn bridge<R, W>(
    mut input: R,
    mut output: W,
    channel: Box<dyn awaken_agent_channel::AgentChannel>,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (mut read, mut write) = tokio::io::split(channel);
    let outbound = async {
        tokio::io::copy(&mut input, &mut write).await?;
        write.shutdown().await
    };
    let inbound = async {
        tokio::io::copy(&mut read, &mut output).await?;
        output.shutdown().await
    };
    tokio::try_join!(outbound, inbound)?;
    Ok(())
}

/// Dial an exact in-sandbox socket and expose its bytes on the caller's stdio.
pub async fn connect<R, W>(path: &str, input: R, output: W) -> Result<(), crate::ConnectError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    use crate::ChannelFactory as _;
    let channel = crate::TokioChannelFactory
        .connect(&crate::ConnectionPlan::unix_dial(path))
        .await?;
    bridge(input, output, channel)
        .await
        .map_err(|error| crate::ConnectError::Io(error.to_string()))
}

#[cfg(unix)]
struct SocketEntry {
    path: std::path::PathBuf,
    device: u64,
    inode: u64,
}

#[cfg(unix)]
impl SocketEntry {
    fn capture(path: &str) -> std::io::Result<Self> {
        use std::os::unix::fs::MetadataExt as _;
        let metadata = std::fs::symlink_metadata(path)?;
        Ok(Self {
            path: path.into(),
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

#[cfg(unix)]
impl Drop for SocketEntry {
    fn drop(&mut self) {
        use std::os::unix::fs::MetadataExt as _;
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| metadata.dev() == self.device && metadata.ino() == self.inode)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Bind a fresh socket, emit one readiness line, and serve one client. The
/// existing attached process stream is the only path out of the sandbox.
/// Host EOF while awaiting the client closes the rendezvous immediately.
pub async fn listen<R, W>(
    path: &str,
    mut input: R,
    mut output: W,
) -> Result<(), crate::ConnectError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let listener = crate::bind_unix_exclusive(&crate::ConnectionPlan::unix_listen(path))?;
        let _entry = SocketEntry::capture(path)
            .map_err(|error| crate::ConnectError::Io(error.to_string()))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| crate::ConnectError::Io(error.to_string()))?;
        output.write_all(READY).await.map_err(io_error)?;
        output.flush().await.map_err(io_error)?;
        let mut first = [0_u8; 1];
        let accepted = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            Ok::<_, crate::ConnectError>(tokio::select! {
                accepted = listener.accept() => Some((accepted?, None)),
                read = input.read(&mut first) => match read.map_err(io_error)? {
                    0 => return Ok(None),
                    _ => Some((listener.accept().await?, Some(first[0]))),
                },
            })
        })
        .await
        .map_err(|_| {
            crate::ConnectError::Io("attached socket client readiness timed out".into())
        })??;
        let Some((channel, prefix)) = accepted else {
            return Ok(());
        };
        drop(listener);
        let mut channel = channel;
        if let Some(prefix) = prefix {
            channel.write_all(&[prefix]).await.map_err(io_error)?;
            channel.flush().await.map_err(io_error)?;
        }
        bridge(input, output, channel).await.map_err(io_error)
    }
    #[cfg(not(unix))]
    {
        let _ = (path, &mut input, &mut output);
        Err(crate::ConnectError::Unsupported(
            "Unix rendezvous is unavailable".into(),
        ))
    }
}

fn io_error(error: std::io::Error) -> crate::ConnectError {
    crate::ConnectError::Io(error.to_string())
}
