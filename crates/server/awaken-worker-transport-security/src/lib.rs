//! Neutral security and timing SPIs for the worker-facing HTTP boundary.
//!
//! The open runtime defines what must be trusted; a deployment decides how that
//! trust is established. The default header authenticator is suitable for local
//! and test compositions behind a trusted network. Managed deployments replace it
//! with WorkerLease/mTLS verification without changing dispatch semantics.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use awaken_run_ingress_contract::RunClaim;
use awaken_worker_contract::{WorkerDirectory, WorkerIdentity, WorkerSnapshot, WorkerState};
use axum::Json;
use axum::extract::{Request, State};
use axum::http::{StatusCode, request::Parts};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

/// Compatibility identity header used by the local HTTP client and authenticator.
pub const WORKER_ID_HEADER: &str = "x-awaken-worker-id";
/// Authorization scheme carrying a signed, short-lived Worker request assertion.
pub const SIGNED_WORKER_SCHEME: &str = "AwakenWorker";

/// Client-side counterpart of the Coordinator's Worker authenticator.
///
/// It decorates every lifecycle, dispatch, recovery, Resource and commit
/// request. The absolute path is part of the signed authority.
pub trait WorkerRequestAuthorizer: Send + Sync {
    fn authorize(
        &self,
        method: &str,
        path: &str,
        worker_id: &str,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, String>;

    fn bind_worker_identity(&self, identity: &WorkerIdentity) -> Arc<dyn WorkerRequestAuthorizer>;
}

/// Authenticate one Worker-facing HTTP request and publish the verified context
/// as a request extension for the typed route handler.
///
/// Every Worker transport router installs this same middleware with its own
/// configured authenticator. Keeping the protocol response and extension logic
/// here prevents dispatch, commit, and Resource adapters from drifting into
/// separate authentication paths.
pub async fn authenticate_worker_request(
    State(authenticator): State<Arc<dyn WorkerRequestAuthenticator>>,
    request: Request,
    next: Next,
) -> Response {
    let (parts, body) = request.into_parts();
    match authenticator.authenticate(&parts).await {
        Ok(worker) => {
            let mut request = Request::from_parts(parts, body);
            request.extensions_mut().insert(worker);
            next.run(request).await
        }
        Err(error) => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}

/// Verify the authenticated transport identity against one exact registered
/// Worker incarnation carried by an application request.
pub fn verify_worker_identity(
    worker: &VerifiedWorkerContext,
    identity: &WorkerIdentity,
) -> Result<(), String> {
    if worker.worker_id() != identity.worker_id {
        return Err("authenticated worker id does not match request identity".into());
    }
    if worker.credential_id().is_some() && worker.identity() != Some(identity) {
        return Err("authenticated worker incarnation does not match request identity".into());
    }
    Ok(())
}

/// Resolve and verify the one live Coordinator-owned registration record.
/// Dispatch, claimed commit, and per-kind Resource handlers reuse this check so
/// registry state and signed/mTLS incarnation semantics cannot drift.
pub async fn verify_current_worker_identity(
    directory: &dyn WorkerDirectory,
    worker: &VerifiedWorkerContext,
    identity: &WorkerIdentity,
    now_ms: u64,
    require_ready: bool,
) -> Result<WorkerSnapshot, String> {
    verify_worker_identity(worker, identity)?;
    let record = directory
        .current(&identity.worker_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "worker is not registered".to_string())?;
    if &record.snapshot.identity != identity {
        return Err("worker incarnation is stale".into());
    }
    if (require_ready && record.snapshot.state != WorkerState::Ready)
        || record.snapshot.expires_at_ms <= now_ms
        || record.snapshot.state == WorkerState::Dead
    {
        return Err("worker is not ready or its registry lease expired".into());
    }
    Ok(record.snapshot)
}

/// Verify that one authenticated Worker is the exact current incarnation that
/// owns a dispatch claim. Local compatibility routers may omit a directory;
/// registered production Resource routers always provide one.
pub async fn verify_claim_owner(
    directory: Option<&dyn WorkerDirectory>,
    worker: &VerifiedWorkerContext,
    identity: Option<&WorkerIdentity>,
    claim: &RunClaim,
    now_ms: u64,
) -> Result<(), String> {
    if let Some(directory) = directory {
        let identity =
            identity.ok_or_else(|| "registered Worker identity is required".to_string())?;
        verify_current_worker_identity(directory, worker, identity, now_ms, false).await?;
        return (claim.owner == identity.lease_owner())
            .then_some(())
            .ok_or_else(|| "dispatch claim owner does not match Worker incarnation".to_string());
    }
    match identity {
        Some(identity) => {
            verify_worker_identity(worker, identity)?;
            (claim.owner == identity.lease_owner())
                .then_some(())
                .ok_or_else(|| "dispatch claim owner does not match Worker incarnation".to_string())
        }
        None => (claim.owner == worker.worker_id())
            .then_some(())
            .ok_or_else(|| "dispatch claim owner does not match Worker".to_string()),
    }
}

type HmacSha256 = Hmac<Sha256>;

/// Shared provisioning material for one logical Worker credential.
///
/// The same value configures the client request authorizer and is enrolled in
/// the server authenticator. Its `Debug` output is deliberately redacted.
#[derive(Clone)]
pub struct WorkerSigningCredential {
    worker_id: String,
    key_id: String,
    credential_id: String,
    secret: Arc<[u8]>,
}

impl std::fmt::Debug for WorkerSigningCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkerSigningCredential")
            .field("worker_id", &self.worker_id)
            .field("key_id", &self.key_id)
            .field("credential_id", &self.credential_id)
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

impl WorkerSigningCredential {
    pub fn new(
        worker_id: impl Into<String>,
        key_id: impl Into<String>,
        credential_id: impl Into<String>,
        secret: impl Into<Vec<u8>>,
    ) -> Result<Self, WorkerCredentialError> {
        let credential = Self {
            worker_id: worker_id.into(),
            key_id: key_id.into(),
            credential_id: credential_id.into(),
            secret: Arc::from(secret.into()),
        };
        if credential.worker_id.trim().is_empty()
            || credential.key_id.trim().is_empty()
            || credential.credential_id.trim().is_empty()
            || credential.secret.is_empty()
        {
            return Err(WorkerCredentialError::Invalid);
        }
        Ok(credential)
    }

    #[must_use]
    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }

    #[must_use]
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    #[must_use]
    pub fn credential_id(&self) -> &str {
        &self.credential_id
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum WorkerCredentialError {
    #[error("worker signing credential fields and secret must not be empty")]
    Invalid,
    #[error("worker request assertion could not be encoded")]
    Encode,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectedWorkerCredential {
    worker_id: String,
    key_id: String,
    credential_id: String,
    secret_base64: String,
}

/// Decode the canonical projected-file representation shared by Worker clients
/// and Coordinator enrollment. Keeping this parser at the transport-contract
/// boundary prevents the two process roles from accepting different wire shapes.
pub fn parse_projected_signing_credentials(
    source: &str,
) -> Result<Vec<WorkerSigningCredential>, String> {
    let projected = if source.trim_start().starts_with('[') {
        serde_json::from_str::<Vec<ProjectedWorkerCredential>>(source)
            .map_err(|error| format!("parse projected Worker credentials: {error}"))?
    } else {
        vec![
            serde_json::from_str::<ProjectedWorkerCredential>(source)
                .map_err(|error| format!("parse projected Worker credential: {error}"))?,
        ]
    };
    projected
        .into_iter()
        .map(|credential| {
            let secret = base64::engine::general_purpose::STANDARD
                .decode(credential.secret_base64.trim())
                .map_err(|_| "Worker credential secret_base64 is invalid".to_owned())?;
            WorkerSigningCredential::new(
                credential.worker_id,
                credential.key_id,
                credential.credential_id,
                secret,
            )
            .map_err(|error| error.to_string())
        })
        .collect()
}

/// Build the client authorizer from exactly one projected credential bound to
/// the configured Worker identity.
pub fn projected_request_authorizer(
    source: &str,
    worker_id: &str,
) -> Result<Arc<dyn WorkerRequestAuthorizer>, String> {
    let mut credentials = parse_projected_signing_credentials(source)?;
    if credentials.len() != 1 || credentials[0].worker_id() != worker_id {
        return Err(
            "Worker request credential must contain exactly the configured worker_id".into(),
        );
    }
    Ok(Arc::new(SignedWorkerRequestAuthorizer::new(
        credentials.remove(0),
    )))
}

/// File-system edge for the canonical projected request-credential parser.
pub fn load_projected_request_authorizer(
    path: &std::path::Path,
    worker_id: &str,
) -> Result<Arc<dyn WorkerRequestAuthorizer>, String> {
    let source = std::fs::read_to_string(path)
        .map_err(|error| format!("read Worker request credential {}: {error}", path.display()))?;
    projected_request_authorizer(&source, worker_id)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkerRequestAssertion {
    version: u32,
    worker_id: String,
    key_id: String,
    credential_id: String,
    identity: Option<WorkerIdentity>,
    method: String,
    path: String,
    request_id: String,
    issued_at_ms: u64,
    expires_at_ms: u64,
}

fn assertion_signature(secret: &[u8], payload: &str) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(payload.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

fn encode_assertion(
    assertion: &WorkerRequestAssertion,
    secret: &[u8],
) -> Result<String, WorkerCredentialError> {
    let payload = serde_json::to_vec(assertion).map_err(|_| WorkerCredentialError::Encode)?;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
    let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(assertion_signature(secret, &payload));
    Ok(format!("{payload}.{signature}"))
}

/// Client-side production authorizer. Every request receives a fresh,
/// route-bound, short-lived assertion; after registration the same authorizer is
/// rebound to the allocated incarnation and generation.
pub struct SignedWorkerRequestAuthorizer {
    credential: WorkerSigningCredential,
    identity: Option<WorkerIdentity>,
    clock: Arc<dyn WorkerClock>,
    assertion_ttl_ms: u64,
}

impl SignedWorkerRequestAuthorizer {
    #[must_use]
    pub fn new(credential: WorkerSigningCredential) -> Self {
        Self {
            credential,
            identity: None,
            clock: Arc::new(SystemWorkerClock),
            assertion_ttl_ms: 30_000,
        }
    }

    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn WorkerClock>) -> Self {
        self.clock = clock;
        self
    }

    #[must_use]
    pub fn with_assertion_ttl_ms(mut self, assertion_ttl_ms: u64) -> Self {
        self.assertion_ttl_ms = assertion_ttl_ms.max(1);
        self
    }

    fn request_id() -> Result<String, WorkerCredentialError> {
        static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);
        Ok(format!(
            "{}-{}",
            REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed),
            SystemWorkerClock.now_ms()
        ))
    }
}

impl WorkerRequestAuthorizer for SignedWorkerRequestAuthorizer {
    fn authorize(
        &self,
        method: &str,
        path: &str,
        worker_id: &str,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, String> {
        if worker_id != self.credential.worker_id {
            return Err("request worker id does not match signing credential".to_string());
        }
        let issued_at_ms = self.clock.now_ms();
        let assertion = WorkerRequestAssertion {
            version: 1,
            worker_id: worker_id.to_string(),
            key_id: self.credential.key_id.clone(),
            credential_id: self.credential.credential_id.clone(),
            identity: self.identity.clone(),
            method: method.to_ascii_uppercase(),
            path: path.to_string(),
            request_id: Self::request_id().map_err(|error| error.to_string())?,
            issued_at_ms,
            expires_at_ms: issued_at_ms.saturating_add(self.assertion_ttl_ms),
        };
        let token = encode_assertion(&assertion, &self.credential.secret)
            .map_err(|error| error.to_string())?;
        Ok(request.header(WORKER_ID_HEADER, worker_id).header(
            axum::http::header::AUTHORIZATION,
            format!("{SIGNED_WORKER_SCHEME} {token}"),
        ))
    }

    fn bind_worker_identity(&self, identity: &WorkerIdentity) -> Arc<dyn WorkerRequestAuthorizer> {
        Arc::new(Self {
            credential: self.credential.clone(),
            identity: Some(identity.clone()),
            clock: self.clock.clone(),
            assertion_ttl_ms: self.assertion_ttl_ms,
        })
    }
}

#[derive(Clone)]
struct EnrolledWorkerCredential {
    worker_id: String,
    secret: Arc<[u8]>,
}

/// Production authenticator for signed Worker request assertions.
///
/// Credentials can overlap during rotation. Removing a key or revoking one
/// credential takes effect immediately. A successfully verified request id is
/// remembered until assertion expiry so an exact captured request cannot replay.
pub struct SignedWorkerAuthenticator {
    credentials: RwLock<BTreeMap<(String, String), EnrolledWorkerCredential>>,
    revoked_credentials: RwLock<HashSet<String>>,
    seen_requests: Mutex<HashMap<(String, String), u64>>,
    clock: Arc<dyn WorkerClock>,
    max_assertion_ttl_ms: u64,
    clock_skew_ms: u64,
}

impl SignedWorkerAuthenticator {
    #[must_use]
    pub fn new(credential: WorkerSigningCredential) -> Self {
        let authenticator = Self {
            credentials: RwLock::new(BTreeMap::new()),
            revoked_credentials: RwLock::new(HashSet::new()),
            seen_requests: Mutex::new(HashMap::new()),
            clock: Arc::new(SystemWorkerClock),
            max_assertion_ttl_ms: 60_000,
            clock_skew_ms: 5_000,
        };
        authenticator.enroll(credential);
        authenticator
    }

    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn WorkerClock>) -> Self {
        self.clock = clock;
        self
    }

    #[must_use]
    pub fn with_time_policy(mut self, max_assertion_ttl_ms: u64, clock_skew_ms: u64) -> Self {
        self.max_assertion_ttl_ms = max_assertion_ttl_ms.max(1);
        self.clock_skew_ms = clock_skew_ms;
        self
    }

    /// Add a new key/credential while old credentials remain valid for overlap.
    pub fn enroll(&self, credential: WorkerSigningCredential) {
        self.credentials
            .write()
            .expect("worker credential registry lock")
            .insert(
                (credential.key_id.clone(), credential.credential_id.clone()),
                EnrolledWorkerCredential {
                    worker_id: credential.worker_id,
                    secret: credential.secret,
                },
            );
    }

    /// Revoke one provisioned credential without removing other credentials
    /// under the same rotation key.
    pub fn revoke_credential(&self, credential_id: impl Into<String>) {
        self.revoked_credentials
            .write()
            .expect("worker credential revocation lock")
            .insert(credential_id.into());
    }

    /// Retire every credential signed under one rotation key.
    pub fn remove_key(&self, key_id: &str) {
        self.credentials
            .write()
            .expect("worker credential registry lock")
            .retain(|(candidate, _), _| candidate != key_id);
    }

    fn verify(&self, parts: &Parts) -> Result<VerifiedWorkerContext, WorkerAuthError> {
        let header_worker = parts
            .headers
            .get(WORKER_ID_HEADER)
            .ok_or(WorkerAuthError::Missing)?
            .to_str()
            .map_err(|_| WorkerAuthError::Invalid("identity header is not ASCII".to_string()))?;
        let authorization = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .ok_or(WorkerAuthError::Missing)?
            .to_str()
            .map_err(|_| WorkerAuthError::Invalid("authorization is not ASCII".to_string()))?;
        let token = authorization
            .strip_prefix(SIGNED_WORKER_SCHEME)
            .and_then(|value| value.strip_prefix(' '))
            .ok_or_else(|| WorkerAuthError::Invalid("unsupported authorization scheme".into()))?;
        let (payload, signature) = token
            .split_once('.')
            .ok_or_else(|| WorkerAuthError::Invalid("malformed worker assertion".into()))?;
        let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| WorkerAuthError::Invalid("malformed worker assertion".into()))?;
        let assertion: WorkerRequestAssertion = serde_json::from_slice(&payload_bytes)
            .map_err(|_| WorkerAuthError::Invalid("malformed worker assertion".into()))?;
        let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| WorkerAuthError::Invalid("malformed worker assertion".into()))?;
        let credential = self
            .credentials
            .read()
            .expect("worker credential registry lock")
            .get(&(assertion.key_id.clone(), assertion.credential_id.clone()))
            .cloned()
            .ok_or_else(|| WorkerAuthError::Invalid("unknown worker credential".into()))?;
        if self
            .revoked_credentials
            .read()
            .expect("worker credential revocation lock")
            .contains(&assertion.credential_id)
        {
            return Err(WorkerAuthError::Invalid(
                "worker credential is revoked".into(),
            ));
        }
        let mut mac =
            HmacSha256::new_from_slice(&credential.secret).expect("HMAC accepts any key length");
        mac.update(payload.as_bytes());
        mac.verify_slice(&signature).map_err(|_| {
            WorkerAuthError::Invalid("worker assertion signature is invalid".into())
        })?;
        let now_ms = self.clock.now_ms();
        if assertion.version != 1
            || assertion.worker_id != credential.worker_id
            || assertion.worker_id != header_worker
            || assertion.method != parts.method.as_str().to_ascii_uppercase()
            || assertion.path != parts.uri.path()
            || assertion.issued_at_ms > now_ms.saturating_add(self.clock_skew_ms)
            || assertion.expires_at_ms.saturating_add(self.clock_skew_ms) < now_ms
            || assertion.expires_at_ms < assertion.issued_at_ms
            || assertion
                .expires_at_ms
                .saturating_sub(assertion.issued_at_ms)
                > self.max_assertion_ttl_ms
        {
            return Err(WorkerAuthError::Invalid(
                "worker assertion claims are invalid".into(),
            ));
        }
        if assertion.identity.as_ref().is_some_and(|identity| {
            identity.worker_id != assertion.worker_id
                || identity.incarnation_id.is_empty()
                || identity.generation == 0
        }) {
            return Err(WorkerAuthError::Invalid(
                "worker incarnation binding is invalid".into(),
            ));
        }
        let mut seen = self
            .seen_requests
            .lock()
            .expect("worker request replay lock");
        seen.retain(|_, expires_at_ms| expires_at_ms.saturating_add(self.clock_skew_ms) >= now_ms);
        let replay_key = (
            assertion.credential_id.clone(),
            assertion.request_id.clone(),
        );
        if seen.insert(replay_key, assertion.expires_at_ms).is_some() {
            return Err(WorkerAuthError::Invalid(
                "worker request assertion was replayed".into(),
            ));
        }
        Ok(VerifiedWorkerContext::signed(
            assertion.worker_id,
            assertion.identity,
            assertion.credential_id,
        ))
    }
}

#[async_trait]
impl WorkerRequestAuthenticator for SignedWorkerAuthenticator {
    async fn authenticate(&self, parts: &Parts) -> Result<VerifiedWorkerContext, WorkerAuthError> {
        self.verify(parts)
    }
}

/// Identity extracted from a mutually authenticated TLS peer certificate by the
/// server's TLS acceptor. Certificate parsing stays at the TLS boundary; no
/// forwarded HTTP header is trusted as an mTLS identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MtlsWorkerPrincipal {
    worker_id: String,
    identity: Option<WorkerIdentity>,
    certificate_sha256: String,
}

