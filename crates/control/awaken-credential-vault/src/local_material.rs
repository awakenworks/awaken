//! Control-owned local credential material and write-back adapter.

use std::collections::BTreeSet;
use std::sync::Arc;

use awaken_agent_contract::RedactedString;
use awaken_credential_contract::{
    CredentialAccess, CredentialMaterial, CredentialMaterialBinding, CredentialMaterialCapability,
    CredentialMaterialError, CredentialMaterialRequest, CredentialMaterialResolver,
    CredentialMaterialSource, CredentialProviderExpectation, CredentialSourceId, CredentialUsage,
    OAuthCredentialMaterial, PlaintextBoundary, PlaintextHolder, ResolvedCredentialMaterial,
    SELF_HOSTED_ACP_TRUST_DOMAIN, SELF_HOSTED_WORKER_TRUST_DOMAIN,
};
use awaken_time::WallClockExt as _;

use crate::repo::{
    CredentialMaterialPatch, CredentialRepo, rotate_credential_materials_exact,
    write_back_credential_material_for_effect,
};
use crate::{CredentialError, CredentialSource, CredentialStatus, SecretRef, SecretStore};

/// The sole local Vault authority installed into a neutral materializer.
/// One instance serves exact reads and the existing Sandbox `SecretBroker`
/// write-back port over the same repository and sealed-byte store.
#[derive(Clone)]
pub struct VaultLocalMaterialAuthority {
    credentials: Arc<dyn CredentialRepo>,
    secrets: Arc<dyn SecretStore>,
    clock: Arc<dyn awaken_time::WallClock>,
}

/// Last-mile status gate after the current source row is loaded and before
/// any SecretStore I/O. Publication-time status is never sufficient.
const fn credential_status_allows_materialization(status: CredentialStatus) -> bool {
    matches!(status, CredentialStatus::Active)
}

#[cfg(kani)]
/// @obligation credential_effect.boundary_preserves_exact_materialization_authority
/// @obligation worker.revoked_credential_blocks_new_materialization
/// @proof-input-debt tautology credential_status_allows_materialization
#[kani::proof]
fn new_materialization_admits_only_active_credential_status() {
    // Causes: current persisted lifecycle Active, Disabled, or Archived.
    // Effects: only Active admits a new material read; both revoked states
    // fail before SecretStore I/O. Rules M1=Active=>admit; M2=Disabled=>deny;
    // M3=Archived=>deny. The proof calls the same predicate as the adapter.
    let status = match kani::any::<u8>() % 3 {
        0 => CredentialStatus::Active,
        1 => CredentialStatus::Disabled,
        _ => CredentialStatus::Archived,
    };
    assert_eq!(
        credential_status_allows_materialization(status),
        matches!(status, CredentialStatus::Active)
    );
}

impl VaultLocalMaterialAuthority {
    #[must_use]
    pub fn new(
        credentials: Arc<dyn CredentialRepo>,
        secrets: Arc<dyn SecretStore>,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self {
            credentials,
            secrets,
            clock,
        }
    }

    async fn load_active_source(
        &self,
        reference: &str,
    ) -> Result<CredentialSource, CredentialMaterialError> {
        let source = self
            .credentials
            .get(&CredentialSourceId(reference.to_string()))
            .await
            .map_err(|_| CredentialMaterialError::Unavailable)?;
        source
            .validate_authority()
            .map_err(|_| CredentialMaterialError::Invalid)?;
        if !credential_status_allows_materialization(source.status) {
            return Err(CredentialMaterialError::Unavailable);
        }
        Ok(source)
    }

    async fn materialize_source(
        &self,
        source: &CredentialSource,
    ) -> Result<RedactedString, CredentialMaterialError> {
        crate::materialize(source, self.secrets.as_ref())
            .await
            .map_err(classify_secret_store_error)
    }

