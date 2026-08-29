//! Closed, provider-neutral control protocol for one live Sandbox.
//!
//! This crate owns only the bounded request/response codec and the publication
//! ports. It owns no Session, claim, credential, repository, policy, cache, or
//! persistence state. A [`SandboxControlService`] must resolve every request
//! against its owning application's current authority.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::{Zeroize, Zeroizing};

/// Hard upper bound for one control request or response.
pub const MAX_SANDBOX_CONTROL_FRAME_BYTES: usize = 64 * 1024;

/// Runtime-owned rendezvous directory. It is outside the repository/workspace
/// namespace and is projected read-only into untrusted workloads.
pub const SANDBOX_CONTROL_DIRECTORY_PATH: &str = "/run/awaken/control";

/// One stable in-Sandbox endpoint shared by Namespace and hosted Kubernetes.
/// Providers project it differently outside the Sandbox, but Agent/Hand
/// processes inherit one transport-independent helper overlay.
pub const REPOSITORY_GIT_CREDENTIAL_SOCKET_PATH: &str =
    "/run/awaken/control/repository-git-credential.sock";

/// Sidecar-private readiness evidence adjacent to the stable logical socket.
/// The workload can read but never write this file; only the trusted forwarder
/// creates it after both of its listeners are bound.
pub const REPOSITORY_GIT_CREDENTIAL_READY_MARKER_PATH: &str =
    "/run/awaken/control/repository-git-credential.ready";

/// Closed service families a Sandbox provider may publish.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxControlServiceKind {
    RepositoryGitCredential,
}

/// Exact rewritten HTTPS coordinate supplied by Git's credential-helper wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryGitCredentialQuery {
    pub protocol: String,
    pub host: String,
    pub path: String,
}

impl RepositoryGitCredentialQuery {
    /// Reject ambiguous or line-oriented values before they reach an authority.
    pub fn validate(&self) -> Result<(), SandboxControlProtocolError> {
        if self.protocol != "https"
            || self.host.trim().is_empty()
            || self.path.trim_matches('/').is_empty()
            || [&self.protocol, &self.host, &self.path]
                .into_iter()
                .any(|value| {
                    value
                        .chars()
                        .any(|character| matches!(character, '\r' | '\n' | '\0'))
                })
        {
            return Err(SandboxControlProtocolError::InvalidMessage);
        }
        Ok(())
    }
}

/// One request on the closed Sandbox control port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SandboxControlRequest {
    RepositoryGitCredentialGet { query: RepositoryGitCredentialQuery },
}

/// Exact wall-clock upper bound supplied by the capability issuer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapabilityExpiresAtUnixMs(pub u64);

impl CapabilityExpiresAtUnixMs {
    #[must_use]
    pub const fn is_live_at(self, now_unix_ms: u64) -> bool {
        self.0 > now_unix_ms
    }
}

/// Secret bytes used only on the private, process-local Sandbox control wire.
///
/// Unlike durable/public contracts, this type is intentionally serializable so
/// the provider-neutral transport can deliver one freshly verified capability
/// to Git. It is redacted in operator surfaces and zeroized on drop.
#[derive(Serialize)]
#[serde(transparent)]
pub struct SandboxControlSecret(String);

impl SandboxControlSecret {
    pub fn new(value: impl Into<String>) -> Result<Self, SandboxControlProtocolError> {
        let mut value = value.into();
        if value.is_empty()
            || value
                .chars()
                .any(|character| matches!(character, '\r' | '\n' | '\0'))
        {
            value.zeroize();
            return Err(SandboxControlProtocolError::InvalidMessage);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for SandboxControlSecret {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = <String as Deserialize>::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

impl Drop for SandboxControlSecret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for SandboxControlSecret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SandboxControlSecret([redacted])")
    }
}

/// One response from the closed Sandbox control port.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SandboxControlResponse {
    RepositoryGitCredential {
        username: SandboxControlSecret,
        password: SandboxControlSecret,
        expires_at_unix_ms: CapabilityExpiresAtUnixMs,
    },
    Denied,
    Unavailable,
}

impl std::fmt::Debug for SandboxControlResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RepositoryGitCredential {
                expires_at_unix_ms, ..
            } => formatter
                .debug_struct("RepositoryGitCredential")
                .field("username", &"[REDACTED]")
                .field("password", &"[REDACTED]")
                .field("expires_at_unix_ms", expires_at_unix_ms)
                .finish(),
            Self::Denied => formatter.write_str("Denied"),
            Self::Unavailable => formatter.write_str("Unavailable"),
        }
    }
}

