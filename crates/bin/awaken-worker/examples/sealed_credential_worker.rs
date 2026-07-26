//! E2E fixture for ADR-0067 recipient-bound credential resolution.
//!
//! The resolver is deliberately small but implements the production public
//! contract: exact source capability, recipient-bound-envelope evidence,
//! Workspace/target-use binding, payload fingerprint, credential and holder.

use std::collections::BTreeSet;
use std::sync::Arc;

use awaken_agent_contract::RedactedString;
use awaken_runtime_contract::{
    CredentialEnvelope, CredentialMaterialBinding, CredentialMaterialError,
    CredentialMaterialRequest, CredentialMaterialResolver, CredentialMaterialSource, CredentialRef,
    CredentialUsage, InferenceEndpoint, PlaintextBoundary, PlaintextHolder,
    ResolvedCredentialMaterial,
};

struct ExactSealedResolver {
    credential: CredentialRef,
    holder: PlaintextHolder,
    binding: CredentialMaterialBinding,
    payload_fingerprint: String,
    material: String,
}

#[async_trait::async_trait]
impl CredentialMaterialResolver for ExactSealedResolver {
    fn supported_material_sources(&self) -> BTreeSet<CredentialMaterialSource> {
        BTreeSet::from([CredentialMaterialSource::ControlPlaneReference])
    }

    fn supports_recipient_bound_envelopes(&self) -> bool {
        true
    }

    async fn resolve_exact(
        &self,
        request: CredentialMaterialRequest<'_>,
    ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError> {
        request.binding.validate()?;
        if request.binding != &self.binding {
            return Err(CredentialMaterialError::BindingMismatch);
        }
        if request.access.credential != self.credential || request.selected_holder != &self.holder {
            return Err(CredentialMaterialError::ResolverMismatch);
        }
        let payload_fingerprint = match request.access.envelope.as_ref() {
            Some(CredentialEnvelope::SealedForWorker { envelope_ref, .. }) => {
                &envelope_ref.payload_fingerprint
            }
            Some(CredentialEnvelope::SealedForWorkload { .. }) | None => {
                return Err(CredentialMaterialError::RecipientMismatch);
            }
        };
        if payload_fingerprint != &self.payload_fingerprint {
            return Err(CredentialMaterialError::PayloadMismatch);
        }
        Ok(ResolvedCredentialMaterial {
            credential: self.credential.clone(),
            holder: self.holder.clone(),
            material: awaken_runtime_contract::CredentialMaterial::bearer(RedactedString::new(
                self.material.clone(),
            )),
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    awaken_observability::init();
    let upstream = std::env::var("AWAKEN_UPSTREAM_URL")?;
    let provider_ref = std::env::var("AWAKEN_TEST_PROVIDER_REF")?;
    let endpoint = InferenceEndpoint {
        adapter_kind: "anthropic".into(),
        base_url: std::env::var("AWAKEN_TEST_PROVIDER_URL")?,
        upstream_model: std::env::var("AWAKEN_TEST_PROVIDER_MODEL")?,
    };
    let workspace = std::env::var("AWAKEN_TEST_WORKSPACE")?;
    let credential = CredentialRef {
        id: std::env::var("AWAKEN_TEST_CREDENTIAL_ID")?,
        revision: std::env::var("AWAKEN_TEST_CREDENTIAL_REVISION")?.parse()?,
    };
    let holder = PlaintextHolder::new(
        PlaintextBoundary::Worker,
        awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
    );
    let resolver = Arc::new(ExactSealedResolver {
        credential,
        holder,
        binding: CredentialMaterialBinding::for_target(
            &workspace,
            &(provider_ref.as_str(), &endpoint),
            &CredentialUsage::ProviderAdapter,
        ),
        payload_fingerprint: std::env::var("AWAKEN_TEST_PAYLOAD_FINGERPRINT")?,
        material: std::env::var("AWAKEN_TEST_PROVIDER_SECRET")?,
    });
    let credentials = Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
    let secrets = Arc::new(awaken_credential_vault::InMemorySecretStore::new());
    awaken_worker::WorkerNodeBuilder::new(awaken_runtime_host::WorkerUpstream::new(upstream))
        .with_credential_stores(credentials, secrets)
        .with_external_credential_resolver(resolver)
        .with_standard_manifest(Default::default())
        .with_admin_listen(std::env::var("AWAKEN_WORKER_ADMIN_LISTEN")?)
        .build()?
        .run_until_shutdown()
        .await
}
