// @formal surface requirement=resource.files.resource_lifecycle
// @formal reason: The one check that every live MemoryStore and Repository binding a frozen Session manifest
// @formal reason: names is still at its frozen configuration version before a Run is admitted; the verifiers
// @formal reason: own the authority decision. Classification, not a formal-proof claim.
//! The live binding checks a frozen Resource manifest implies: each bound
//! MemoryStore and Repository must still be at the configuration version the
//! Session froze, or a Run of the Session is refused before it is admitted.

use awaken_resource_contract::{
    LiveResourceBindingVerifier, RepositoryBindingVerifier, RepositoryBindingVerifierError,
};
use awaken_session_types::RunError;

use super::{ResolvedInputSource, SessionResourceManifest};

/// The code of a Repository binding authority that returned corrupt data.
pub const REPOSITORY_BINDING_CORRUPT_CODE: &str = "repository_binding_authority_corrupt";

/// The stable code of a Repository binding failure.
#[must_use]
pub fn repository_binding_code(error: &RepositoryBindingVerifierError) -> &'static str {
    match error {
        RepositoryBindingVerifierError::InvalidRequest { .. }
        | RepositoryBindingVerifierError::Denied { .. } => "invalid_request",
        RepositoryBindingVerifierError::Conflict { .. } => {
            awaken_session_types::error::CONFLICT_CODE
        }
        RepositoryBindingVerifierError::AuthorityUnavailable { .. }
        | RepositoryBindingVerifierError::Indeterminate { .. } => "unavailable",
        RepositoryBindingVerifierError::CorruptAuthority { .. } => REPOSITORY_BINDING_CORRUPT_CODE,
    }
}

/// A Repository binding failure keeps its fault across the Run boundary: a
/// concurrently changed authority stays a conflict.
#[must_use]
pub fn repository_binding_run_error(error: &RepositoryBindingVerifierError) -> RunError {
    let message = error.to_string();
    let code = repository_binding_code(error);
    match error {
        RepositoryBindingVerifierError::InvalidRequest { .. }
        | RepositoryBindingVerifierError::Denied { .. } => {
            RunError::bad_request_classified(code, message)
        }
        RepositoryBindingVerifierError::Conflict { .. } => {
            RunError::conflict_classified(code, message)
        }
        RepositoryBindingVerifierError::AuthorityUnavailable { .. }
        | RepositoryBindingVerifierError::Indeterminate { .. } => {
            RunError::unavailable_classified(code, message)
        }
        RepositoryBindingVerifierError::CorruptAuthority { .. } => {
            RunError::classified(code, message)
        }
    }
}