impl MtlsWorkerPrincipal {
    pub fn bootstrap(
        worker_id: impl Into<String>,
        certificate_sha256: impl Into<String>,
    ) -> Result<Self, WorkerCredentialError> {
        Self::new(worker_id.into(), None, certificate_sha256.into())
    }

    pub fn registered(
        identity: WorkerIdentity,
        certificate_sha256: impl Into<String>,
    ) -> Result<Self, WorkerCredentialError> {
        Self::new(
            identity.worker_id.clone(),
            Some(identity),
            certificate_sha256.into(),
        )
    }

    fn new(
        worker_id: String,
        identity: Option<WorkerIdentity>,
        certificate_sha256: String,
    ) -> Result<Self, WorkerCredentialError> {
        if worker_id.trim().is_empty()
            || certificate_sha256.trim().is_empty()
            || identity
                .as_ref()
                .is_some_and(|identity| identity.worker_id != worker_id)
        {
            return Err(WorkerCredentialError::Invalid);
        }
        Ok(Self {
            worker_id,
            identity,
            certificate_sha256,
        })
    }

    #[must_use]
    pub fn certificate_sha256(&self) -> &str {
        &self.certificate_sha256
    }
}

/// Authenticator for TLS stacks that publish a verified
/// [`MtlsWorkerPrincipal`] request extension.
#[derive(Debug, Clone, Copy, Default)]
pub struct MtlsWorkerAuthenticator;

