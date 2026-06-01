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
//! Static bearers bypass the broker on the production hot path (see
//! `awaken_server::services::config_runtime::build_genai_provider_executor`)
//! because there is no token to refresh.
//! The broker still accepts static-bearer material for embedders that
//! want everything funnelled through one chokepoint and for tests.
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
//! Bearer providers without `api_key` fail closed by default. Set
//! `ProviderSpec.adapter_options.allow_env_credentials = true` only when the
//! provider should intentionally use the host environment's adapter-specific
//! credential variable.
//!
//! Compatibility rules and validation live in [`material::build_material`].
//!
//! ## Disabled-feature gating
//! `service_account_json` requires the `credentials-google` cargo feature.
//! When the feature is off, `build_material` rejects the configuration at
//! the server boundary (config write time) with a clear error — there is
//! no runtime stub mod swapped in via cfg.

pub mod broker;
pub mod error;
pub mod material;
mod minter;

#[cfg(any(test, feature = "credentials-google"))]
pub mod google_oauth;

mod token;

pub use broker::{AwakenCredentialBroker, CredentialBroker, CredentialRetryPolicy};
pub use error::CredentialError;
pub use material::{
    CredentialKind, CredentialMaterial, GoogleServiceAccountKey,
    allow_env_credentials_from_options, build_material, build_material_allowing_env_fallback,
};
pub use token::IssuedToken;
