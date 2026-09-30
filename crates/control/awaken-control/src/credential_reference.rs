// @formal surface requirement=credential.lifecycle.credential_lifecycle
// @formal reason: This source implements the repository-controlled credential lifecycle product boundary
// @formal reason: and is classified against its product requirement; this link is not a formal-proof
// @formal reason: claim.
//! Credential aggregate adapter for Agent publication.
//!
//! The config domain supplies the trusted execution Workspace and an exact
//! secret-free revision. This adapter reads the existing credential aggregate
//! and enforces ownership, lifecycle, and revision without materializing secret
//! bytes.

use std::sync::Arc;

use awaken_config_service::CredentialReferenceValidator;
use awaken_credential_contract::CredentialSourceId;
use awaken_credential_vault::CredentialStatus;
use awaken_credential_vault::repo::CredentialRepo;
use awaken_runtime_contract::credential::CredentialRef;
use awaken_tenancy::ScopeId;

pub struct CredentialRevisionValidator {
    credentials: Arc<dyn CredentialRepo>,
    /// The process's one wall clock: the instant a descriptor's expiry is
    /// judged at.
    clock: Arc<dyn awaken_lease_contract::WallClock>,
}

impl CredentialRevisionValidator {
    #[must_use]
    pub fn new(
        credentials: Arc<dyn CredentialRepo>,
        clock: Arc<dyn awaken_lease_contract::WallClock>,
    ) -> Self {
        Self { credentials, clock }
    }
}

#[async_trait::async_trait]
impl awaken_resource_contract::RepositoryCredentialValidator for CredentialRevisionValidator {
    async fn validate(
        &self,
        workspace_id: &str,
        source_id: &str,
        remote_url: &str,
    ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
        use awaken_resource_contract::ResourceRegistryError;
        let unavailable = || ResourceRegistryError::AdmissionRejected {
            detail:
                "Choose an active repository authorization in this Workspace for this Git server"
                    .into(),
        };
        // Session-owned material is retired with its Session and must not be
        // promoted into a reusable Agent resource, even if its target matches.
        if source_id.starts_with("managed:") || source_id.starts_with("profiled:") {
            return Err(unavailable());
        }
        let source = self
            .credentials
            .get(&CredentialSourceId(source_id.into()))
            .await
            .map_err(repository_credential_lookup_error)?;
        if source.workspace_id != workspace_id
            || source.status != CredentialStatus::Active
            || source.version < 1
        {
            return Err(unavailable());
        }
        source.validate_authority().map_err(|_| unavailable())?;
        let descriptor = source.descriptor.as_ref().ok_or_else(unavailable)?;
        if !matches!(&descriptor.material,
            awaken_credential_contract::CredentialMaterialDescriptor::Structured { type_id, fields }
            if type_id.0 == awaken_credential_contract::HTTP_BASIC_MATERIAL_TYPE
                && fields.len() == 2 && fields.contains("username") && fields.contains("password")
        ) {
            return Err(unavailable());
        }
        let target =
            awaken_session_resource_contract::repository_transport_credential_target(remote_url)
                .map_err(|_| unavailable())?;
        source
            .validate_access_target(
                Some(&target),
                &awaken_session_resource_contract::repository_transport_credential_usage(),
            )
            .map_err(|_| unavailable())?;
        let now = awaken_lease_contract::WallClockExt::now_ms(self.clock.as_ref());
        descriptor.validate_expiry(now).map_err(|_| unavailable())
    }
}

fn repository_credential_lookup_error(
    error: awaken_credential_vault::CredentialError,
) -> awaken_resource_contract::ResourceRegistryError {
    use awaken_resource_contract::ResourceRegistryError;
    match error {
        awaken_credential_vault::CredentialError::SourceNotFound(_) => {
            ResourceRegistryError::AdmissionRejected {
                detail: "Choose an active repository authorization in this Workspace for this Git server".into(),
            }
        }
        _ => ResourceRegistryError::AuthorityUnavailable { detail: "Repository authorization could not be checked; retry when the credential service is available".into() },
    }
}