    /// The sole use-time source authorization decision for ordinary local
    /// resolution and attempt envelope issuance. All checks precede material
    /// store I/O, and an absent Provider coordinate fails closed.
    fn validate_exact_source(
        &self,
        source: &CredentialSource,
        access: &CredentialAccess,
        selected_holder: &PlaintextHolder,
        binding: &CredentialMaterialBinding,
        expected_provider: Option<CredentialProviderExpectation<'_>>,
    ) -> Result<(), CredentialMaterialError> {
        binding.validate()?;
        if access.material_source != CredentialMaterialSource::ControlPlaneReference
            || access.envelope.is_some()
        {
            return Err(CredentialMaterialError::Unavailable);
        }
        if !access
            .policy
            .allowed_plaintext_holders
            .contains(selected_holder)
        {
            return Err(CredentialMaterialError::RecipientMismatch);
        }
        awaken_credential_contract::validate_credential_target_usage(
            access.target.as_ref(),
            &access.usage,
        )
        .map_err(|_| CredentialMaterialError::BindingMismatch)?;
        if access.usage == CredentialUsage::ProviderAdapter && expected_provider.is_none() {
            return Err(CredentialMaterialError::BindingMismatch);
        }
        let revision =
            u64::try_from(source.version).map_err(|_| CredentialMaterialError::RevisionMismatch)?;
        if revision != access.credential.revision {
            return Err(CredentialMaterialError::RevisionMismatch);
        }
        if source.workspace_id != binding.workspace_id {
            return Err(CredentialMaterialError::RecipientMismatch);
        }
        if expected_provider.is_some_and(
            |CredentialProviderExpectation {
                 provider_id,
                 protocol_endpoint_id,
             }| {
                !source
                    .authorization_scope()
                    .authorizes(provider_id, protocol_endpoint_id)
            },
        ) {
            return Err(CredentialMaterialError::RecipientMismatch);
        }
        source
            .validate_access_target(access.target.as_ref(), &access.usage)
            .map_err(|_| CredentialMaterialError::BindingMismatch)?;
        if let Some(descriptor) = &source.descriptor {
            descriptor
                .validate_expiry(self.clock.now_ms())
                .map_err(|error| match error {
                    awaken_credential_contract::CredentialDescriptorError::Expired => {
                        CredentialMaterialError::Expired
                    }
                    _ => CredentialMaterialError::Invalid,
                })?;
        }
        Ok(())
    }
}

/// A sealed material integrity failure is terminal; absence and unavailable
/// backends remain retryable. This is the only Vault-error classifier used by
/// the local resolver and its Sandbox broker.
fn classify_secret_store_error(error: CredentialError) -> CredentialMaterialError {
    match error {
        CredentialError::Seal => CredentialMaterialError::Invalid,
        _ => CredentialMaterialError::Unavailable,
    }
}

#[async_trait::async_trait]
impl CredentialMaterialResolver for VaultLocalMaterialAuthority {
    fn material_capabilities(&self) -> BTreeSet<CredentialMaterialCapability> {
        [
            PlaintextHolder::new(PlaintextBoundary::Worker, SELF_HOSTED_WORKER_TRUST_DOMAIN),
            PlaintextHolder::new(PlaintextBoundary::Workload, SELF_HOSTED_ACP_TRUST_DOMAIN),
        ]
        .into_iter()
        .map(|holder| CredentialMaterialCapability {
            holder,
            source: CredentialMaterialSource::ControlPlaneReference,
            envelope: false,
        })
        .collect()
    }

