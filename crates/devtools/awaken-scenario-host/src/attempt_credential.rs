//! Deterministic claim authority used only by scenario-host model adapters.

use std::sync::Arc;

use awaken_runtime_contract::resolved::{ModelProvisioning, ResolvedModelCandidate};

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
        selected_plaintext_holder: awaken_runtime_contract::PlaintextHolder::new(
            awaken_runtime_contract::PlaintextBoundary::Worker,
            awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
        ),
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