#[async_trait]
impl WorkerRequestAuthenticator for MtlsWorkerAuthenticator {
    async fn authenticate(&self, parts: &Parts) -> Result<VerifiedWorkerContext, WorkerAuthError> {
        let principal = parts
            .extensions
            .get::<MtlsWorkerPrincipal>()
            .ok_or(WorkerAuthError::Missing)?;
        let header_worker = parts
            .headers
            .get(WORKER_ID_HEADER)
            .ok_or(WorkerAuthError::Missing)?
            .to_str()
            .map_err(|_| WorkerAuthError::Invalid("identity header is not ASCII".to_string()))?;
        if header_worker != principal.worker_id {
            return Err(WorkerAuthError::Invalid(
                "mTLS principal does not match worker header".into(),
            ));
        }
        Ok(VerifiedWorkerContext {
            worker_id: principal.worker_id.clone(),
            identity: principal.identity.clone(),
            credential_id: Some(format!("mtls:{}", principal.certificate_sha256)),
        })
    }
}

/// Closed classification of the remote Coordinator endpoint presented to the
/// production Worker transport constructor.
///
/// Six values are intentional: together with the credential and identity
/// classifications they make the complete 6 x 6 x 6 admission space finite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteEndpointPosture {
    Https,
    PlainHttp,
    EmbeddedAuthority,
    QueryOrFragment,
    OtherScheme,
    Invalid,
}

