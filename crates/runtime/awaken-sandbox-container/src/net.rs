//! Remote-tier transport (ADR-0041 Slice 5, `connection` feature).
//!
//! The container/pod agent runs as the container's main process; the runtime reaches
//! its stdio over the network. This module establishes that [`AgentChannel`] via
//! `awaken-connection`, in two modes:
//!
//! - **Direct dial** ([`TcpAgentTransport`]) for a published port (Docker `-p`,
//!   k8s Service) with bearer-token authentication.
//! - **Reverse dial** ([`bind_reverse`] / [`SecureReverseListen`]) for a
//!   firewalled/outbound-only pod where the sandbox dials out to a rendezvous the
//!   host listens on.
//!
//! ## Plan layer (TLS + token)
//!
//! [`SecureReverseListen`] + [`SecureReverseDial`] are the plan-layer types:
//!
//! - The host binds a TLS rendezvous listener with a [`ServerIdentity`] (cert+key).
//! - The host serialises [`RendezvousCoord`] and delivers it to the pod via the
//!   control plane (environment variable, provisioning secret, etc.).
//! - The pod constructs a [`SecureReverseDial`], connects to the rendezvous address,
//!   and verifies the host's TLS cert by its SHA-256 fingerprint ([`CertFingerprint`]).
//! - After TLS, the pod sends its [`ChannelToken`]; the host validates it.
//!
//! Direct-dial uses [`TokenAgentTransport`], which performs the same token handshake
//! over a plain TCP connection (traffic is intra-cluster or loopback-protected).

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use awaken_agent_channel::{AgentChannel, AgentTransport, ChannelError};
use awaken_connection::{Channel, ConnectError, DialEnd, ListenSide, Transport, bind_pair};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, ServerConfig, SignatureScheme};
use tokio_rustls::{TlsAcceptor, TlsConnector};

// ── Bare channel (shared by all transport types) ─────────────────────────────

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

// ── Bare TCP transport (core-only, no plan layer) ─────────────────────────────

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

/// Direct-dial agent transport: reaches a published container port. No TLS; use in
/// loopback/intra-cluster environments where the network layer provides isolation.
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

// ── Bare reverse dial ─────────────────────────────────────────────────────────

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

/// Pair a firewalled sandbox's outbound dial with the host's rendezvous listener.
/// Returns `(host_channel, sandbox_channel)`. Binds on `bind_addr` (`:0` is fine)
/// and dials the *resolved* port.
pub async fn bind_reverse(bind_addr: SocketAddr) -> Result<(TcpChannel, TcpChannel), ConnectError> {
    let listen = ReverseListen::bind(bind_addr).await?;
    let dial_addr = listen.local_addr()?;
    bind_pair(&ReverseDial, &listen, dial_addr, ()).await
}

// ── Plan-layer types ──────────────────────────────────────────────────────────

/// Errors raised in the plan layer (credential validation and TLS/token handshake).
#[derive(Debug, thiserror::Error)]
pub enum ChannelPlanError {
    /// Bearer token is empty or contains ASCII control bytes.
    #[error("bearer token must not be empty or contain control bytes")]
    InvalidToken,
    /// TLS cert or key material is malformed or the identity cannot be loaded.
    #[error("TLS identity error: {0}")]
    Identity(String),
    /// TLS handshake failed.
    #[error("TLS handshake failed: {0}")]
    Tls(String),
    /// Token handshake failed (wrong token or I/O error during the exchange).
    #[error("token handshake failed: {0}")]
    TokenHandshake(String),
    /// Underlying I/O error (TCP connect / bind failed).
    #[error("I/O error: {0}")]
    Io(String),
}

impl From<ChannelPlanError> for ConnectError {
    fn from(e: ChannelPlanError) -> Self {
        match e {
            ChannelPlanError::InvalidToken | ChannelPlanError::Identity(_) => {
                ConnectError::Setup(e.to_string())
            }
            ChannelPlanError::Tls(_)
            | ChannelPlanError::TokenHandshake(_)
            | ChannelPlanError::Io(_) => ConnectError::Io(e.to_string()),
        }
    }
}

/// A bearer token that authenticates a remote channel connection.
///
/// Non-empty, no ASCII control bytes. Presented over TLS after the handshake.
#[derive(Clone)]
pub struct ChannelToken(String);

