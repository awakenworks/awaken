//! One attempt-local Unix rendezvous over an existing attached process channel.
//! Carries opaque bytes; MCP/ACP, tools, permissions and credentials stay above it.

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

pub const READY: &[u8] = b"AWAKEN_SOCKET_STDIO_MUX_V1_READY\n";

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

/// Bind one attempt-owned socket and map each local client to an independent
/// substream on the existing attached channel. One client's EOF never closes
/// another client or replaces the tool export owner. Physical Host EOF revokes
/// the complete rendezvous; a first-client deadline bounds abandoned setup.
pub async fn listen<R, W>(path: &str, input: R, mut output: W) -> Result<(), crate::ConnectError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let listener = crate::bind_unix_exclusive(&crate::ConnectionPlan::unix_listen(path))?;
        let _entry = SocketEntry::capture(path).map_err(io_error)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(io_error)?;
        output.write_all(READY).await.map_err(io_error)?;
        output.flush().await.map_err(io_error)?;
        let mut physical = crate::MultiplexedChannel::client(
            awaken_agent_channel::SplitChannel::new(input, output),
        );
        let mut bridges = tokio::task::JoinSet::new();
        let deadline = tokio::time::sleep(std::time::Duration::from_secs(10));
        tokio::pin!(deadline);
        let mut connected = false;
        loop {
            tokio::select! {
                ended = physical.accept() => {
                    return match ended {
                        None => Ok(()),
                        Some(Err(error)) => Err(io_error(error)),
                        Some(Ok(_)) => Err(crate::ConnectError::Policy(
                            "attached Host cannot initiate a local client stream".into(),
                        )),
                    };
                }
                accepted = listener.accept() => {
                    let socket = accepted?;
                    if bridges.len() >= crate::MultiplexedChannel::MAX_STREAMS {
                        // A local excess client receives EOF; existing streams
                        // retain their independent byte and permission context.
                        continue;
                    }
                    let Ok(stream) = physical.open().await else { continue };
                    connected = true;
                    let (read, write) = tokio::io::split(socket);
                    bridges.spawn(bridge(read, write, stream));
                }
                _ = bridges.join_next(), if !bridges.is_empty() => {},
                _ = &mut deadline, if !connected => {
                    return Err(crate::ConnectError::Io(
                        "attached socket client readiness timed out".into(),
                    ));
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, input, &mut output);
        Err(crate::ConnectError::Unsupported(
            "Unix rendezvous is unavailable".into(),
        ))
    }
}

fn io_error(error: std::io::Error) -> crate::ConnectError {
    crate::ConnectError::Io(error.to_string())
}