/// Closed classification of authentication material at the remote boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteCredentialPosture {
    ExactlyOneSigned,
    Missing,
    Multiple,
    UnsignedHeader,
    MtlsOnly,
    Invalid,
}

/// Closed classification of the configured and credential-bound Worker IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteIdentityPosture {
    Exact,
    ConfiguredEmpty,
    CredentialEmpty,
    ConfiguredWhitespace,
    CredentialWhitespace,
    Mismatch,
}

/// The only authority-bearing transport posture admitted for a remote Worker.
/// Its fields are deliberately not configurable: admission always preserves
/// all three security requirements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteWorkerTransportSelection {
    require_https: bool,
    require_signed_credential: bool,
    bind_exact_worker_id: bool,
}

impl RemoteWorkerTransportSelection {
    #[must_use]
    pub const fn requires_https(self) -> bool {
        self.require_https
    }

    #[must_use]
    pub const fn requires_signed_credential(self) -> bool {
        self.require_signed_credential
    }

    #[must_use]
    pub const fn binds_exact_worker_id(self) -> bool {
        self.bind_exact_worker_id
    }
}

/// Select the production remote transport from the complete finite posture
/// algebra. Exactly one of the 216 combinations is admitted.
#[must_use]
pub const fn select_remote_worker_transport(
    endpoint: RemoteEndpointPosture,
    credential: RemoteCredentialPosture,
    identity: RemoteIdentityPosture,
) -> Option<RemoteWorkerTransportSelection> {
    match (endpoint, credential, identity) {
        (
            RemoteEndpointPosture::Https,
            RemoteCredentialPosture::ExactlyOneSigned,
            RemoteIdentityPosture::Exact,
        ) => Some(RemoteWorkerTransportSelection {
            require_https: true,
            require_signed_credential: true,
            bind_exact_worker_id: true,
        }),
        _ => None,
    }
}