    async fn resolve_exact(
        &self,
        request: CredentialMaterialRequest<'_>,
    ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError> {
        let CredentialMaterialRequest {
            access,
            selected_holder,
            binding,
            expected_provider,
        } = request;
        if access.usage == CredentialUsage::ProviderAdapter && expected_provider.is_none() {
            // Reject missing, one-way endpoint evidence before even opening
            // the source row; the same rule is rechecked at the source below.
            return Err(CredentialMaterialError::BindingMismatch);
        }
        let source = self.load_active_source(&access.credential.id).await?;
        self.validate_exact_source(&source, access, selected_holder, binding, expected_provider)?;
        let access_token = self.materialize_source(&source).await?;
        let material = if access.usage == CredentialUsage::ProviderAdapter
            && let Some(refresh) = &access.refresh
        {
            let source_ref = source
                .material_ref
                .as_ref()
                .ok_or(CredentialMaterialError::Unavailable)?;
            if source_ref.0 != refresh.access_token_ref {
                return Err(CredentialMaterialError::BindingMismatch);
            }
            let refresh_token = self
                .secrets
                .get(&SecretRef(refresh.refresh_token_ref.clone()))
                .await
                .map_err(classify_secret_store_error)?;
            CredentialMaterial::OAuth(OAuthCredentialMaterial {
                access_token,
                refresh_token,
                expires_at_unix_ms: refresh.expires_at_unix_ms,
                account_id: refresh.account_id.clone(),
                account_plan: refresh.account_plan.clone(),
            })
        } else {
            match crate::decode_structured_material(access_token)
                .map_err(|_| CredentialMaterialError::Invalid)?
            {
                Ok(material) => CredentialMaterial::Structured(material),
                Err(secret) => CredentialMaterial::secret(secret),
            }
        };
        material.validate_usage(&access.usage)?;
        if let Some(descriptor) = &source.descriptor {
            descriptor
                .validate_material(&material)
                .map_err(|_| CredentialMaterialError::MaterialKindMismatch)?;
        }
        Ok(ResolvedCredentialMaterial {
            credential: access.credential.clone(),
            holder: selected_holder.clone(),
            material,
        })
    }
}

mod attempt_delivery;
mod broker;
pub use attempt_delivery::VaultAttemptCredentialDeliverySource;

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::repo::{CredentialRepo, InMemoryCredentialRepo, enter_credential};
    use crate::{
        CredentialCreateParams, CredentialError, CredentialKind, InMemorySecretStore, SecretRef,
        SecretStore,
    };
    use awaken_agent_contract::RedactedString;
    use awaken_credential_contract::{
        CredentialDescriptor, CredentialMaterialDescriptor, CredentialPurpose, CredentialTarget,
        CredentialTargetContract, OPAQUE_SECRET_MATERIAL_TYPE,
    };
    use awaken_runtime_contract::{
        CredentialAccess, CredentialExecutionPolicy, CredentialMaterialBinding,
        CredentialMaterialError, CredentialMaterialRequest, CredentialMaterialResolver,
        CredentialMaterialSource, CredentialRef, CredentialUsage, HttpEffectPlacement,
        ModelExposurePolicy, PlaintextBoundary, PlaintextHolder,
    };

    use super::{
        CredentialProviderExpectation, CredentialStatus, SELF_HOSTED_WORKER_TRUST_DOMAIN,
        VaultLocalMaterialAuthority,
    };

    struct CountingSecrets {
        inner: Arc<InMemorySecretStore>,
        reads: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl SecretStore for CountingSecrets {
        async fn put(
            &self,
            reference: &SecretRef,
            secret: RedactedString,
        ) -> Result<(), CredentialError> {
            self.inner.put(reference, secret).await
        }

        async fn get(&self, reference: &SecretRef) -> Result<RedactedString, CredentialError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.inner.get(reference).await
        }

        async fn delete(&self, reference: &SecretRef) -> Result<(), CredentialError> {
            self.inner.delete(reference).await
        }
    }

