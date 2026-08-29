//! Deterministic claim authority used only by scenario-host model adapters.

use std::sync::Arc;

use awaken_runtime_contract::resolved::{ModelProvisioning, ResolvedModelCandidate};

pub(crate) fn direct_provider_execution() -> (
    awaken_runtime_contract::CredentialExecutionPolicy,
    awaken_runtime_contract::PlaintextHolder,
) {
    // Scenario deployment decision D1: direct provider bytes are materialized
    // by the canonical self-hosted Worker adapter. Publication and attempt
    // context must consume this same explicit pair; neither may rediscover a
    // holder from config or choose an ambient process boundary independently.
    let policy = awaken_runtime_contract::CredentialExecutionPolicy::self_hosted_provider();
    let holder = policy
        .allowed_plaintext_holders
        .iter()
        .next()
        .expect("self-hosted provider policy has one exact plaintext holder")
        .clone();
    (policy, holder)
}

fn native_binding(
    candidate: &ResolvedModelCandidate,
) -> awaken_runtime_contract::AttemptCredentialBinding {
    let credential = match candidate.provisioning() {
        ModelProvisioning::Provider {
            credential: Some(access),
            ..
        } => access.credential.clone(),
        _ => panic!("scenario provider candidate must carry a credential"),
    };
    awaken_runtime_contract::AttemptCredentialBinding {
        candidate_fingerprint: awaken_runtime_contract::candidate_fingerprint(candidate)
            .expect("fingerprint scenario candidate"),
        credential,
        selected_plaintext_holder: direct_provider_execution().1,
        selected_realization_kind:
            awaken_runtime_contract::CredentialRealizationKind::WorkerProviderAdapter,
        claim_epoch: 1,
    }
}

struct CurrentOwnership;

#[async_trait::async_trait]
impl awaken_runtime_contract::AttemptOwnershipVerifier for CurrentOwnership {
    async fn verify_current(&self) -> Result<(), awaken_runtime_contract::AttemptOwnershipError> {
        Ok(())
    }
}

struct ReceiptSink;

#[async_trait::async_trait]
impl awaken_runtime_contract::CredentialRealizationRecorder for ReceiptSink {
    async fn record(
        &self,
        _receipt: awaken_runtime_contract::CredentialRealizationReceipt,
    ) -> Result<(), awaken_runtime_contract::CredentialRealizationRecordError> {
        Ok(())
    }
}

pub(crate) fn context(
    candidate: &ResolvedModelCandidate,
) -> awaken_runtime_contract::RuntimeRunContext {
    awaken_runtime_contract::RuntimeRunContext::new()
        .with_ownership(Arc::new(CurrentOwnership))
        .with_credential_realization(awaken_runtime_contract::AttemptCredentialRealization::new(
            vec![native_binding(candidate)],
            Arc::new(ReceiptSink),
        ))
}