impl ChannelToken {
    /// Validate and wrap `s` as a `ChannelToken`.
    pub fn new(s: impl Into<String>) -> Result<Self, ChannelPlanError> {
        let s = s.into();
        if s.trim().is_empty() || s.bytes().any(|b| b.is_ascii_control()) {
            return Err(ChannelPlanError::InvalidToken);
        }
        Ok(Self(s))
    }

    /// Expose the token string. Handle carefully — this is secret material.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ChannelToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChannelToken(<redacted>)")
    }
}

/// SHA-256 fingerprint of a DER-encoded TLS leaf certificate.
///
/// Used to pin the host's TLS identity in [`RendezvousCoord`] so the pod can
/// verify the server cert without a trusted CA chain.
#[derive(Clone, PartialEq, Eq)]
pub struct CertFingerprint([u8; 32]);

impl CertFingerprint {
    /// Compute the SHA-256 fingerprint of a DER-encoded certificate.
    pub fn of_der(cert_der: &[u8]) -> Self {
        let hash = Sha256::digest(cert_der);
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&hash);
        Self(arr)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<[u8; 32]> for CertFingerprint {
    fn from(arr: [u8; 32]) -> Self {
        Self(arr)
    }
}

impl std::fmt::Debug for CertFingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CertFingerprint({:x?})", &self.0[..4])
    }
}

/// DER-encoded TLS server identity: cert + PKCS#8 private key.
///
/// Held by the host only; never sent to the pod (only the cert's fingerprint travels).
pub struct ServerIdentity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
}

impl ServerIdentity {
    /// Compute the fingerprint of this identity's certificate.
    pub fn fingerprint(&self) -> CertFingerprint {
        CertFingerprint::of_der(&self.cert_der)
    }
}

/// Rendezvous coordinates the control plane delivers to a firewalled pod.
///
/// The pod deserialises this (e.g. from an injected environment variable or a
/// provisioning secret file) and calls [`SecureReverseDial::dial_coord`] to
/// connect back to the host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RendezvousCoord {
    /// Host address the pod should dial (the host's rendezvous TCP listener).
    pub addr: SocketAddr,
    /// Bearer token the pod presents to authenticate itself.
    pub token: String,
    /// SHA-256 fingerprint of the host's TLS leaf cert; the pod pins to it.
    pub server_fingerprint: [u8; 32],
}

impl RendezvousCoord {
    /// Validate and extract the bearer token.
    pub fn channel_token(&self) -> Result<ChannelToken, ChannelPlanError> {
        ChannelToken::new(&self.token)
    }

    /// Extract the cert fingerprint.
    pub fn fingerprint(&self) -> CertFingerprint {
        CertFingerprint(self.server_fingerprint)
    }
}

// ── TLS channel ───────────────────────────────────────────────────────────────

/// A TLS-wrapped `TcpStream` presented as an `awaken-connection` [`Channel`].
///
/// All I/O is encrypted; the token handshake happens *after* TLS.
pub struct SecureTcpChannel(tokio_rustls::TlsStream<TcpStream>);

impl Channel for SecureTcpChannel {}