fn classify_remote_endpoint(base_url: &str) -> RemoteEndpointPosture {
    let Ok(url) = reqwest::Url::parse(base_url) else {
        return RemoteEndpointPosture::Invalid;
    };
    if !url.username().is_empty() || url.password().is_some() {
        return RemoteEndpointPosture::EmbeddedAuthority;
    }
    if url.query().is_some() || url.fragment().is_some() {
        return RemoteEndpointPosture::QueryOrFragment;
    }
    match url.scheme() {
        "https" if url.host_str().is_some() => RemoteEndpointPosture::Https,
        "http" => RemoteEndpointPosture::PlainHttp,
        _ => RemoteEndpointPosture::OtherScheme,
    }
}

fn classify_remote_credentials(count: usize) -> RemoteCredentialPosture {
    match count {
        0 => RemoteCredentialPosture::Missing,
        1 => RemoteCredentialPosture::ExactlyOneSigned,
        _ => RemoteCredentialPosture::Multiple,
    }
}

fn classify_remote_identity(configured: &str, credential: &str) -> RemoteIdentityPosture {
    if configured.is_empty() {
        RemoteIdentityPosture::ConfiguredEmpty
    } else if credential.is_empty() {
        RemoteIdentityPosture::CredentialEmpty
    } else if configured.trim().is_empty() {
        RemoteIdentityPosture::ConfiguredWhitespace
    } else if credential.trim().is_empty() {
        RemoteIdentityPosture::CredentialWhitespace
    } else if configured == credential {
        RemoteIdentityPosture::Exact
    } else {
        RemoteIdentityPosture::Mismatch
    }
}

/// Client-side worker transport configuration. A managed composition injects a
/// TLS-configured client and the identity bound to its WorkerLease; the same
/// values are then used for dispatch, claimed commit, and ordinary commit calls.
#[derive(Clone)]
pub struct WorkerUpstream {
    base_url: String,
    client: reqwest::Client,
    worker_id: String,
    worker_identity: Option<WorkerIdentity>,
    request_authorizer: Arc<dyn WorkerRequestAuthorizer>,
}

