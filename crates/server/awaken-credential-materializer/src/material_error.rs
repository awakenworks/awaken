use awaken_runtime_contract::CredentialMaterialError;

/// Preserve the credential domain's integrity distinction while keeping
/// missing or temporarily inaccessible material retryable at execution edges.
pub(crate) fn classify_secret_store_error(
    error: awaken_credential_vault::CredentialError,
) -> CredentialMaterialError {
    match error {
        awaken_credential_vault::CredentialError::Seal => CredentialMaterialError::Invalid,
        _ => CredentialMaterialError::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;

    use awaken_agent_contract::RedactedString;
    use awaken_credential_vault::repo::{InMemoryCredentialRepo, enter_credential};
    use awaken_credential_vault::{
        CredentialCreateParams, CredentialError, CredentialKind, InMemorySecretStore, SecretRef,
        SecretStore,
    };
    use awaken_runtime_contract::{
        CredentialAccess, CredentialExecutionPolicy, CredentialMaterialError,
        CredentialMaterialSource, CredentialRealizationKind, CredentialRef, CredentialUsage,
        HttpEffectPlacement, ModelExposurePolicy, PlaintextBoundary, PlaintextHolder,
    };

    use crate::PinnedCredentialMaterializer;

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
        let source = enter_credential(
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
        )
        .await
        .unwrap();
        let holder = PlaintextHolder::new(PlaintextBoundary::Platform, "gateway.beta");
        let access = CredentialAccess::new(
            CredentialRef {
                id: source.id.0,
                revision: u64::try_from(source.version).unwrap(),
            },
            CredentialMaterialSource::ControlPlaneReference,
            CredentialUsage::HttpEffect {
                fields: BTreeMap::from([(
                    "token".into(),
                    BTreeSet::from([HttpEffectPlacement::Header {
                        name: "authorization".into(),
                    }]),
                )]),
            },
            CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::Forbidden),
        )
        .with_target(awaken_credential_contract::CredentialTarget::new(
            awaken_credential_contract::CredentialPurpose::HttpEffect,
            "https://api.github.com",
        ));

        let error = PinnedCredentialMaterializer::new(credentials, Arc::new(SealErrorSecretStore))
            .resolve_for_workspace(
                &access,
                &holder,
                CredentialRealizationKind::PlatformRelay,
                "workspace-a",
                &("route-1", "POST", "/issues"),
            )
            .await
            .unwrap_err();

        assert_eq!(error, CredentialMaterialError::Invalid, "E2");
    }
}