    async fn resolve<'a>(
        authority: &'a VaultLocalMaterialAuthority,
        access: &'a CredentialAccess,
        holder: &'a PlaintextHolder,
        binding: &'a CredentialMaterialBinding,
        expected_provider: Option<CredentialProviderExpectation<'a>>,
    ) -> Result<awaken_runtime_contract::ResolvedCredentialMaterial, CredentialMaterialError> {
        authority
            .resolve_exact(CredentialMaterialRequest {
                access,
                selected_holder: holder,
                binding,
                expected_provider,
            })
            .await
    }

    #[tokio::test]
    async fn exact_provider_scope_is_rechecked_before_any_secret_read() {
        // Causes: C1 id/revision/Workspace/holder exact; C2 provider+endpoint
        // present and exact; C3 source active. Effects: E1 only C1+C2+C3
        // yields material; E2 a missing/wrong coordinate, revision, Workspace,
        // holder or revoked source yields a typed failure and zero new SecretStore
        // reads. Decision rows R1=all exact=>E1; R2=!C2=>E2;
        // R3=!C1=>E2; R4=!C3=>E2. The opaque binding alone cannot satisfy C2.
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let inner = Arc::new(InMemorySecretStore::new());
        let mut source = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("provider-a".into()),
                env_key: None,
                secret: Some(RedactedString::new("exact-secret")),
                oauth_command: None,
            },
            inner.as_ref(),
            credentials.as_ref(),
            awaken_time::system_clock().as_ref(),
        )
        .await
        .unwrap();
        source.protocol_endpoint_id = Some("endpoint-1".into());
        credentials.put(source.clone()).await.unwrap();
        let secrets = Arc::new(CountingSecrets {
            inner,
            reads: AtomicUsize::new(0),
        });
        let authority = VaultLocalMaterialAuthority::new(
            credentials.clone(),
            secrets.clone(),
            awaken_time::system_clock(),
        );
        let holder =
            PlaintextHolder::new(PlaintextBoundary::Worker, SELF_HOSTED_WORKER_TRUST_DOMAIN);
        let access = CredentialAccess::new(
            CredentialRef {
                id: source.id.0.clone(),
                revision: u64::try_from(source.version).unwrap(),
            },
            CredentialMaterialSource::ControlPlaneReference,
            CredentialUsage::ProviderAdapter,
            CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::Forbidden),
        )
        .with_target(CredentialTarget::new(
            CredentialPurpose::ProviderAdapter,
            "provider-a",
        ));
        let binding =
            CredentialMaterialBinding::for_target("workspace-a", &"provider-a", &access.usage);
        let exact = CredentialProviderExpectation {
            provider_id: "provider-a",
            protocol_endpoint_id: Some("endpoint-1"),
        };
        assert!(
            resolve(&authority, &access, &holder, &binding, Some(exact))
                .await
                .is_ok(),
            "R1/E1"
        );
        assert_eq!(secrets.reads.load(Ordering::SeqCst), 1, "R1/E1 one read");
        assert_eq!(
            resolve(&authority, &access, &holder, &binding, None)
                .await
                .unwrap_err(),
            CredentialMaterialError::BindingMismatch,
            "R2/E2 missing"
        );
        assert_eq!(
            resolve(
                &authority,
                &access,
                &holder,
                &binding,
                Some(CredentialProviderExpectation {
                    provider_id: "provider-a",
                    protocol_endpoint_id: Some("endpoint-2"),
                })
            )
            .await
            .unwrap_err(),
            CredentialMaterialError::RecipientMismatch,
            "R2/E2 endpoint"
        );
        let mut wrong_revision = access.clone();
        wrong_revision.credential.revision += 1;
        assert_eq!(
            resolve(&authority, &wrong_revision, &holder, &binding, Some(exact))
                .await
                .unwrap_err(),
            CredentialMaterialError::RevisionMismatch,
            "R3/E2 revision"
        );
        let wrong_workspace =
            CredentialMaterialBinding::for_target("workspace-b", &"provider-a", &access.usage);
        assert_eq!(
            resolve(&authority, &access, &holder, &wrong_workspace, Some(exact))
                .await
                .unwrap_err(),
            CredentialMaterialError::RecipientMismatch,
            "R3/E2 Workspace"
        );
        let foreign_holder = PlaintextHolder::new(PlaintextBoundary::Worker, "foreign.worker");
        assert_eq!(
            resolve(&authority, &access, &foreign_holder, &binding, Some(exact))
                .await
                .unwrap_err(),
            CredentialMaterialError::RecipientMismatch,
            "R3/E2 holder"
        );
        source.status = CredentialStatus::Disabled;
        credentials.put(source).await.unwrap();
        assert_eq!(
            resolve(&authority, &access, &holder, &binding, Some(exact))
                .await
                .unwrap_err(),
            CredentialMaterialError::Unavailable,
            "R4/E2 revoked"
        );
        assert_eq!(
            secrets.reads.load(Ordering::SeqCst),
            1,
            "R2-R4/E2 no secret reads"
        );
    }

    struct SealErrorSecretStore;

    #[async_trait::async_trait]
    impl SecretStore for SealErrorSecretStore {
        async fn put(
            &self,
            _reference: &SecretRef,
            _secret: RedactedString,
        ) -> Result<(), CredentialError> {
            unreachable!("the materializer never writes credential material")
        }

        async fn get(&self, _reference: &SecretRef) -> Result<RedactedString, CredentialError> {
            Err(CredentialError::Seal)
        }

        async fn delete(&self, _reference: &SecretRef) -> Result<(), CredentialError> {
            unreachable!("the materializer never deletes credential material")
        }
    }

    /// Local material-error cause/effect graph: C1 source/revision/Workspace and
    /// holder are exact; C2 the persisted material opens; C3 the opened material
    /// matches the published usage. C1+!C2(Seal) is an absorbing typed Invalid
    /// result, while a missing/transient store remains Unavailable. Usage is not
    /// revalidated at write time; a scalar would be legal for one HttpEffect
    /// field if C2 succeeded. E1/E3 are retained by the main exact-resolution
    /// table; this case owns the previously collapsed E2 branch.
    ///
    /// | Rule | C1 exact pin | C2 store | C3 usage | Effect |
    /// |---|---|---|---|---|
    /// | E1 | T | opens | exact | resolved material |
    /// | E2 | T | seal failure | - | Invalid |
    /// | E3 | T | unavailable | - | Unavailable |
    #[tokio::test]
    async fn seal_open_failure_is_not_collapsed_into_store_unavailability() {
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let usage = CredentialUsage::HttpEffect {
            fields: BTreeMap::from([(
                "token".into(),
                BTreeSet::from([HttpEffectPlacement::Header {
                    name: "authorization".into(),
                }]),
            )]),
        };
        let target = CredentialTarget::new(CredentialPurpose::HttpEffect, "https://api.github.com");
        let mut source = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("github.com/api".into()),
                env_key: None,
                secret: Some(RedactedString::new("github-token")),
                oauth_command: None,
            },
            &InMemorySecretStore::new(),
            credentials.as_ref(),
            awaken_time::system_clock().as_ref(),
        )
        .await
        .unwrap();
        source.provider_id = None;
        source.descriptor = Some(CredentialDescriptor::new(
            "github.com/api",
            CredentialMaterialDescriptor::secret(OPAQUE_SECRET_MATERIAL_TYPE),
            [CredentialTargetContract::new(target.clone(), usage.clone())],
        ));
        credentials.put(source.clone()).await.unwrap();
        let holder = PlaintextHolder::new(PlaintextBoundary::Platform, "gateway.beta");
        let access = CredentialAccess::new(
            CredentialRef {
                id: source.id.0,
                revision: u64::try_from(source.version).unwrap(),
            },
            CredentialMaterialSource::ControlPlaneReference,
            usage,
            CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::Forbidden),
        )
        .with_target(target);

        let authority = VaultLocalMaterialAuthority::new(
            credentials,
            Arc::new(SealErrorSecretStore),
            awaken_time::system_clock(),
        );
        let binding = CredentialMaterialBinding::for_target(
            "workspace-a",
            &("route-1", "POST", "/issues"),
            &access.usage,
        );
        let error = authority
            .resolve_exact(CredentialMaterialRequest {
                access: &access,
                selected_holder: &holder,
                binding: &binding,
                expected_provider: None,
            })
            .await
            .unwrap_err();

        assert_eq!(error, CredentialMaterialError::Invalid, "E2");
    }
}
