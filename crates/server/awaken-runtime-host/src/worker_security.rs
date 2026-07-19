//! Neutral security and timing ports for the worker-facing HTTP boundary.
//!
//! The open runtime defines what must be trusted; a deployment decides how that
//! trust is established. The default header authenticator is suitable for local
//! and test compositions behind a trusted network. Managed deployments replace it
//! with WorkerLease/mTLS verification without changing dispatch semantics.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::http::request::Parts;

/// Compatibility identity header used by the local HTTP client and authenticator.
pub const WORKER_ID_HEADER: &str = "x-awaken-worker-id";

/// Client-side worker transport configuration. A managed composition injects a
/// TLS-configured client and the identity bound to its WorkerLease; the same
/// values are then used for dispatch, claimed commit, and ordinary commit calls.
#[derive(Clone)]
pub struct WorkerUpstream {
    base_url: String,
    client: reqwest::Client,
    worker_id: String,
}

impl WorkerUpstream {
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
            worker_id: std::env::var("AWAKEN_WORKER_ID")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "awaken-worker".to_string()),
        }
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
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub(crate) fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub(crate) fn worker_id(&self) -> &str {
        &self.worker_id
    }
}

/// Process-local proof that a worker request passed the configured authenticator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedWorkerContext {
    worker_id: String,
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
        }
    }

    #[must_use]
    pub fn worker_id(&self) -> &str {
        &self.worker_id
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