#[async_trait::async_trait]
impl CredentialReferenceValidator for CredentialRevisionValidator {
    async fn validate(
        &self,
        workspace: &ScopeId,
        credential: &CredentialRef,
    ) -> Result<(), String> {
        let source = self
            .credentials
            .get(&CredentialSourceId(credential.id.clone()))
            .await
            .map_err(|_| "MCP credential is unavailable in this Workspace".to_string())?;
        let revision = u64::try_from(source.version).ok();
        if source.workspace_id != workspace.as_str()
            || source.status != CredentialStatus::Active
            || revision != Some(credential.revision)
        {
            return Err(
                "MCP credential is unavailable in this Workspace at the requested revision"
                    .to_string(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::RedactedString;
    use awaken_credential_vault::repo::{InMemoryCredentialRepo, enter_credential};
    use awaken_credential_vault::{CredentialCreateParams, CredentialKind, InMemorySecretStore};

    #[test]
    fn repository_credential_outage_is_not_reported_as_invalid_user_input() {
        use awaken_credential_vault::CredentialError;
        use awaken_resource_contract::ResourceRegistryError;
        let error = repository_credential_lookup_error(CredentialError::Storage(
            "private database location".into(),
        ));
        assert!(matches!(
            error,
            ResourceRegistryError::AuthorityUnavailable { .. }
        ));
        assert!(!error.to_string().contains("private database location"));
        assert!(matches!(
            repository_credential_lookup_error(CredentialError::SourceNotFound(
                "foreign-id".into()
            )),
            ResourceRegistryError::AdmissionRejected { .. }
        ));
    }

    #[tokio::test]
    async fn repository_authorization_checks_scope_target_lifecycle_shape_and_session_ownership() {
        use awaken_resource_contract::RepositoryCredentialValidator;
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let source = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: None,
                env_key: None,
                secret: Some(RedactedString::new("test-only")),
                oauth_command: None,
            },
            &InMemorySecretStore::new(),
            credentials.as_ref(),
            awaken_lease_contract::system_clock().as_ref(),
        )
        .await
        .unwrap();
        // These are intentionally metadata fixtures, not proof that Git fetched
        // private bytes. This adapter must never open the material store.
        let mut source = source;
        source.id = CredentialSourceId("github-reusable".into());
        source.descriptor = Some(serde_json::from_value(serde_json::json!({
            "provider": "github",
            "material": { "kind": "structured", "type_id": "awaken.http-basic/v1", "fields": ["password", "username"] },
            "targets": [{ "target": { "purpose": { "type": "repository_transport" }, "audience": "https://github.com/git" }, "usage": { "type": "http_basic_auth" } }]
        })).unwrap());
        let validator = CredentialRevisionValidator::new(
            credentials.clone(),
            awaken_lease_contract::system_clock(),
        );
        for case in 0..14 {
            let mut candidate = source.clone();
            let mut workspace = "workspace-a";
            let mut remote = "https://github.com/org/repository.git";
            match case {
                0 => {}
                1 => workspace = "workspace-b",
                2 => remote = "https://github.enterprise.example/org/repository.git",
                3 => candidate.status = CredentialStatus::Archived,
                4 => candidate.descriptor.as_mut().unwrap().expires_at_unix_ms = Some(1),
                5 => candidate.descriptor = None,
                6 => candidate.provider_id = Some("github".into()),
                7 => {
                    candidate.descriptor.as_mut().unwrap().material =
                        awaken_credential_contract::CredentialMaterialDescriptor::secret(
                            "awaken.secret/v1",
                        )
                }
                8 => {
                    candidate.id =
                        CredentialSourceId("managed:session:repository:id:credential".into())
                }
                9 => {
                    candidate.id =
                        CredentialSourceId("profiled:session:repository:id:credential".into())
                }
                10 => remote = "https://github.com:8443/org/repository.git",
                11 => candidate.version = 0,
                12 => {
                    candidate.descriptor.as_mut().unwrap().targets[0]
                        .target
                        .purpose = awaken_credential_contract::CredentialPurpose::ProviderAdapter
                }
                _ => {
                    if let awaken_credential_contract::CredentialMaterialDescriptor::Structured {
                        fields,
                        ..
                    } = &mut candidate.descriptor.as_mut().unwrap().material
                    {
                        fields.remove("username");
                    }
                }
            }
            credentials.put(candidate.clone()).await.unwrap();
            let result = RepositoryCredentialValidator::validate(
                &validator,
                workspace,
                &candidate.id.0,
                remote,
            )
            .await;
            assert_eq!(
                result.is_ok(),
                case == 0,
                "admission case {case}: {result:?}"
            );
        }
        assert!(
            RepositoryCredentialValidator::validate(
                &validator,
                "workspace-a",
                "missing",
                "https://github.com/org/repo"
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn validator_accepts_only_the_exact_active_workspace_revision() {
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let secrets = InMemorySecretStore::new();
        let source = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: None,
                env_key: None,
                secret: Some(RedactedString::new("secret")),
                oauth_command: None,
            },
            &secrets,
            credentials.as_ref(),
            awaken_lease_contract::system_clock().as_ref(),
        )
        .await
        .unwrap();
        let validator =
            CredentialRevisionValidator::new(credentials, awaken_lease_contract::system_clock());
        let exact = CredentialRef {
            id: source.id.0,
            revision: 1,
        };

        assert!(
            validator
                .validate(&ScopeId::from("workspace-a"), &exact)
                .await
                .is_ok()
        );
        assert!(
            validator
                .validate(&ScopeId::from("workspace-b"), &exact)
                .await
                .is_err()
        );
        assert!(
            validator
                .validate(
                    &ScopeId::from("workspace-a"),
                    &CredentialRef {
                        revision: 2,
                        ..exact
                    },
                )
                .await
                .is_err()
        );
    }
}
