//! The credential reference vocabulary snapshot routes carry.
//!
//! These values are owned by `awaken-agent-contract` and re-exported here so
//! snapshot readers name them beside the routes that carry them. Custody,
//! realization admission and the attempt-level bindings, deliveries and
//! receipts are Platform vocabulary in `awaken-credential-contract`; the engine
//! carries an attempt's credential realization only as the opaque
//! [`CredentialRealizationHandle`](crate::CredentialRealizationHandle)
//! (ADR-0086 §7).

pub use awaken_agent_contract::credential::{
    CandidateFingerprint, CredentialAccess, CredentialAudienceRef, CredentialEnvelope,
    CredentialExecutionPolicy, CredentialMaterialSource, CredentialPurpose, CredentialRef,
    CredentialRefreshAccess, CredentialTarget, CredentialUsage, CredentialUsageError,
    HttpEffectPlacement, ModelExposurePolicy, PRIVATE_FILE_CONSUMER, PRIVATE_FILE_MATERIAL_TYPE,
    PROCESS_ENV_CONSUMER, PROCESS_ENV_MATERIAL_TYPE, PlaintextBoundary, PlaintextHolder,
    SELF_HOSTED_ACP_TRUST_DOMAIN, SELF_HOSTED_SANDBOX_TRUST_DOMAIN,
    SELF_HOSTED_WORKER_TRUST_DOMAIN, SealedCredentialEnvelopeRef, SignatureVerificationAlgorithm,
    TokenEndpointAuth, TrustDomainRef, is_core_delivery_consumer,
};
