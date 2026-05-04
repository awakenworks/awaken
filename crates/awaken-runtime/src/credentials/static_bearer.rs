//! Pass-through signer for static bearer credentials.
//!
//! There is no network call; the broker hands the configured string back
//! verbatim. The "expiry" is set to far-future so the cache layer never
//! tries to refresh it.

use std::time::{Duration, SystemTime};

use awaken_contract::secret::RedactedString;

use super::token::Token;

/// 30 days — chosen to be much longer than any realistic admin re-config
/// cadence yet still finite (so an absurd value doesn't risk overflow).
/// The actual API key may have a different upstream expiry; rotation is
/// the operator's responsibility for static bearers.
const STATIC_BEARER_TTL: Duration = Duration::from_secs(30 * 24 * 3600);

/// Mint a `Token` from a static bearer string.
///
/// This is a pure function — no I/O, no async — so the broker can call it
/// inline without entering the single-flight slot.
pub(super) fn mint_static_bearer(bearer: &RedactedString) -> Token {
    Token {
        bearer: bearer.clone(),
        expires_at: SystemTime::now() + STATIC_BEARER_TTL,
    }
}
