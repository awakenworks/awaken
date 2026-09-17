//! Bounded u32 length-prefixed JSON framing for one control exchange.

use awaken_sandbox_control::{
    SandboxControlProtocolError, SandboxControlRequest, SandboxControlService,
};
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::Zeroizing;

/// Hard upper bound for one control request or response.
pub const MAX_SANDBOX_CONTROL_FRAME_BYTES: usize = 64 * 1024;

pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> Result<(), SandboxControlProtocolError>
where
    W: AsyncWrite + Unpin + ?Sized,
    T: Serialize,
{
    let payload = Zeroizing::new(
        serde_json::to_vec(value).map_err(|_| SandboxControlProtocolError::InvalidMessage)?,
    );
    if payload.len() > MAX_SANDBOX_CONTROL_FRAME_BYTES {
        return Err(SandboxControlProtocolError::InvalidFrame);
    }
    let length =
        u32::try_from(payload.len()).map_err(|_| SandboxControlProtocolError::InvalidFrame)?;
    writer
        .write_all(&length.to_be_bytes())
        .await
        .map_err(|_| SandboxControlProtocolError::Transport)?;
    writer
        .write_all(&payload)
        .await
        .map_err(|_| SandboxControlProtocolError::Transport)?;
    writer
        .flush()
        .await
        .map_err(|_| SandboxControlProtocolError::Transport)
}

pub async fn read_frame<R, T>(reader: &mut R) -> Result<T, SandboxControlProtocolError>
where
    R: AsyncRead + Unpin + ?Sized,
    T: DeserializeOwned,
{
    let mut encoded_length = [0_u8; 4];
    reader
        .read_exact(&mut encoded_length)
        .await
        .map_err(|_| SandboxControlProtocolError::Transport)?;
    let length = usize::try_from(u32::from_be_bytes(encoded_length))
        .ok()
        .filter(|length| *length <= MAX_SANDBOX_CONTROL_FRAME_BYTES)
        .ok_or(SandboxControlProtocolError::InvalidFrame)?;
    let mut payload = Zeroizing::new(vec![0_u8; length]);
    reader
        .read_exact(&mut payload)
        .await
        .map_err(|_| SandboxControlProtocolError::Transport)?;
    serde_json::from_slice(&payload).map_err(|_| SandboxControlProtocolError::InvalidMessage)
}

/// Serve exactly one request/response exchange on a provider-owned stream.
pub async fn serve_one<C>(
    channel: &mut C,
    service: &dyn SandboxControlService,
) -> Result<(), SandboxControlProtocolError>
where
    C: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    let request: SandboxControlRequest = read_frame(channel).await?;
    match &request {
        SandboxControlRequest::RepositoryGitCredentialGet { query } => query.validate()?,
    }
    let response = service.handle(request).await;
    write_frame(channel, &response).await
}

struct StartedSandboxControlChannel<'a, C: ?Sized> {
    first_byte: Option<u8>,
    channel: &'a mut C,
}

impl<C> AsyncRead for StartedSandboxControlChannel<'_, C>
where
    C: AsyncRead + Unpin + ?Sized,
{
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if buffer.remaining() > 0
            && let Some(first_byte) = self.first_byte.take()
        {
            buffer.put_slice(&[first_byte]);
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut *self.channel).poll_read(context, buffer)
    }
}

impl<C> AsyncWrite for StartedSandboxControlChannel<'_, C>
where
    C: AsyncWrite + Unpin + ?Sized,
{
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut *self.channel).poll_write(context, buffer)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.channel).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.channel).poll_shutdown(context)
    }
}

