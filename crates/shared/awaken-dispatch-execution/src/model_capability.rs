// @formal surface requirement=credential.plaintext_boundary.exact_holder
// @formal reason: Immutable credential capability selection from the admitted model candidate.
//! Immutable candidate credential capability requirement.
use awaken_agent_contract::credential::CredentialMaterialSource;
use awaken_worker_contract::{HOST_EXECUTOR_CAPABILITY, PROVIDER_CREDENTIAL_SOURCE_CAPABILITY};
/// Select the exact Worker capability required to realize one immutable model
/// candidate. Credential custody owns this mapping; dispatch placement only
/// aggregates the selected capabilities across candidates.
pub fn model_realization_capability(
    candidate: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
) -> Option<&'static str> {
    match candidate.provisioning() {
        awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor => {
            Some(HOST_EXECUTOR_CAPABILITY)
        }
        awaken_runtime_contract::resolved::ModelProvisioning::BackendOwned { .. } => {
            Some(awaken_worker_contract::WORKER_LOCAL_CREDENTIALS_CAPABILITY)
        }
        awaken_runtime_contract::resolved::ModelProvisioning::Provider {
            credential: Some(credential),
            ..
        }
        | awaken_runtime_contract::resolved::ModelProvisioning::Remote {
            credential: Some(credential),
            ..
        } if credential.material_source == CredentialMaterialSource::WorkerReference => {
            Some(awaken_worker_contract::WORKER_LOCAL_CREDENTIALS_CAPABILITY)
        }
        awaken_runtime_contract::resolved::ModelProvisioning::Provider { .. }
        | awaken_runtime_contract::resolved::ModelProvisioning::Remote {
            credential: Some(_),
            ..
        } => Some(PROVIDER_CREDENTIAL_SOURCE_CAPABILITY),
        awaken_runtime_contract::resolved::ModelProvisioning::Remote {
            credential: None, ..
        } => None,
    }
}
