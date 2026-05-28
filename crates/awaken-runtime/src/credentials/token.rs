//! Token type returned by the credential broker.
//!
//! `Token` is the broker-internal cache entry. `IssuedToken` is the
//! short-lived public view callers receive — a plain value object holding
//! the bearer string and the wall-clock deadline. It carries no lease
//! semantics (no Drop, no revoke); callers are expected to use the bearer
//! immediately and ask the broker for a new one next request rather than
//! caching client-side. The broker already caches.

use std::time::{Duration, SystemTime};

use awaken_runtime_contract::secret::RedactedString;

/// Cached token entry held by the broker. Not exposed to callers directly.
#[derive(Debug, Clone)]
pub(crate) struct Token {
    pub(crate) bearer: RedactedString,
    pub(crate) expires_at: SystemTime,
}

impl Token {
    /// True when the cached token is past — or within the safety window of —
    /// its expiry. The window prevents handing out a token that will expire
    /// mid-request.
    pub(crate) fn is_near_expiry(&self, safety_window: Duration) -> bool {
        match self.expires_at.duration_since(SystemTime::now()) {
            Ok(remaining) => remaining <= safety_window,
            // Already expired — any duration_since with a past timestamp
            // returns Err.
            Err(_) => true,
        }
    }
}

/// Public, value-object view of a minted token.
///
/// Returned by [`CredentialBroker::token_for`](super::CredentialBroker::token_for).
/// Use the bearer immediately; the broker caches token state, so client-side
/// caching of this value is unnecessary and risks holding a token past its
/// upstream-side expiry.
#[derive(Debug, Clone)]
pub struct IssuedToken {
    bearer: RedactedString,
    expires_at: SystemTime,
}

impl IssuedToken {
    pub(crate) fn from_token(token: &Token) -> Self {
        Self {
            bearer: token.bearer.clone(),
            expires_at: token.expires_at,
        }
    }

    /// The bearer token value. Use immediately; do not store.
    pub fn bearer(&self) -> &str {
        self.bearer.expose_secret()
    }

    /// Wall-clock deadline after which this token is no longer valid.
    /// Exposed for telemetry / debug — callers must not gate retry logic
    /// on this; ask the broker for a new one instead.
    pub fn expires_at(&self) -> SystemTime {
        self.expires_at
    }
}
