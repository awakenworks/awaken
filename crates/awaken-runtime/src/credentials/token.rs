//! Token + lease types returned by the credential broker.
//!
//! `Token` is the broker-internal form (carries the refresh deadline);
//! `TokenLease` is what callers see — they should not cache it themselves
//! (the broker already does that), so we hand out bearer-only references
//! that auto-drop quickly.

use std::time::{Duration, SystemTime};

use awaken_contract::secret::RedactedString;

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

/// Public, short-lived view of a minted token.
///
/// Callers receive this from [`CredentialBroker::token_for`](super::CredentialBroker::token_for),
/// extract the bearer string with [`TokenLease::bearer`], and are expected to
/// use it immediately rather than cache. The broker handles caching.
#[derive(Debug, Clone)]
pub struct TokenLease {
    bearer: RedactedString,
    expires_at: SystemTime,
}

impl TokenLease {
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
    /// on this; ask the broker for a new lease instead.
    pub fn expires_at(&self) -> SystemTime {
        self.expires_at
    }
}