impl WorkerUpstream {
    /// Connect to the private Worker-to-Coordinator plane without inheriting
    /// workstation or container egress proxies. Deployments that intentionally
    /// proxy this channel can still supply an explicit client with
    /// [`Self::with_client`].
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .no_proxy()
                .connect_timeout(std::time::Duration::from_secs(5))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("the default Worker upstream HTTP client should build"),
            request_authorizer: Arc::new(HeaderWorkerRequestAuthorizer),
            worker_id: "awaken-worker".to_string(),
            worker_identity: None,
        }
    }

    /// Construct the production remote Worker transport. Unlike [`Self::new`],
    /// which remains the explicit local/test compatibility constructor, this
    /// path fails closed unless the endpoint is HTTPS, exactly one signing
    /// credential is supplied, and its Worker ID exactly matches the configured
    /// identity.
    pub fn remote(
        base_url: impl Into<String>,
        worker_id: impl Into<String>,
        mut credentials: Vec<WorkerSigningCredential>,
    ) -> Result<Self, String> {
        let base_url = base_url.into();
        let worker_id = worker_id.into();
        let credential_posture = classify_remote_credentials(credentials.len());
        let credential_worker_id = credentials
            .first()
            .map_or("", WorkerSigningCredential::worker_id);
        let identity_posture = classify_remote_identity(&worker_id, credential_worker_id);
        select_remote_worker_transport(
            classify_remote_endpoint(&base_url),
            credential_posture,
            identity_posture,
        )
        .ok_or_else(|| {
            "remote Worker transport requires HTTPS, exactly one signing credential, and an exact configured worker_id"
                .to_owned()
        })?;

        let credential = credentials
            .pop()
            .expect("admitted remote transport has exactly one credential");
        Ok(Self::new(base_url)
            .with_worker_id(worker_id)
            .with_request_authorizer(Arc::new(SignedWorkerRequestAuthorizer::new(credential))))
    }

    /// Construct the production remote transport and trust one operator-
    /// projected private CA in addition to the public WebPKI roots. This keeps
    /// TLS trust material at the Worker boundary and avoids global process TLS
    /// overrides for private clusters and service meshes.
    pub fn remote_with_ca_certificate(
        base_url: impl Into<String>,
        worker_id: impl Into<String>,
        credentials: Vec<WorkerSigningCredential>,
        ca_certificate_pem: &[u8],
    ) -> Result<Self, String> {
        let upstream = Self::remote(base_url, worker_id, credentials)?;
        let certificate = reqwest::Certificate::from_pem(ca_certificate_pem)
            .map_err(|error| format!("parse Worker server CA certificate: {error}"))?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(30))
            .add_root_certificate(certificate)
            .build()
            .map_err(|error| format!("build Worker upstream TLS client: {error}"))?;
        Ok(upstream.with_client(client))
    }

    #[must_use]
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    #[must_use]
    pub fn with_worker_id(mut self, worker_id: impl Into<String>) -> Self {
        self.worker_id = worker_id.into();
        self
    }

    #[must_use]
    pub fn with_worker_identity(mut self, identity: WorkerIdentity) -> Self {
        self.worker_id = identity.worker_id.clone();
        self.request_authorizer = self.request_authorizer.bind_worker_identity(&identity);
        self.worker_identity = Some(identity);
        self
    }

    #[must_use]
    pub fn with_request_authorizer(mut self, authorizer: Arc<dyn WorkerRequestAuthorizer>) -> Self {
        self.request_authorizer = match &self.worker_identity {
            Some(identity) => authorizer.bind_worker_identity(identity),
            None => authorizer,
        };
        self
    }

    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Clone the HTTP client carrying this upstream's transport identity (for
    /// example mTLS configuration) for an application request.
    #[must_use]
    pub fn http_client(&self) -> reqwest::Client {
        self.client.clone()
    }

    /// Apply the same registered Worker request authorization used by dispatch
    /// and claimed commit to an application-owned endpoint.
    pub fn authorize_request(
        &self,
        method: &str,
        path: &str,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, String> {
        self.authorize(method, path, request)
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }

    pub fn worker_identity(&self) -> Option<&WorkerIdentity> {
        self.worker_identity.as_ref()
    }

    pub fn authorize(
        &self,
        method: &str,
        path: &str,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, String> {
        self.request_authorizer
            .authorize(method, path, &self.worker_id, request)
    }

    pub fn request_authorizer(&self) -> Arc<dyn WorkerRequestAuthorizer> {
        self.request_authorizer.clone()
    }
}

#[cfg(kani)]
fn remote_endpoint_from_index(index: u8) -> RemoteEndpointPosture {
    match index {
        0 => RemoteEndpointPosture::Https,
        1 => RemoteEndpointPosture::PlainHttp,
        2 => RemoteEndpointPosture::EmbeddedAuthority,
        3 => RemoteEndpointPosture::QueryOrFragment,
        4 => RemoteEndpointPosture::OtherScheme,
        _ => RemoteEndpointPosture::Invalid,
    }
}

#[cfg(kani)]
fn remote_credential_from_index(index: u8) -> RemoteCredentialPosture {
    match index {
        0 => RemoteCredentialPosture::ExactlyOneSigned,
        1 => RemoteCredentialPosture::Missing,
        2 => RemoteCredentialPosture::Multiple,
        3 => RemoteCredentialPosture::UnsignedHeader,
        4 => RemoteCredentialPosture::MtlsOnly,
        _ => RemoteCredentialPosture::Invalid,
    }
}

#[cfg(kani)]
fn remote_identity_from_index(index: u8) -> RemoteIdentityPosture {
    match index {
        0 => RemoteIdentityPosture::Exact,
        1 => RemoteIdentityPosture::ConfiguredEmpty,
        2 => RemoteIdentityPosture::CredentialEmpty,
        3 => RemoteIdentityPosture::ConfiguredWhitespace,
        4 => RemoteIdentityPosture::CredentialWhitespace,
        _ => RemoteIdentityPosture::Mismatch,
    }
}

/// Exhaustively covers the finite 6 x 6 x 6 posture algebra and proves that
/// only HTTPS + one signed credential + exact identity is admitted.
#[cfg(kani)]
#[kani::proof]
fn worker_transport_selector_admits_only_three_exact_postures() {
    let endpoint_index: u8 = kani::any();
    let credential_index: u8 = kani::any();
    let identity_index: u8 = kani::any();
    kani::assume(endpoint_index < 6);
    kani::assume(credential_index < 6);
    kani::assume(identity_index < 6);

    let endpoint = remote_endpoint_from_index(endpoint_index);
    let credential = remote_credential_from_index(credential_index);
    let identity = remote_identity_from_index(identity_index);
    let admitted = select_remote_worker_transport(endpoint, credential, identity);
    let exact = endpoint == RemoteEndpointPosture::Https
        && credential == RemoteCredentialPosture::ExactlyOneSigned
        && identity == RemoteIdentityPosture::Exact;
    assert_eq!(admitted.is_some(), exact);
}

/// Any admitted remote selection retains every required security property; no
/// state can downgrade TLS/signing or broaden the Worker identity binding.
#[cfg(kani)]
#[kani::proof]
fn remote_transport_never_downgrades_or_widens_identity() {
    let endpoint_index: u8 = kani::any();
    let credential_index: u8 = kani::any();
    let identity_index: u8 = kani::any();
    kani::assume(endpoint_index < 6);
    kani::assume(credential_index < 6);
    kani::assume(identity_index < 6);

    if let Some(selection) = select_remote_worker_transport(
        remote_endpoint_from_index(endpoint_index),
        remote_credential_from_index(credential_index),
        remote_identity_from_index(identity_index),
    ) {
        assert!(selection.requires_https());
        assert!(selection.requires_signed_credential());
        assert!(selection.binds_exact_worker_id());
        assert_eq!(endpoint_index, 0);
        assert_eq!(credential_index, 0);
        assert_eq!(identity_index, 0);
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct HeaderWorkerRequestAuthorizer;

impl WorkerRequestAuthorizer for HeaderWorkerRequestAuthorizer {
    fn authorize(
        &self,
        _method: &str,
        _path: &str,
        worker_id: &str,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, String> {
        Ok(request.header(WORKER_ID_HEADER, worker_id))
    }

    fn bind_worker_identity(&self, _identity: &WorkerIdentity) -> Arc<dyn WorkerRequestAuthorizer> {
        Arc::new(*self)
    }
}

/// Process-local proof that a worker request passed the configured authenticator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedWorkerContext {
    worker_id: String,
    identity: Option<WorkerIdentity>,
    credential_id: Option<String>,
}

impl VerifiedWorkerContext {
    /// Construct a verified context from an authenticator implementation.
    ///
    /// The field remains private so application code cannot alter identity after
    /// authentication. Managed implementations call this only after validating
    /// their signed WorkerLease or mTLS peer.
    #[must_use]
    pub fn authenticated(worker_id: impl Into<String>) -> Self {
        Self {
            worker_id: worker_id.into(),
            identity: None,
            credential_id: None,
        }
    }

    fn signed(worker_id: String, identity: Option<WorkerIdentity>, credential_id: String) -> Self {
        Self {
            worker_id,
            identity,
            credential_id: Some(credential_id),
        }
    }

    #[must_use]
    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }

    #[must_use]
    pub fn identity(&self) -> Option<&WorkerIdentity> {
        self.identity.as_ref()
    }

    #[must_use]
    pub fn credential_id(&self) -> Option<&str> {
        self.credential_id.as_deref()
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum WorkerAuthError {
    #[error("worker authentication is missing")]
    Missing,
    #[error("worker authentication is invalid: {0}")]
    Invalid(String),
}

#[async_trait]
pub trait WorkerRequestAuthenticator: Send + Sync {
    async fn authenticate(&self, parts: &Parts) -> Result<VerifiedWorkerContext, WorkerAuthError>;
}

/// Local/test authenticator. It establishes identity only from the compatibility
/// header and must be mounted behind a trusted boundary in production.
#[derive(Debug, Clone, Copy, Default)]
pub struct HeaderWorkerAuthenticator;

#[async_trait]
impl WorkerRequestAuthenticator for HeaderWorkerAuthenticator {
    async fn authenticate(&self, parts: &Parts) -> Result<VerifiedWorkerContext, WorkerAuthError> {
        let worker = parts
            .headers
            .get(WORKER_ID_HEADER)
            .ok_or(WorkerAuthError::Missing)?
            .to_str()
            .map_err(|_| WorkerAuthError::Invalid("identity header is not ASCII".to_string()))?
            .trim();
        if worker.is_empty() {
            return Err(WorkerAuthError::Invalid(
                "identity header is empty".to_string(),
            ));
        }
        Ok(VerifiedWorkerContext::authenticated(worker))
    }
}

pub trait WorkerClock: Send + Sync {
    fn now_ms(&self) -> u64;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemWorkerClock;

impl WorkerClock for SystemWorkerClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }
}

/// Deterministic server clock for conformance and fault-window tests.
#[derive(Debug)]
pub struct ManualWorkerClock(AtomicU64);

impl ManualWorkerClock {
    #[must_use]
    pub fn new(now_ms: u64) -> Self {
        Self(AtomicU64::new(now_ms))
    }

    pub fn set(&self, now_ms: u64) {
        self.0.store(now_ms, Ordering::SeqCst);
    }
}

impl WorkerClock for ManualWorkerClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

pub trait WorkerLeasePolicy: Send + Sync {
    fn lease_ms(&self, worker: &VerifiedWorkerContext) -> u64;
}

#[derive(Debug, Clone, Copy)]
pub struct FixedWorkerLeasePolicy {
    lease_ms: u64,
}

impl FixedWorkerLeasePolicy {
    #[must_use]
    pub fn new(lease_ms: u64) -> Self {
        Self {
            lease_ms: lease_ms.max(1),
        }
    }
}

impl Default for FixedWorkerLeasePolicy {
    fn default() -> Self {
        Self::new(30_000)
    }
}

impl WorkerLeasePolicy for FixedWorkerLeasePolicy {
    fn lease_ms(&self, _worker: &VerifiedWorkerContext) -> u64 {
        self.lease_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cause/effect decision table: C1 document shape is one object or an
    /// enrollment array, C2 base64 is valid, C3 required identity/key/secret
    /// fields are non-empty, C4 no unknown field is present, C5 request identity
    /// matches the sole credential. R1 all true -> exact credentials/authorizer;
    /// R2 !C2/!C3/!C4/!C5 -> reject. Both process roles exercise this one parser
    /// rather than maintaining separate projected-file DTOs.
    #[test]
    fn projected_credential_parser_decision_table() {
        let one = r#"{"worker_id":"worker-a","key_id":"key-a","credential_id":"credential-a","secret_base64":"c2VjcmV0"}"#;
        assert_eq!(
            parse_projected_signing_credentials(one).unwrap().len(),
            1,
            "R1 object"
        );
        assert_eq!(
            parse_projected_signing_credentials(&format!("[{one},{one}]"))
                .unwrap()
                .len(),
            2,
            "R1 array"
        );
        assert!(projected_request_authorizer(one, "worker-a").is_ok(), "R1");
        assert!(
            projected_request_authorizer(one, "worker-b").is_err(),
            "R2 identity mismatch"
        );
        for invalid in [
            r#"{"worker_id":"worker-a","key_id":"key-a","credential_id":"credential-a","secret_base64":"%%%"}"#,
            r#"{"worker_id":"","key_id":"key-a","credential_id":"credential-a","secret_base64":"c2VjcmV0"}"#,
            r#"{"worker_id":"worker-a","key_id":"key-a","credential_id":"credential-a","secret_base64":"c2VjcmV0","extra":true}"#,
        ] {
            assert!(parse_projected_signing_credentials(invalid).is_err(), "R2");
        }
    }

    fn credential(id: &str) -> WorkerSigningCredential {
        WorkerSigningCredential::new(
            "worker-signed",
            format!("key-{id}"),
            format!("credential-{id}"),
            format!("secret-{id}").into_bytes(),
        )
        .unwrap()
    }

    #[test]
    fn production_remote_transport_requires_the_exact_secure_posture() {
        assert!(
            WorkerUpstream::remote(
                "https://coordinator.invalid",
                "worker-signed",
                vec![credential("remote")],
            )
            .is_ok()
        );
        for rejected in [
            WorkerUpstream::remote(
                "http://coordinator.invalid",
                "worker-signed",
                vec![credential("remote")],
            ),
            WorkerUpstream::remote(
                "https://coordinator.invalid",
                "worker-other",
                vec![credential("remote")],
            ),
            WorkerUpstream::remote("https://coordinator.invalid", "worker-signed", Vec::new()),
            WorkerUpstream::remote(
                "https://coordinator.invalid",
                "worker-signed",
                vec![credential("one"), credential("two")],
            ),
        ] {
            assert!(rejected.is_err());
        }
    }

    fn signed_parts(
        authorizer: &dyn WorkerRequestAuthorizer,
        method: &str,
        signed_path: &str,
        request_path: &str,
    ) -> Parts {
        let request = authorizer
            .authorize(
                method,
                signed_path,
                "worker-signed",
                reqwest::Client::new().post("http://control.invalid"),
            )
            .unwrap()
            .build()
            .unwrap();
        let mut builder = axum::http::Request::builder()
            .method(method)
            .uri(request_path);
        for (name, value) in request.headers() {
            builder = builder.header(name, value);
        }
        builder.body(()).unwrap().into_parts().0
    }

    #[tokio::test]
    async fn signed_assertion_binds_route_incarnation_and_rejects_replay() {
        let clock = Arc::new(ManualWorkerClock::new(10_000));
        let credential = credential("primary");
        let authorizer = SignedWorkerRequestAuthorizer::new(credential.clone())
            .with_clock(clock.clone())
            .with_assertion_ttl_ms(1_000);
        let identity = WorkerIdentity::new("worker-signed", "boot-a", 7);
        let authorizer = authorizer.bind_worker_identity(&identity);
        let authenticator = SignedWorkerAuthenticator::new(credential)
            .with_clock(clock)
            .with_time_policy(2_000, 0);
        let parts = signed_parts(
            authorizer.as_ref(),
            "POST",
            "/v1/worker/heartbeat",
            "/v1/worker/heartbeat",
        );

        let verified = authenticator.authenticate(&parts).await.unwrap();
        assert_eq!(verified.worker_id(), "worker-signed");
        assert_eq!(verified.identity(), Some(&identity));
        assert_eq!(verified.credential_id(), Some("credential-primary"));
        assert!(matches!(
            authenticator.authenticate(&parts).await,
            Err(WorkerAuthError::Invalid(message)) if message.contains("replayed")
        ));
    }

    #[tokio::test]
    async fn signed_assertion_rejects_route_tampering_expiry_and_revocation() {
        let clock = Arc::new(ManualWorkerClock::new(20_000));
        let credential = credential("rotation-a");
        let authorizer = SignedWorkerRequestAuthorizer::new(credential.clone())
            .with_clock(clock.clone())
            .with_assertion_ttl_ms(500);
        let authenticator = SignedWorkerAuthenticator::new(credential.clone())
            .with_clock(clock.clone())
            .with_time_policy(1_000, 0);

        let tampered = signed_parts(
            &authorizer,
            "POST",
            "/v1/worker/heartbeat",
            "/v1/worker/drain",
        );
        assert!(authenticator.authenticate(&tampered).await.is_err());

        let expired = signed_parts(
            &authorizer,
            "POST",
            "/v1/worker/heartbeat",
            "/v1/worker/heartbeat",
        );
        clock.set(20_501);
        assert!(authenticator.authenticate(&expired).await.is_err());

        clock.set(21_000);
        let revoked = signed_parts(
            &authorizer,
            "POST",
            "/v1/worker/heartbeat",
            "/v1/worker/heartbeat",
        );
        authenticator.revoke_credential(credential.credential_id());
        assert!(authenticator.authenticate(&revoked).await.is_err());
    }

    #[tokio::test]
    async fn credential_rotation_overlaps_then_old_key_is_retired() {
        let clock = Arc::new(ManualWorkerClock::new(30_000));
        let old = credential("old");
        let new = credential("new");
        let old_authorizer =
            SignedWorkerRequestAuthorizer::new(old.clone()).with_clock(clock.clone());
        let new_authorizer =
            SignedWorkerRequestAuthorizer::new(new.clone()).with_clock(clock.clone());
        let authenticator = SignedWorkerAuthenticator::new(old.clone()).with_clock(clock.clone());
        authenticator.enroll(new);

        let old_request = signed_parts(
            &old_authorizer,
            "POST",
            "/v1/worker/register",
            "/v1/worker/register",
        );
        let new_request = signed_parts(
            &new_authorizer,
            "POST",
            "/v1/worker/register",
            "/v1/worker/register",
        );
        assert!(authenticator.authenticate(&old_request).await.is_ok());
        assert!(authenticator.authenticate(&new_request).await.is_ok());

        authenticator.remove_key(old.key_id());
        let retired = signed_parts(
            &old_authorizer,
            "POST",
            "/v1/worker/register",
            "/v1/worker/register",
        );
        assert!(authenticator.authenticate(&retired).await.is_err());
    }

    #[test]
    fn signing_credential_debug_redacts_secret() {
        let debug = format!("{:?}", credential("debug"));
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("secret-debug"));
    }

    #[tokio::test]
    async fn mtls_authenticator_uses_verified_extension_and_binds_incarnation() {
        let identity = WorkerIdentity::new("worker-signed", "boot-mtls", 9);
        let principal =
            MtlsWorkerPrincipal::registered(identity.clone(), "sha256:certificate").unwrap();
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/worker/heartbeat")
            .header(WORKER_ID_HEADER, "worker-signed")
            .extension(principal)
            .body(())
            .unwrap();
        let parts = request.into_parts().0;

        let verified = MtlsWorkerAuthenticator.authenticate(&parts).await.unwrap();
        assert_eq!(verified.identity(), Some(&identity));
        assert_eq!(verified.credential_id(), Some("mtls:sha256:certificate"));
    }
}
