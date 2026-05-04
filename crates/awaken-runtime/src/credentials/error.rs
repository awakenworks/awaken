//! Error types for the credential broker.
//!
//! Errors are split along the **retry policy axis**: callers (typically the
//! inference loop) decide whether to retry by inspecting the variant, not by
//! string-matching the message. The split mirrors `InferenceExecutionError`
//! (see `awaken-runtime/src/engine/executor.rs`) so credential failures
//! flow into the same classification policy.

use thiserror::Error;

/// Errors raised by the [`CredentialBroker`](super::CredentialBroker).
#[derive(Debug, Error)]
pub enum CredentialError {
    /// No provider with this id is registered with the broker.
    #[error("no credentials registered for provider '{0}'")]
    NotConfigured(String),

    /// The configured material could not be parsed (e.g. malformed
    /// service-account JSON, missing required fields). **Permanent** —
    /// retrying without a config change won't help.
    #[error("invalid credential material for provider '{provider_id}': {reason}")]
    InvalidMaterial { provider_id: String, reason: String },

    /// JWT signing or other cryptographic operation failed. **Permanent** —
    /// usually means the private key is malformed.
    #[error("signing failed for provider '{provider_id}': {reason}")]
    SigningFailed { provider_id: String, reason: String },

    /// Token endpoint returned a 4xx (auth rejected, scope unsupported,
    /// account suspended, etc.). **Permanent** — surface to user, do not
    /// retry. `body` carries the upstream response so the operator can act.
    #[error("token endpoint rejected request for provider '{provider_id}': {status} — {body}")]
    PermanentUpstream {
        provider_id: String,
        status: u16,
        body: String,
    },

    /// Token endpoint returned a 5xx or another transient failure.
    /// **Retryable** with backoff.
    #[error("token endpoint transient failure for provider '{provider_id}': {reason}")]
    TransientUpstream { provider_id: String, reason: String },

    /// Network failure reaching the token endpoint (DNS, TCP, TLS, timeout).
    /// **Retryable** with backoff.
    #[error("network error reaching token endpoint for provider '{provider_id}': {reason}")]
    Network { provider_id: String, reason: String },
}

impl CredentialError {
    /// Whether the inference loop should retry after this error.
    ///
    /// Mirrors `InferenceExecutionError::is_retryable` so the two axes stay
    /// consistent: a permanent credential failure must not be retried just
    /// because the surrounding inference layer would retry an `Provider`
    /// error.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::TransientUpstream { .. } | Self::Network { .. })
    }
}
