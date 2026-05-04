//! Credential management for LLM providers.
//!
//! All provider auth — static bearer tokens, OAuth via service-account JWT,
//! and (future) AWS SigV4 / Azure client secret — flows through the
//! [`CredentialBroker`] trait. The broker:
//! - parses credential material from `ProviderSpec` at config write time,
//! - caches minted tokens until near expiry,
//! - serialises concurrent refreshes ("single-flight") so a token rotation
//!   does not stampede the upstream OAuth endpoint.
//!
//! ## Why a broker?
//! Earlier revisions of awaken passed credentials directly to genai via
//! `with_auth_resolver_fn`, which works fine for pre-signed bearers but
//! fans out into ad-hoc per-provider refresh code as soon as you add
//! anything dynamic (Vertex AI service accounts, AWS SigV4, …). The
//! broker is the dedicated owner: one place to look at all auth, one
//! trait to mock in tests, one observability hook to instrument.
//!
//! ## Configuration discriminator
//! `ProviderSpec.adapter_options.credentials_kind` selects how the broker
//! interprets `ProviderSpec.api_key`:
//!
//! | `credentials_kind`         | `api_key` payload                  | Refresh         |
//! |----------------------------|------------------------------------|-----------------|
//! | absent / `"bearer"`        | OAuth bearer or static API key     | operator-managed|
//! | `"service_account_json"`   | full Google service-account JSON   | broker, automatic|
//!
//! Compatibility rules and validation live in [`material::build_material`].

pub mod broker;
pub mod error;
pub mod material;
mod static_bearer;

#[cfg(any(test, feature = "credentials-google"))]
pub mod google_oauth;

#[cfg(not(any(test, feature = "credentials-google")))]
mod google_oauth {
    //! Stubbed signer when the `credentials-google` feature is disabled.
    //!
    //! Configuring a `service_account_json` provider without the feature
    //! produces a clear error at first mint instead of a silent panic.
    use std::sync::Arc;

    use super::error::CredentialError;
    use super::material::GoogleServiceAccountKey;
    use super::token::Token;

    pub(super) async fn mint(
        provider_id: &str,
        _key: &Arc<GoogleServiceAccountKey>,
        _scope: &str,
        _http: &reqwest::Client,
    ) -> Result<Token, CredentialError> {
        Err(CredentialError::InvalidMaterial {
            provider_id: provider_id.to_owned(),
            reason: "credentials_kind 'service_account_json' requires the \
                     `credentials-google` feature to be enabled at build time"
                .to_owned(),
        })
    }
}

mod token;

pub use broker::{AwakenCredentialBroker, CredentialBroker};
pub use error::CredentialError;
pub use material::{CredentialKind, CredentialMaterial, GoogleServiceAccountKey, build_material};
pub use token::TokenLease;
