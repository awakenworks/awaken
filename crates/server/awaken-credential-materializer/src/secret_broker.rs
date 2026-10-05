// @formal entrypoint credential_reference.exact_revision_never_resolves_another_revision
//! Claim-fenced one-shot secret delivery and the sole route to the injected
//! Control Vault broker for local reads and write-back.

use super::*;

const PROCESS_SECRET_REFERENCE_PREFIX: &str = "awaken-process-secret://";
const CREDENTIAL_ARTIFACT_REFERENCE_PREFIX: &str = "awaken-credential-artifact://";
const PROCESS_SECRET_TTL_MS: u64 = 60_000;

#[derive(Clone)]
pub(super) struct PendingProcessSecret {
    candidate: ResolvedModelCandidate,
    context: awaken_runtime_contract::RuntimeRunContext,
    expires_at_unix_ms: u64,
}

#[derive(Clone)]
pub(super) struct PendingCredentialArtifact {
    candidate: ResolvedModelCandidate,
    context: awaken_runtime_contract::RuntimeRunContext,
    codec: awaken_acp_contract::managed_credential::CredentialArtifactCodec,
    expires_at_unix_ms: u64,
}

impl PinnedCredentialMaterializer {
    /// Issue a short-lived, one-shot process-secret reference for the exact
    /// credential decision already frozen in the current claim. No material is
    /// opened here; the provider's `SecretBroker` call performs ownership,
    /// revision, policy, material, and receipt checks immediately before spawn.
    pub fn plan_claimed_process_secret(
        &self,
        candidate: &ResolvedModelCandidate,
        context: &awaken_runtime_contract::RuntimeRunContext,
    ) -> Result<Option<String>, String> {
        let Some((_, binding)) = Self::claimed_provider_binding(candidate, context)? else {
            return Ok(None);
        };
        if binding.selected_realization_kind != CredentialRealizationKind::ProcessSecretEnvironment
        {
            return Ok(None);
        }
        let now = self.clock.now_ms();
        let mut pending = self
            .pending_process_secrets
            .lock()
            .map_err(|_| "process-secret registry lock is poisoned".to_string())?;
        pending.retain(|_, requirement| requirement.expires_at_unix_ms > now);
        let reference = format!("{PROCESS_SECRET_REFERENCE_PREFIX}{}", uuid::Uuid::new_v4());
        pending.insert(
            reference.clone(),
            PendingProcessSecret {
                candidate: candidate.clone(),
                context: context.clone(),
                expires_at_unix_ms: now.saturating_add(PROCESS_SECRET_TTL_MS),
            },
        );
        Ok(Some(reference))
    }

    /// Issue a one-shot reference for a provider-owned credential artifact. The
    /// same claim/revision checks as direct provider execution are repeated when
    /// the sandbox broker consumes it immediately before spawn.
    pub fn plan_claimed_credential_artifact(
        &self,
        candidate: &ResolvedModelCandidate,
        context: &awaken_runtime_contract::RuntimeRunContext,
        delivery: awaken_acp_contract::managed_credential::ManagedCredentialDelivery,
    ) -> Result<
        Option<awaken_acp_contract::managed_credential::CredentialArtifactRequirement>,
        String,
    > {
        let Some((access, binding)) = Self::claimed_provider_binding(candidate, context)? else {
            return Ok(None);
        };
        if binding.selected_realization_kind != CredentialRealizationKind::PrivateSecretFile {
            return Ok(None);
        }
        let Some(artifact) = delivery.credential_artifact(access.refresh.is_some()) else {
            return Ok(None);
        };
        let now = self.clock.now_ms();
        let mut pending = self
            .pending_credential_artifacts
            .lock()
            .map_err(|_| "credential-artifact registry lock is poisoned".to_string())?;
        pending.retain(|_, requirement| requirement.expires_at_unix_ms > now);
        let reference = format!(
            "{CREDENTIAL_ARTIFACT_REFERENCE_PREFIX}{}",
            uuid::Uuid::new_v4()
        );
        pending.insert(
            reference.clone(),
            PendingCredentialArtifact {
                candidate: candidate.clone(),
                context: context.clone(),
                codec: artifact.codec,
                expires_at_unix_ms: now.saturating_add(PROCESS_SECRET_TTL_MS),
            },
        );
        Ok(Some(
            awaken_acp_contract::managed_credential::CredentialArtifactRequirement::new(
                reference,
                artifact.relative_path,
            ),
        ))
    }
}