/// Application-owned request handler. Implementations must perform a fresh
/// authority read; this port deliberately offers no seed/store/cache method.
#[async_trait]
pub trait SandboxControlService: Send + Sync {
    async fn handle(&self, request: SandboxControlRequest) -> SandboxControlResponse;
}

/// Session-owned publication lease. Closing it revokes the provider transport;
/// it does not mutate application authority.
#[async_trait]
pub trait PublishedSandboxControlService: Send + Sync {
    async fn close(&self);
}

/// Provider capability for publishing one application-owned control handler.
#[async_trait]
pub trait SandboxControlServicePublisher: Send + Sync {
    async fn publish_sandbox_control_service(
        &self,
        kind: SandboxControlServiceKind,
        service: Arc<dyn SandboxControlService>,
    ) -> Result<Box<dyn PublishedSandboxControlService>, SandboxControlPublishError>;
}

#[derive(Debug, thiserror::Error)]
#[error("Sandbox control service publication is unavailable")]
pub struct SandboxControlPublishError;

/// Stable framing failures. Payload bytes and serde diagnostics are omitted so
/// a credential-bearing response cannot enter an error or log accidentally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SandboxControlProtocolError {
    #[error("Sandbox control frame is invalid")]
    InvalidFrame,
    #[error("Sandbox control message is invalid")]
    InvalidMessage,
    #[error("Sandbox control transport is unavailable")]
    Transport,
}

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

    #[test]
    fn credential_query_rejects_non_https_and_line_injection() {
        /* Credential validation table: C1=canonical HTTPS authority and
         * nonempty LF/CR/NUL-free secret; C2=non-HTTPS/empty/query injection;
         * C3=wire-deserialized empty, CRLF, or NUL secret; C4=valid credential
         * response is formatted for diagnostics. E1=C1 validates; E2=C2 and
         * C3 reject before use; E3=C4 redacts both fields. Rules CV1 C1=>E1;
         * CV2 C2=>E2; CV3 C3=>E2; CV4 C1+C4=>E3.
         */
        let query = |protocol: &str, host: &str, path: &str| RepositoryGitCredentialQuery {
            protocol: protocol.into(),
            host: host.into(),
            path: path.into(),
        };
        assert!(
            query("https", "gateway.test", "git/repo")
                .validate()
                .is_ok()
        );
        assert!(
            query("http", "gateway.test", "git/repo")
                .validate()
                .is_err()
        );
        assert!(
            query("https", "gateway.test\npassword=x", "git/repo")
                .validate()
                .is_err()
        );
        assert!(query("https", "gateway.test", "").validate().is_err());
        assert!(SandboxControlSecret::new("line\ninjection").is_err());
        assert!(SandboxControlSecret::new("").is_err());

        for encoded in [r#""""#, r#""line\r\nbreak""#, r#""prefix\u0000suffix""#] {
            assert!(
                serde_json::from_str::<SandboxControlSecret>(encoded).is_err(),
                "CV3/E2 {encoded}"
            );
        }
        let response: SandboxControlResponse = serde_json::from_value(serde_json::json!({
            "type": "repository_git_credential",
            "username": "git-user",
            "password": "valid-capability", // awaken-allow: secret -- inert redaction fixture
            "expires_at_unix_ms": 42
        }))
        .unwrap();
        let debug = format!("{response:?}");
        assert!(debug.contains("REDACTED"), "CV4/E3 marker");
        assert!(!debug.contains("git-user"), "CV4/E3 username");
        assert!(!debug.contains("valid-capability"), "CV4/E3 password");
    }
}