/// Verify every live binding `manifest` freezes, in manifest order.
///
/// Decision table (each bound input; the first refusal ends the check):
/// | Rule | input | verifier | live binding | Effect |
/// |---|---|---|---|---|
/// | B1 | File | - | - | nothing to verify |
/// | B2 | MemoryStore | absent | - | refused: no validator is configured |
/// | B3 | MemoryStore | present | another version | refused as a bad request |
/// | B4 | Repository | absent | - | refused: no verifier is configured |
/// | B5 | Repository | present | refused | that refusal's class |
/// | B6 | any | present | the frozen version | admitted |
pub async fn verify_manifest_bindings<C: Sync>(
    manifest: &SessionResourceManifest,
    memory: Option<&dyn LiveResourceBindingVerifier>,
    repositories: Option<&dyn RepositoryBindingVerifier<C>>,
) -> Result<(), RunError> {
    let workspace = manifest.workspace_id.as_str();
    for input in manifest.resources.inputs() {
        match &input.source {
            ResolvedInputSource::File { .. } => {}
            ResolvedInputSource::MemoryStore {
                memory_store_id,
                config,
            } => memory
                .ok_or_else(|| {
                    RunError::bad_request(
                        "memory resources require a configured resource binding validator",
                    )
                })?
                .verify_memory_binding(workspace, memory_store_id.as_str(), config.version)
                .await
                .map_err(|error| RunError::bad_request(error.to_string()))?,
            ResolvedInputSource::Repository {
                repository_id,
                config,
                ..
            } => {
                repositories
                    .ok_or_else(|| {
                        RunError::bad_request(
                            "repository resources require a configured binding verifier",
                        )
                    })?
                    .verify(workspace, repository_id.as_str(), config.version, None)
                    .await
                    .map_err(|error| repository_binding_run_error(&error))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{ResolvedInput, ResolvedSessionResources};
    use awaken_resource_contract::{
        ClonePolicy, ConfigVersion, MemoryStoreConfigVersion, RepositoryConfigVersion,
        RepositoryTransport, ResourceAccess, ResourceRegistryError, RetentionPolicy,
    };

    /// Live bindings at fixed versions; a Repository refusal by id.
    struct Live {
        memory: u64,
        repository: u64,
    }

    #[async_trait::async_trait]
    impl LiveResourceBindingVerifier for Live {
        async fn verify_memory_binding(
            &self,
            _workspace_id: &str,
            id: &str,
            version: ConfigVersion,
        ) -> Result<(), ResourceRegistryError> {
            if version == ConfigVersion(self.memory) {
                Ok(())
            } else {
                Err(ResourceRegistryError::NotFound(id.into()))
            }
        }

        async fn verify_repository_binding(
            &self,
            _workspace_id: &str,
            _id: &str,
            _version: ConfigVersion,
        ) -> Result<(), ResourceRegistryError> {
            unreachable!("Repository bindings verify through their own verifier")
        }
    }

    #[async_trait::async_trait]
    impl RepositoryBindingVerifier for Live {
        async fn verify(
            &self,
            _workspace_id: &str,
            repository_id: &str,
            config_version: ConfigVersion,
            _fence: Option<&()>,
        ) -> Result<RepositoryTransport, RepositoryBindingVerifierError> {
            if config_version == ConfigVersion(self.repository) {
                Ok(RepositoryTransport::Direct)
            } else {
                Err(RepositoryBindingVerifierError::Conflict {
                    detail: repository_id.into(),
                })
            }
        }
    }

    fn input(binding: &str, source: ResolvedInputSource, mount_path: &str) -> ResolvedInput {
        ResolvedInput {
            created_at_unix_ms: None,
            updated_at_unix_ms: None,
            binding_id: binding.into(),
            source,
            mount_path: mount_path.into(),
            access: ResourceAccess::ReadOnly,
            instructions: None,
        }
    }

    fn memory(version: u64) -> ResolvedInput {
        input(
            "memory",
            ResolvedInputSource::MemoryStore {
                memory_store_id: "memory-1".into(),
                config: MemoryStoreConfigVersion {
                    memory_store_id: "memory-1".into(),
                    version: ConfigVersion(version),
                    retention_policy: RetentionPolicy::default(),
                },
            },
            "/mnt/memory",
        )
    }

    fn repository(version: u64) -> ResolvedInput {
        input(
            "repository",
            ResolvedInputSource::Repository {
                repository_id: "repo-1".into(),
                config: RepositoryConfigVersion {
                    repository_id: "repo-1".into(),
                    version: ConfigVersion(version),
                    remote_url: "https://example.test/repo.git".into(),
                    credential_binding: None,
                    initial_branch: Some("main".into()),
                    initial_commit: None,
                    clone_policy: ClonePolicy::default(),
                },
                credential: None,
            },
            "/workspace/repo",
        )
    }

    fn manifest(inputs: Vec<ResolvedInput>) -> SessionResourceManifest {
        SessionResourceManifest::new(
            "workspace",
            ResolvedSessionResources::try_new(inputs, Vec::new()).expect("valid inputs"),
        )
    }

    async fn verify(
        manifest: &SessionResourceManifest,
        verifiers: Option<&Live>,
    ) -> Result<(), RunError> {
        verify_manifest_bindings(
            manifest,
            verifiers.map(|live| live as &dyn LiveResourceBindingVerifier),
            verifiers.map(|live| live as &dyn RepositoryBindingVerifier),
        )
        .await
    }

    #[tokio::test]
    async fn manifest_binding_checks_follow_their_decision_table() {
        let live = Live {
            memory: 4,
            repository: 7,
        };
        // B1: a File input binds nothing live.
        let file = manifest(vec![input(
            "file",
            ResolvedInputSource::File {
                file_id: "file-1".into(),
            },
            "/mnt/file",
        )]);
        verify(&file, None).await.expect("B1");

        // B2/B4: a bound MemoryStore or Repository needs its verifier.
        let refused = verify(&manifest(vec![memory(4)]), None)
            .await
            .expect_err("B2");
        assert_eq!(refused.code, "invalid_request", "B2");
        let refused = verify(&manifest(vec![repository(7)]), None)
            .await
            .expect_err("B4");
        assert_eq!(refused.code, "invalid_request", "B4");

        // B3: a MemoryStore at another version is refused.
        let refused = verify(&manifest(vec![memory(3)]), Some(&live))
            .await
            .expect_err("B3");
        assert_eq!(refused.code, "invalid_request", "B3");

        // B5: a Repository refusal keeps its class.
        let refused = verify(&manifest(vec![repository(6)]), Some(&live))
            .await
            .expect_err("B5");
        assert_eq!(
            refused.code,
            awaken_session_types::error::CONFLICT_CODE,
            "B5"
        );

        // B6: bindings at their frozen versions are admitted.
        verify(&manifest(vec![memory(4), repository(7)]), Some(&live))
            .await
            .expect("B6");
    }
}