#[async_trait::async_trait]
impl awaken_provisioning_contract::SecretBroker for PinnedCredentialMaterializer {
    async fn materialize(
        &self,
        reference: &str,
    ) -> Result<Vec<u8>, awaken_provisioning_contract::SandboxError> {
        if reference.starts_with(CREDENTIAL_ARTIFACT_REFERENCE_PREFIX) {
            let pending = self
                .pending_credential_artifacts
                .lock()
                .map_err(|_| {
                    awaken_provisioning_contract::SandboxError::backend(
                        "materialize",
                        "credential-artifact registry lock is poisoned",
                    )
                })?
                .remove(reference)
                .ok_or_else(|| {
                    awaken_provisioning_contract::SandboxError::gone(
                        "credential artifact is missing, expired, or already consumed",
                    )
                })?;
            if pending.expires_at_unix_ms <= self.clock.now_ms() {
                return Err(awaken_provisioning_contract::SandboxError::gone(
                    "credential artifact expired",
                ));
            }
            let material = self
                .materialize_claimed_provider_material(
                    &pending.candidate,
                    &pending.context,
                    CredentialRealizationKind::PrivateSecretFile,
                )
                .await
                .and_then(|material| {
                    material.ok_or_else(|| {
                        "credential_revision_unavailable: provider has no credential material"
                            .to_string()
                    })
                })
                .map_err(|error| {
                    awaken_provisioning_contract::SandboxError::backend("materialize", error)
                })?;
            return crate::credential_artifact::encode(pending.codec, material)
                .map(|artifact| artifact.bytes)
                .map_err(|error| {
                    awaken_provisioning_contract::SandboxError::backend("materialize", error)
                });
        }
        #[cfg(feature = "authority")]
        {
            return self
                .local_secret_broker
                .as_ref()
                .ok_or_else(|| {
                    awaken_provisioning_contract::SandboxError::unsupported(
                        "credential materialization requires the installed Vault authority",
                    )
                })?
                .materialize(reference)
                .await;
        }
        #[cfg(not(feature = "authority"))]
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "database-less Worker cannot materialize a local Vault reference",
        ))
    }

    async fn materialize_process(
        &self,
        reference: &str,
    ) -> Result<Vec<u8>, awaken_provisioning_contract::SandboxError> {
        if !reference.starts_with(PROCESS_SECRET_REFERENCE_PREFIX) {
            return Err(awaken_provisioning_contract::SandboxError::invalid(
                "process-secret reference is not a claim-fenced capability",
            ));
        }
        let pending = self
            .pending_process_secrets
            .lock()
            .map_err(|_| {
                awaken_provisioning_contract::SandboxError::backend(
                    "materialize_process",
                    "process-secret registry lock is poisoned",
                )
            })?
            .remove(reference)
            .ok_or_else(|| {
                awaken_provisioning_contract::SandboxError::gone(
                    "process-secret reference is missing, expired, or already consumed",
                )
            })?;
        if pending.expires_at_unix_ms <= self.clock.now_ms() {
            return Err(awaken_provisioning_contract::SandboxError::gone(
                "process-secret reference expired",
            ));
        }
        self.materialize_claimed_provider(
            &pending.candidate,
            &pending.context,
            CredentialRealizationKind::ProcessSecretEnvironment,
        )
        .await
        .and_then(|secret| {
            secret.ok_or_else(|| "process-secret requirement has no material".to_string())
        })
        .map(|secret| secret.expose_secret().as_bytes().to_vec())
        .map_err(|error| {
            awaken_provisioning_contract::SandboxError::backend("materialize_process", error)
        })
    }

    async fn write_back(
        &self,
        target: awaken_provisioning_contract::SecretWritebackTarget<'_>,
        bytes: Vec<u8>,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        #[cfg(feature = "authority")]
        {
            return self
                .local_secret_broker
                .as_ref()
                .ok_or_else(|| {
                    awaken_provisioning_contract::SandboxError::unsupported(
                        "credential write-back requires the installed Vault authority",
                    )
                })?
                .write_back(target, bytes)
                .await;
        }
        #[cfg(not(feature = "authority"))]
        {
            let _ = (target, bytes);
            Err(awaken_provisioning_contract::SandboxError::unsupported(
                "database-less Worker cannot write a local Vault reference",
            ))
        }
    }
}