impl AsyncRead for SecureTcpChannel {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl AsyncWrite for SecureTcpChannel {
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

// ── Token handshake helpers ───────────────────────────────────────────────────

/// Maximum token line length (bytes). Guards against unbounded reads.
const MAX_TOKEN_LINE: usize = 512;

/// Client side: write `<token>\n` into `stream` after TLS.
async fn send_token<S>(stream: &mut S, token: &ChannelToken) -> Result<(), ChannelPlanError>
where
    S: AsyncWrite + Unpin,
{
    let mut line = token.expose().as_bytes().to_vec();
    line.push(b'\n');
    stream
        .write_all(&line)
        .await
        .map_err(|e| ChannelPlanError::TokenHandshake(e.to_string()))?;
    stream
        .flush()
        .await
        .map_err(|e| ChannelPlanError::TokenHandshake(e.to_string()))
}

/// Server side: read `<token>\n`, validate against `expected`. Returns an error if
/// the token is wrong or the line exceeds [`MAX_TOKEN_LINE`].
async fn receive_and_validate_token<S>(
    stream: &mut S,
    expected: &ChannelToken,
) -> Result<(), ChannelPlanError>
where
    S: AsyncRead + Unpin,
{
    let mut buf = Vec::with_capacity(MAX_TOKEN_LINE + 1);
    // Read byte-by-byte until '\n' or we exceed the max. Avoids the BufReader
    // ownership dance and works for any AsyncRead without a wrapper.
    loop {
        let mut byte = [0u8; 1];
        let n = stream
            .read(&mut byte)
            .await
            .map_err(|e| ChannelPlanError::TokenHandshake(e.to_string()))?;
        if n == 0 {
            return Err(ChannelPlanError::TokenHandshake(
                "connection closed before token".into(),
            ));
        }
        if byte[0] == b'\n' {
            break;
        }
        buf.push(byte[0]);
        if buf.len() > MAX_TOKEN_LINE {
            return Err(ChannelPlanError::TokenHandshake(
                "token line too long".into(),
            ));
        }
    }
    // Strip optional '\r' before '\n'.
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    let received = std::str::from_utf8(&buf)
        .map_err(|_| ChannelPlanError::TokenHandshake("non-UTF-8 token".into()))?;
    if received != expected.expose() {
        return Err(ChannelPlanError::TokenHandshake("token mismatch".into()));
    }
    Ok(())
}

// ── TLS helpers ───────────────────────────────────────────────────────────────

fn ring_provider() -> Arc<tokio_rustls::rustls::crypto::CryptoProvider> {
    Arc::new(tokio_rustls::rustls::crypto::ring::default_provider())
}

fn make_server_config(identity: &ServerIdentity) -> Result<ServerConfig, ChannelPlanError> {
    let cert = CertificateDer::from(identity.cert_der.clone());
    let key = PrivateKeyDer::try_from(identity.key_der.clone())
        .map_err(|e| ChannelPlanError::Identity(e.to_string()))?;
    ServerConfig::builder_with_provider(ring_provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| ChannelPlanError::Identity(e.to_string()))?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(|e| ChannelPlanError::Identity(e.to_string()))
}

fn make_client_config(fingerprint: CertFingerprint) -> Result<ClientConfig, ChannelPlanError> {
    ClientConfig::builder_with_provider(ring_provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| ChannelPlanError::Tls(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(FingerprintVerifier(fingerprint)))
        .with_no_client_auth()
        .pipe_ok()
}

trait PipeOk: Sized {
    fn pipe_ok(self) -> Result<Self, ChannelPlanError>;
}

impl PipeOk for ClientConfig {
    fn pipe_ok(self) -> Result<Self, ChannelPlanError> {
        Ok(self)
    }
}

// ── Fingerprint-pinning TLS server cert verifier ──────────────────────────────

/// Custom TLS server cert verifier: accepts any cert whose SHA-256 fingerprint
/// matches the expected value. The host's TLS identity is pinned in
/// [`RendezvousCoord::server_fingerprint`] and delivered via the control plane.
#[derive(Debug)]
struct FingerprintVerifier(CertFingerprint);

impl tokio_rustls::rustls::client::danger::ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: tokio_rustls::rustls::pki_types::UnixTime,
    ) -> Result<tokio_rustls::rustls::client::danger::ServerCertVerified, tokio_rustls::rustls::Error>
    {
        let actual = CertFingerprint::of_der(end_entity.as_ref());
        if actual == self.0 {
            Ok(tokio_rustls::rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(tokio_rustls::rustls::Error::General(
                "TLS cert fingerprint mismatch: the host identity was not as expected".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        // Verify the server's handshake signature using the ring provider's
        // supported algorithms. This ensures the server actually holds the
        // private key corresponding to the pinned cert.
        tokio_rustls::rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &ring_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        tokio_rustls::rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &ring_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        ring_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

// ── Secure reverse-dial plan layer ───────────────────────────────────────────

/// Host side of the plan-layer reverse-dial rendezvous.
///
/// Binds a TLS listener. The host:
/// 1. Constructs `SecureReverseListen` with a [`ServerIdentity`] and a
///    [`ChannelToken`].
/// 2. Serialises [`RendezvousCoord`] (from [`SecureReverseListen::rendezvous_coord`])
///    and delivers it to the pod via the control plane.
/// 3. Calls `accept()` to wait for the pod's reverse dial.
pub struct SecureReverseListen {
    listener: TcpListener,
    acceptor: TlsAcceptor,
    fingerprint: CertFingerprint,
    expected_token: ChannelToken,
}

impl SecureReverseListen {
    /// Bind a TLS rendezvous listener on `addr`.
    ///
    /// `identity` is the host's TLS server cert + key. `token` is the bearer token
    /// the pod must send after the TLS handshake.
    pub async fn bind(
        addr: SocketAddr,
        identity: &ServerIdentity,
        token: ChannelToken,
    ) -> Result<Self, ChannelPlanError> {
        let fingerprint = identity.fingerprint();
        let config = make_server_config(identity)?;
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| ChannelPlanError::Io(e.to_string()))?;
        Ok(Self {
            listener,
            acceptor,
            fingerprint,
            expected_token: token,
        })
    }

    /// The address this listener is bound to (resolves ephemeral `:0`).
    pub fn local_addr(&self) -> Result<SocketAddr, ChannelPlanError> {
        self.listener
            .local_addr()
            .map_err(|e| ChannelPlanError::Io(e.to_string()))
    }

    /// The SHA-256 fingerprint of the server cert, for embedding in
    /// [`RendezvousCoord`].
    pub fn fingerprint(&self) -> &CertFingerprint {
        &self.fingerprint
    }

    /// Build the [`RendezvousCoord`] the control plane should deliver to the pod.
    ///
    /// `external_addr` overrides the listener's local address when the host is
    /// behind NAT or the pod needs to reach a different interface.
    pub fn rendezvous_coord(&self, external_addr: SocketAddr) -> RendezvousCoord {
        RendezvousCoord {
            addr: external_addr,
            token: self.expected_token.expose().to_string(),
            server_fingerprint: *self.fingerprint.as_bytes(),
        }
    }

    /// Build a [`RendezvousCoord`] using this listener's local address (loopback /
    /// same-host use; pods on the same host as the runtime).
    pub fn rendezvous_coord_local(&self) -> Result<RendezvousCoord, ChannelPlanError> {
        Ok(self.rendezvous_coord(self.local_addr()?))
    }
}

#[async_trait]
impl ListenSide for SecureReverseListen {
    type Channel = SecureTcpChannel;
    type Peer = SecureReverseDial;

    /// Accept one inbound reverse-dial connection: complete TLS handshake then
    /// validate the pod's bearer token. Returns an error if the pod presents
    /// the wrong token or TLS fails.
    async fn accept(&self) -> Result<SecureTcpChannel, ConnectError> {
        let (tcp, _peer) = self
            .listener
            .accept()
            .await
            .map_err(|e| ConnectError::Io(e.to_string()))?;
        let tls = self
            .acceptor
            .accept(tcp)
            .await
            .map_err(|e| ConnectError::Io(e.to_string()))?;

        let mut channel = SecureTcpChannel(tokio_rustls::TlsStream::Server(tls));
        receive_and_validate_token(&mut channel, &self.expected_token)
            .await
            .map_err(|e| ConnectError::Io(e.to_string()))?;
        Ok(channel)
    }
}

/// Pod side of the plan-layer reverse-dial rendezvous.
///
/// Constructed from a [`RendezvousCoord`] delivered by the control plane. The pod:
/// 1. Dials the host's rendezvous address.
/// 2. Performs the TLS handshake, verifying the server cert by fingerprint.
/// 3. Sends the bearer token.
pub struct SecureReverseDial {
    coord: RendezvousCoord,
}

impl SecureReverseDial {
    /// Build a dial end from a [`RendezvousCoord`] delivered by the control plane.
    pub fn from_coord(coord: RendezvousCoord) -> Self {
        Self { coord }
    }

    /// Dial the rendezvous directly from the embedded coord address.
    ///
    /// Convenience wrapper over [`DialEnd::dial`] for the common in-process case
    /// (tests, same-host sandbox).
    pub async fn dial_coord(&self) -> Result<SecureTcpChannel, ChannelPlanError> {
        self.dial_addr(self.coord.addr).await
    }

    async fn dial_addr(&self, addr: SocketAddr) -> Result<SecureTcpChannel, ChannelPlanError> {
        let token = self.coord.channel_token()?;
        let tcp = TcpStream::connect(addr)
            .await
            .map_err(|e| ChannelPlanError::Io(e.to_string()))?;

        let config = make_client_config(self.coord.fingerprint())?;
        let connector = TlsConnector::from(Arc::new(config));
        let server_name: ServerName<'static> = ServerName::try_from("awaken-rendezvous")
            .map_err(|e| ChannelPlanError::Tls(e.to_string()))?;
        let tls = connector
            .connect(server_name, tcp)
            .await
            .map_err(|e| ChannelPlanError::Tls(e.to_string()))?;

        let mut channel = SecureTcpChannel(tokio_rustls::TlsStream::Client(tls));
        send_token(&mut channel, &token).await?;
        Ok(channel)
    }
}

#[async_trait]
impl DialEnd for SecureReverseDial {
    type Channel = SecureTcpChannel;
    type Address = SocketAddr;
    type DialMaterial = RendezvousCoord;
    type Peer = SecureReverseListen;

    async fn dial(
        &self,
        addr: SocketAddr,
        material: RendezvousCoord,
    ) -> Result<SecureTcpChannel, ConnectError> {
        let dial = SecureReverseDial::from_coord(material);
        dial.dial_addr(addr).await.map_err(ConnectError::from)
    }
}

/// Establish a plan-layer secure reverse-dial pair: the host's TLS rendezvous
/// listener and the pod's TLS client both complete in the same process. Returns
/// `(host_channel, pod_channel)`.
///
/// Binds on `bind_addr` (`:0` is fine) and delivers the rendezvous coord to the pod
/// so both sides use the *resolved* address.
pub async fn bind_secure_reverse(
    bind_addr: SocketAddr,
    identity: &ServerIdentity,
    token: ChannelToken,
) -> Result<(SecureTcpChannel, SecureTcpChannel), ChannelPlanError> {
    let listen = SecureReverseListen::bind(bind_addr, identity, token.clone()).await?;
    let coord = listen.rendezvous_coord_local()?;
    let dialer = SecureReverseDial::from_coord(coord);

    let (accept_res, dial_res) = tokio::join!(listen.accept(), dialer.dial_coord());
    let accepted = accept_res.map_err(|e| ChannelPlanError::Io(e.to_string()))?;
    let dialed = dial_res?;

    Ok((accepted, dialed))
}

// ── Direct-dial with token authentication ─────────────────────────────────────

/// Direct-dial agent transport with bearer-token authentication.
///
/// After TCP connect the host sends the bearer token so the container agent can
/// validate the caller. Traffic is unencrypted; use in loopback / intra-cluster
/// environments. For TLS direct-dial the container agent would need to expose
/// its cert fingerprint during provisioning (future work).
pub struct TokenAgentTransport {
    addr: SocketAddr,
    token: ChannelToken,
}

impl TokenAgentTransport {
    pub fn new(addr: SocketAddr, token: ChannelToken) -> Self {
        Self { addr, token }
    }
}

#[async_trait]
impl AgentTransport for TokenAgentTransport {
    async fn open_channel(&self) -> Result<Box<dyn AgentChannel>, ChannelError> {
        let mut stream = TcpStream::connect(self.addr)
            .await
            .map_err(|e| ChannelError::Setup(e.to_string()))?;
        send_token(&mut stream, &self.token)
            .await
            .map_err(|e| ChannelError::Setup(e.to_string()))?;
        Ok(Box::new(TcpChannel(stream)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

    fn loopback() -> SocketAddr {
        "127.0.0.1:0".parse().unwrap()
    }

    // ── Bare TCP tests ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn direct_dial_opens_a_duplex_agent_channel() {
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
        let listen = ReverseListen::bind(loopback()).await.unwrap();
        let addr = listen.local_addr().unwrap();
        let (mut host, mut sandbox) = bind_pair(&ReverseDial, &listen, addr, ()).await.unwrap();

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
        let addr = {
            let l = TcpListener::bind(loopback()).await.unwrap();
            l.local_addr().unwrap()
        };
        assert!(TcpAgentTransport::new(addr).open_channel().await.is_err());
    }

    // ── Plan-layer type validation ─────────────────────────────────────────────

    #[test]
    fn channel_token_rejects_empty_and_control_bytes() {
        assert!(ChannelToken::new("").is_err());
        assert!(ChannelToken::new("   ").is_err());
        assert!(ChannelToken::new("bad\ntoken").is_err());
        assert!(ChannelToken::new("bad\x00token").is_err());
        assert!(ChannelToken::new("valid-token-abc123").is_ok());
    }

    #[test]
    fn channel_token_debug_redacts_value() {
        let tok = ChannelToken::new("secret").unwrap();
        assert!(!format!("{tok:?}").contains("secret"));
    }

    #[test]
    fn cert_fingerprint_of_der_is_sha256() {
        let data = b"fake-der-bytes";
        let fp = CertFingerprint::of_der(data);
        let expected = Sha256::digest(data);
        assert_eq!(fp.as_bytes(), expected.as_slice());
    }

    #[test]
    fn rendezvous_coord_round_trips_json() {
        let coord = RendezvousCoord {
            addr: "127.0.0.1:9999".parse().unwrap(),
            token: "tok123".into(),
            server_fingerprint: [0xab; 32],
        };
        let json = serde_json::to_string(&coord).unwrap();
        let back: RendezvousCoord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.addr, coord.addr);
        assert_eq!(back.token, coord.token);
        assert_eq!(back.server_fingerprint, coord.server_fingerprint);
    }

    #[test]
    fn rendezvous_coord_channel_token_validates() {
        let good = RendezvousCoord {
            addr: "127.0.0.1:1".parse().unwrap(),
            token: "good-token".into(),
            server_fingerprint: [0u8; 32],
        };
        assert!(good.channel_token().is_ok());

        let bad = RendezvousCoord {
            addr: "127.0.0.1:1".parse().unwrap(),
            token: String::new(),
            server_fingerprint: [0u8; 32],
        };
        assert!(bad.channel_token().is_err());
    }

    #[test]
    fn channel_plan_error_maps_to_connect_error() {
        let io_err = ConnectError::from(ChannelPlanError::Io("connect refused".into()));
        assert!(matches!(io_err, ConnectError::Io(_)));
        let setup_err = ConnectError::from(ChannelPlanError::InvalidToken);
        assert!(matches!(setup_err, ConnectError::Setup(_)));
    }

    // ── TLS/token integration tests ───────────────────────────────────────────

    /// A pre-generated self-signed P-256 cert (CN=awaken-test, SAN=127.0.0.1)
    /// and its PKCS#8 private key, both DER-encoded. Used so the test suite
    /// does not need a cert-generation dependency at build time.
    const TEST_CERT_DER: &[u8] = &[
        0x30, 0x82, 0x01, 0x94, 0x30, 0x82, 0x01, 0x3a, 0xa0, 0x03, 0x02, 0x01, 0x02, 0x02, 0x14,
        0x57, 0x5f, 0xa9, 0xf7, 0x9b, 0x7f, 0x85, 0xd1, 0x78, 0xaf, 0x93, 0x59, 0x03, 0x45, 0x1d,
        0xdb, 0x76, 0x56, 0x0e, 0x5e, 0x30, 0x0a, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04,
        0x03, 0x02, 0x30, 0x16, 0x31, 0x14, 0x30, 0x12, 0x06, 0x03, 0x55, 0x04, 0x03, 0x0c, 0x0b,
        0x61, 0x77, 0x61, 0x6b, 0x65, 0x6e, 0x2d, 0x74, 0x65, 0x73, 0x74, 0x30, 0x20, 0x17, 0x0d,
        0x32, 0x36, 0x30, 0x37, 0x30, 0x34, 0x30, 0x39, 0x33, 0x35, 0x31, 0x30, 0x5a, 0x18, 0x0f,
        0x32, 0x31, 0x32, 0x36, 0x30, 0x36, 0x31, 0x30, 0x30, 0x39, 0x33, 0x35, 0x31, 0x30, 0x5a,
        0x30, 0x16, 0x31, 0x14, 0x30, 0x12, 0x06, 0x03, 0x55, 0x04, 0x03, 0x0c, 0x0b, 0x61, 0x77,
        0x61, 0x6b, 0x65, 0x6e, 0x2d, 0x74, 0x65, 0x73, 0x74, 0x30, 0x59, 0x30, 0x13, 0x06, 0x07,
        0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03,
        0x01, 0x07, 0x03, 0x42, 0x00, 0x04, 0x59, 0x57, 0x56, 0x44, 0x3d, 0x8b, 0x42, 0x12, 0x62,
        0x3a, 0x36, 0x8b, 0x2d, 0x3c, 0xa4, 0xa2, 0x00, 0xbb, 0xb2, 0xc2, 0xd0, 0x92, 0x63, 0xd2,
        0x1e, 0xc9, 0xb9, 0x5d, 0x63, 0xe5, 0x2f, 0xfb, 0x54, 0x6f, 0xd6, 0xfd, 0x61, 0x69, 0xc9,
        0xd8, 0xdc, 0xaa, 0xd8, 0x35, 0xe9, 0x1c, 0x8d, 0xf0, 0xef, 0xf5, 0xc9, 0x3c, 0xf3, 0x9a,
        0xe0, 0x5c, 0xca, 0x2a, 0x2a, 0xf0, 0x92, 0x3e, 0xd7, 0xc4, 0xa3, 0x64, 0x30, 0x62, 0x30,
        0x1d, 0x06, 0x03, 0x55, 0x1d, 0x0e, 0x04, 0x16, 0x04, 0x14, 0xcb, 0x71, 0xc7, 0x1e, 0x9d,
        0x3e, 0x99, 0xb2, 0x77, 0x1d, 0x59, 0xb4, 0x49, 0x9b, 0xed, 0x17, 0xf1, 0xa8, 0x49, 0x0f,
        0x30, 0x1f, 0x06, 0x03, 0x55, 0x1d, 0x23, 0x04, 0x18, 0x30, 0x16, 0x80, 0x14, 0xcb, 0x71,
        0xc7, 0x1e, 0x9d, 0x3e, 0x99, 0xb2, 0x77, 0x1d, 0x59, 0xb4, 0x49, 0x9b, 0xed, 0x17, 0xf1,
        0xa8, 0x49, 0x0f, 0x30, 0x0f, 0x06, 0x03, 0x55, 0x1d, 0x13, 0x01, 0x01, 0xff, 0x04, 0x05,
        0x30, 0x03, 0x01, 0x01, 0xff, 0x30, 0x0f, 0x06, 0x03, 0x55, 0x1d, 0x11, 0x04, 0x08, 0x30,
        0x06, 0x87, 0x04, 0x7f, 0x00, 0x00, 0x01, 0x30, 0x0a, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce,
        0x3d, 0x04, 0x03, 0x02, 0x03, 0x48, 0x00, 0x30, 0x45, 0x02, 0x21, 0x00, 0xea, 0x76, 0x91,
        0x6c, 0xe3, 0x7f, 0x7d, 0xba, 0x8a, 0x02, 0xba, 0x87, 0x55, 0x7b, 0x6d, 0x60, 0x7b, 0x60,
        0x8d, 0x64, 0x24, 0x5b, 0xf3, 0xe9, 0xa9, 0x89, 0x8a, 0x88, 0xd0, 0x69, 0x7b, 0xc5, 0x02,
        0x20, 0x6c, 0x17, 0x1a, 0x05, 0x99, 0x83, 0x80, 0x29, 0x33, 0x85, 0xdb, 0xe8, 0x51, 0x0a,
        0xfa, 0x6f, 0x61, 0x4f, 0x4b, 0x76, 0x0e, 0x1b, 0x11, 0x86, 0x37, 0x48, 0x31, 0xfa, 0xaa,
        0x9b, 0x8d, 0x34,
    ];

    const TEST_KEY_DER: &[u8] = &[
        0x30, 0x81, 0x87, 0x02, 0x01, 0x00, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d,
        0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x04, 0x6d, 0x30,
        0x6b, 0x02, 0x01, 0x01, 0x04, 0x20, 0x74, 0x28, 0xb5, 0x3b, 0xf9, 0x95, 0x7d, 0xe3, 0x8d,
        0x9c, 0xd1, 0xda, 0xae, 0xf5, 0x4c, 0xb1, 0x6a, 0x0c, 0x87, 0xae, 0xa6, 0xd2, 0xdf, 0x4e,
        0xea, 0x11, 0x8f, 0x4a, 0x6a, 0x94, 0xff, 0x4f, 0xa1, 0x44, 0x03, 0x42, 0x00, 0x04, 0x59,
        0x57, 0x56, 0x44, 0x3d, 0x8b, 0x42, 0x12, 0x62, 0x3a, 0x36, 0x8b, 0x2d, 0x3c, 0xa4, 0xa2,
        0x00, 0xbb, 0xb2, 0xc2, 0xd0, 0x92, 0x63, 0xd2, 0x1e, 0xc9, 0xb9, 0x5d, 0x63, 0xe5, 0x2f,
        0xfb, 0x54, 0x6f, 0xd6, 0xfd, 0x61, 0x69, 0xc9, 0xd8, 0xdc, 0xaa, 0xd8, 0x35, 0xe9, 0x1c,
        0x8d, 0xf0, 0xef, 0xf5, 0xc9, 0x3c, 0xf3, 0x9a, 0xe0, 0x5c, 0xca, 0x2a, 0x2a, 0xf0, 0x92,
        0x3e, 0xd7, 0xc4,
    ];

    fn test_identity() -> ServerIdentity {
        ServerIdentity {
            cert_der: TEST_CERT_DER.to_vec(),
            key_der: TEST_KEY_DER.to_vec(),
        }
    }

    fn test_token() -> ChannelToken {
        ChannelToken::new("test-bearer-token-abc123").unwrap()
    }

    #[test]
    fn server_identity_fingerprint_matches_expected() {
        let id = test_identity();
        let fp = id.fingerprint();
        // Known SHA-256 of TEST_CERT_DER.
        let expected: [u8; 32] = [
            0xd3, 0x49, 0x09, 0xd1, 0xd5, 0xa2, 0x21, 0xba, 0xc9, 0xb0, 0x3d, 0x89, 0x1e, 0x64,
            0x53, 0x1c, 0xf1, 0xfe, 0xba, 0x31, 0x22, 0xca, 0x94, 0xdc, 0x93, 0xd0, 0x81, 0x5a,
            0x22, 0x18, 0x49, 0xad,
        ];
        assert_eq!(fp.as_bytes(), &expected);
    }

    #[tokio::test]
    async fn secure_reverse_dial_pairs_host_and_pod() {
        let id = test_identity();
        let token = test_token();
        let (mut host, mut pod) = bind_secure_reverse(loopback(), &id, token)
            .await
            .expect("secure reverse dial should succeed");

        host.write_all(b"prompt\n").await.unwrap();
        host.flush().await.unwrap();
        let mut buf = [0u8; 7];
        pod.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"prompt\n");

        pod.write_all(b"event\n").await.unwrap();
        pod.flush().await.unwrap();
        let mut evt = [0u8; 6];
        host.read_exact(&mut evt).await.unwrap();
        assert_eq!(&evt, b"event\n");
    }

    #[tokio::test]
    async fn secure_reverse_rendezvous_coord_is_populated() {
        let id = test_identity();
        let token = test_token();
        let listen = SecureReverseListen::bind(loopback(), &id, token.clone())
            .await
            .unwrap();
        let coord = listen.rendezvous_coord_local().unwrap();
        assert_eq!(coord.server_fingerprint, *id.fingerprint().as_bytes());
        assert_eq!(coord.token, token.expose());
        assert_ne!(coord.addr.port(), 0);
    }

    #[tokio::test]
    async fn wrong_token_is_rejected() {
        let id = test_identity();
        let token = test_token();
        let listen = SecureReverseListen::bind(loopback(), &id, token)
            .await
            .unwrap();
        let coord = listen.rendezvous_coord_local().unwrap();

        // Pod sends a wrong token.
        let bad_coord = RendezvousCoord {
            token: "wrong-token".into(),
            ..coord
        };
        let dialer = SecureReverseDial::from_coord(bad_coord);

        let accept_fut = listen.accept();
        let dial_fut = dialer.dial_coord();

        // At least one of them must fail (the accept or the dial).
        // The dial itself may succeed (TLS is fine), but the accept will reject it.
        let result = tokio::join!(accept_fut, dial_fut);
        let (accept_res, _dial_res) = result;
        assert!(
            accept_res.is_err(),
            "host should reject a wrong-token connection"
        );
    }

    #[tokio::test]
    async fn wrong_fingerprint_is_rejected() {
        let id = test_identity();
        let token = test_token();
        let listen = SecureReverseListen::bind(loopback(), &id, token.clone())
            .await
            .unwrap();
        let coord = listen.rendezvous_coord_local().unwrap();

        // Pod has a wrong fingerprint (zeros instead of the real fingerprint).
        let bad_coord = RendezvousCoord {
            server_fingerprint: [0u8; 32],
            ..coord
        };
        let dialer = SecureReverseDial::from_coord(bad_coord);

        // Both sides must run concurrently: the server sends its cert during the
        // TLS handshake; only after receiving the cert can the client verify (and
        // reject) the fingerprint.  Running the dial alone hangs because the TCP
        // connection enters the OS listen backlog but the server-side TLS engine
        // never starts.
        let (dial_result, _accept_result) = tokio::join!(dialer.dial_coord(), listen.accept());
        assert!(
            dial_result.is_err(),
            "pod should reject a fingerprint-mismatched TLS handshake"
        );
    }

    #[tokio::test]
    async fn token_agent_transport_sends_token() {
        let listener = TcpListener::bind(loopback()).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = tokio::io::BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            line
        });

        let transport = TokenAgentTransport::new(addr, ChannelToken::new("my-token-xyz").unwrap());
        let _chan = transport.open_channel().await.unwrap();

        let received = server.await.unwrap();
        assert_eq!(received.trim(), "my-token-xyz");
    }
}
