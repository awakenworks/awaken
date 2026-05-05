//! Per-kind token-minting strategy.
//!
//! Each [`Minter`] implementation owns the secrets and protocol logic for
//! one credential kind (static bearer, Google SA-JWT, future AWS SigV4, …).
//! The broker dispatches via the trait rather than a central `match`, so
//! adding a new cloud means writing a new [`Minter`] impl and a
//! [`CredentialMaterial`](super::material::CredentialMaterial)
//! constructor — not editing the broker.
//!
//! Disabled-feature stubbing: when the `credentials-google` cargo feature
//! is off, [`super::material::CredentialMaterial::google_service_account`]
//! is removed and `build_material` rejects `service_account_json`
//! configurations with a clear "feature disabled" error. There is no
//! runtime stub mod — the cfg gate lives at construction time.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use awaken_contract::secret::RedactedString;

use super::error::CredentialError;
use super::token::Token;

/// 30 days — chosen to be much longer than any realistic admin re-config
/// cadence yet still finite (so an absurd value doesn't risk overflow).
/// The actual API key may have a different upstream expiry; rotation is
/// the operator's responsibility for static bearers.
const STATIC_BEARER_TTL: Duration = Duration::from_secs(30 * 24 * 3600);

/// Strategy interface for producing fresh tokens.
///
/// Internal to the credentials module — implementations are held inside
/// [`CredentialMaterial`](super::material::CredentialMaterial)
/// as `Arc<dyn Minter>`; the broker dispatches via the trait. To swap
/// auth wholesale, embedders implement [`super::CredentialBroker`]
/// instead.
#[async_trait]
pub(crate) trait Minter: Send + Sync + std::fmt::Debug {
    /// Stable telemetry / logging label (e.g. `"bearer"`,
    /// `"service_account_json"`).
    fn kind_label(&self) -> &'static str;

    /// Mint a fresh token for `scope`. `http` is the broker's shared
    /// reqwest client; minters that don't make HTTP calls (e.g. static
    /// bearer) ignore it.
    async fn mint(&self, scope: &str, http: &reqwest::Client) -> Result<Token, CredentialError>;
}

/// Pass-through minter for static bearer credentials.
///
/// No I/O, no async work — the configured string is returned verbatim
/// with a far-future expiry so the broker's cache layer never tries to
/// refresh it. Production code bypasses the broker entirely for this
/// kind (see `build_genai_provider_executor` in awaken-server); this
/// minter exists for embedders that register static bearers with the
/// broker directly and for unit tests.
#[derive(Debug)]
pub(crate) struct StaticBearerMinter {
    bearer: RedactedString,
}

impl StaticBearerMinter {
    pub(crate) fn new(bearer: RedactedString) -> Self {
        Self { bearer }
    }
}

#[async_trait]
impl Minter for StaticBearerMinter {
    fn kind_label(&self) -> &'static str {
        "bearer"
    }

    async fn mint(&self, _scope: &str, _http: &reqwest::Client) -> Result<Token, CredentialError> {
        Ok(Token {
            bearer: self.bearer.clone(),
            expires_at: SystemTime::now() + STATIC_BEARER_TTL,
        })
    }
}

/// Google Cloud OAuth minter. Signs an RS256 JWT from the parsed service
/// account key and exchanges it for an access token at `token_uri`.
#[cfg(any(test, feature = "credentials-google"))]
#[derive(Debug)]
pub(crate) struct GoogleServiceAccountMinter {
    provider_id: String,
    key: Arc<super::material::GoogleServiceAccountKey>,
}

#[cfg(any(test, feature = "credentials-google"))]
impl GoogleServiceAccountMinter {
    pub(crate) fn new(
        provider_id: String,
        key: Arc<super::material::GoogleServiceAccountKey>,
    ) -> Self {
        Self { provider_id, key }
    }
}

#[cfg(any(test, feature = "credentials-google"))]
#[async_trait]
impl Minter for GoogleServiceAccountMinter {
    fn kind_label(&self) -> &'static str {
        "service_account_json"
    }

    async fn mint(&self, scope: &str, http: &reqwest::Client) -> Result<Token, CredentialError> {
        super::google_oauth::mint(&self.provider_id, &self.key, scope, http).await
    }
}

// Re-export an `Arc<dyn Minter>` constructor surface for the parent module.
pub(super) fn static_bearer_arc(bearer: RedactedString) -> Arc<dyn Minter> {
    Arc::new(StaticBearerMinter::new(bearer))
}

#[cfg(any(test, feature = "credentials-google"))]
pub(super) fn google_service_account_arc(
    provider_id: String,
    key: Arc<super::material::GoogleServiceAccountKey>,
) -> Arc<dyn Minter> {
    Arc::new(GoogleServiceAccountMinter::new(provider_id, key))
}