#[cfg(all(test, feature = "authority"))]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use awaken_credential_contract::{
        CredentialMaterialError, CredentialMaterialRequest, CredentialMaterialResolver,
        ResolvedCredentialMaterial,
    };
    use awaken_provisioning_contract::{SandboxError, SecretBroker, SecretWritebackTarget};

    use super::PinnedCredentialMaterializer;

    #[derive(Default)]
    struct LocalAuthorityProbe {
        reads: AtomicUsize,
        writes: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl CredentialMaterialResolver for LocalAuthorityProbe {
        async fn resolve_exact(
            &self,
            _request: CredentialMaterialRequest<'_>,
        ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError> {
            Err(CredentialMaterialError::Unavailable)
        }
    }

    #[async_trait::async_trait]
    impl SecretBroker for LocalAuthorityProbe {
        async fn materialize(&self, reference: &str) -> Result<Vec<u8>, SandboxError> {
            assert_eq!(reference, "vault-source@7");
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(b"sealed-material".to_vec())
        }

        async fn materialize_process(&self, _reference: &str) -> Result<Vec<u8>, SandboxError> {
            Err(SandboxError::unsupported("not a process capability"))
        }

        async fn write_back(
            &self,
            target: SecretWritebackTarget<'_>,
            bytes: Vec<u8>,
        ) -> Result<(), SandboxError> {
            assert!(matches!(
                target,
                SecretWritebackTarget::Unattached {
                    reference: "vault-source@7"
                }
            ));
            assert_eq!(bytes, b"replacement");
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn local_source_has_one_injected_broker_and_absence_never_falls_back() {
        // Causes: C1 one local authority installed, C2 a targetless read or
        // write-back for its exact source, C3 no local authority installed.
        // Effects: E1 C1+C2 delegates once with original reference/bytes;
        // E2 C3 rejects before any probe effect. Rules R1=read+C1=>E1;
        // R2=write+C1=>E1; R3=C3=>E2. The neutral materializer cannot open a
        // second Vault path or choose another source on failure.
        let authority = Arc::new(LocalAuthorityProbe::default());
        let pinned =
            PinnedCredentialMaterializer::new(authority.clone(), awaken_time::system_clock());
        assert_eq!(
            pinned.materialize("vault-source@7").await.unwrap(),
            b"sealed-material",
            "R1/E1"
        );
        pinned
            .write_back(
                SecretWritebackTarget::Unattached {
                    reference: "vault-source@7",
                },
                b"replacement".to_vec(),
            )
            .await
            .unwrap();
        assert_eq!(authority.reads.load(Ordering::SeqCst), 1, "R1/E1 once");
        assert_eq!(authority.writes.load(Ordering::SeqCst), 1, "R2/E1 once");
        let external = PinnedCredentialMaterializer::external_only(
            authority.clone(),
            awaken_time::system_clock(),
        );
        assert!(
            external.materialize("vault-source@7").await.is_err(),
            "R3/E2"
        );
        assert_eq!(
            authority.reads.load(Ordering::SeqCst),
            1,
            "R3/E2 zero fallback"
        );
    }
}