/// Complete one exchange after a provider has observed its first request byte.
///
/// Providers use this to distinguish an indefinitely idle, pre-opened channel
/// from a bounded active exchange without duplicating the framing codec.
pub async fn serve_one_after_first_byte<C>(
    channel: &mut C,
    first_byte: u8,
    service: &dyn SandboxControlService,
) -> Result<(), SandboxControlProtocolError>
where
    C: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    serve_one(
        &mut StartedSandboxControlChannel {
            first_byte: Some(first_byte),
            channel,
        },
        service,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awaken_sandbox_control::{
        CapabilityExpiresAtUnixMs, RepositoryGitCredentialQuery, SandboxControlResponse,
        SandboxControlSecret,
    };

    struct CountingService(std::sync::atomic::AtomicUsize);

    #[async_trait]
    impl SandboxControlService for CountingService {
        async fn handle(&self, _request: SandboxControlRequest) -> SandboxControlResponse {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            SandboxControlResponse::Unavailable
        }
    }

    #[tokio::test]
    async fn bounded_codec_is_closed_and_redacts_credential_debug() {
        /*
         * R8/R11/R14 cause/effect decision table:
         * C1=request is exact HTTPS host/path; C2=unknown message field or
         * invalid protocol; C3=frame is over the fixed bound; C4=response holds
         * a virtual capability. E1=lossless one-request decode; E2=reject before
         * handler; E3=no allocation beyond the bound; E4=Debug/error reveal no
         * username/password while the private wire remains lossless.
         * Rules: R8 C1&&!C2=>E1; R11 C2=>E2; R14 C3=>E3, C4=>E4.
         */
        let request = SandboxControlRequest::RepositoryGitCredentialGet {
            query: RepositoryGitCredentialQuery {
                protocol: "https".into(),
                host: "gateway.example.test".into(),
                path: "git/repository-a".into(),
            },
        };
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        write_frame(&mut writer, &request).await.unwrap();
        let decoded: SandboxControlRequest = read_frame(&mut reader).await.unwrap();
        assert_eq!(decoded, request, "R8/E1");

        let service = CountingService(std::sync::atomic::AtomicUsize::new(0));
        for invalid in [
            serde_json::json!({
                "type": "repository_git_credential_get",
                "query": {
                    "protocol": "http",
                    "host": "gateway.example.test",
                    "path": "git/repository-a"
                }
            }),
            serde_json::json!({
                "type": "repository_git_credential_get",
                "query": {
                    "protocol": "https",
                    "host": "gateway.example.test",
                    "path": "git/repository-a",
                    "unknown": true
                }
            }),
        ] {
            let (mut client, mut server) = tokio::io::duplex(1024);
            write_frame(&mut client, &invalid).await.unwrap();
            assert!(serve_one(&mut server, &service).await.is_err(), "R11/E2");
        }
        assert_eq!(
            service.0.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "R11/E2 handler never observes a rejected request"
        );

        let credential = SandboxControlResponse::RepositoryGitCredential {
            username: SandboxControlSecret::new("git-user").unwrap(),
            password: SandboxControlSecret::new("virtual-capability").unwrap(),
            expires_at_unix_ms: CapabilityExpiresAtUnixMs(500),
        };
        let debug = format!("{credential:?}");
        assert!(!debug.contains("git-user"), "R14/E4");
        assert!(!debug.contains("virtual-capability"), "R14/E4");
        let encoded = serde_json::to_vec(&credential).unwrap();
        let decoded: SandboxControlResponse = serde_json::from_slice(&encoded).unwrap();
        let SandboxControlResponse::RepositoryGitCredential {
            username, password, ..
        } = decoded
        else {
            panic!("R14/E4 credential response")
        };
        assert_eq!(username.expose_secret(), "git-user", "R14/E4 private wire");
        assert_eq!(
            password.expose_secret(),
            "virtual-capability",
            "R14/E4 private wire"
        );

        let oversized = u32::try_from(MAX_SANDBOX_CONTROL_FRAME_BYTES + 1)
            .unwrap()
            .to_be_bytes();
        let (mut writer, mut reader) = tokio::io::duplex(8);
        writer.write_all(&oversized).await.unwrap();
        assert_eq!(
            read_frame::<_, SandboxControlRequest>(&mut reader).await,
            Err(SandboxControlProtocolError::InvalidFrame),
            "R14/E3"
        );
    }
}
