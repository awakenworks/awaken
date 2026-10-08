//! The credential vocabulary the Runtime SDK reads (ADR-0086 §7).
//!
//! Snapshot routes carry [`CredentialAccess`]: a secret-free reference to one
//! credential revision, with its target, usage, plaintext-holder policy and
//! model exposure. Outbound wire clients take an already-resolved
//! [`Credential`] and may ask the host to refresh it. Custody, sealing,
//! opening, issuance, rollout and realization admission are Platform concerns
//! of `awaken-credential-contract`, which re-exports these values so Platform
//! paths and type identity do not change.

mod outbound;
mod reference;
mod target;
mod usage;
#[cfg(test)]
mod wire_tests;

pub use outbound::{AuthChallenge, Credential, CredentialRefresher};
pub use reference::{
    CandidateFingerprint, CredentialAccess, CredentialEnvelope, CredentialExecutionPolicy,
    CredentialMaterialSource, CredentialRef, CredentialRefreshAccess, ModelExposurePolicy,
    PlaintextBoundary, PlaintextHolder, SELF_HOSTED_ACP_TRUST_DOMAIN,
    SELF_HOSTED_SANDBOX_TRUST_DOMAIN, SELF_HOSTED_WORKER_TRUST_DOMAIN, SealedCredentialEnvelopeRef,
    TokenEndpointAuth, TrustDomainRef,
};
pub use target::{CredentialAudienceRef, CredentialPurpose, CredentialTarget};
pub use usage::{
    CredentialUsage, CredentialUsageError, HttpEffectPlacement, PRIVATE_FILE_CONSUMER,
    PRIVATE_FILE_MATERIAL_TYPE, PROCESS_ENV_CONSUMER, PROCESS_ENV_MATERIAL_TYPE,
    SignatureVerificationAlgorithm, is_core_delivery_consumer,
};
