//! Claim-fenced one-shot secret delivery and authority write-back adapter.

use super::*;

#[cfg(feature = "authority")]
fn split_exact_vault_reference(reference: &str) -> Result<(&str, Option<u64>), String> {
    if reference.is_empty() {
        return Err("credential reference is empty".into());
    }
    let Some((source_id, revision)) = reference.rsplit_once('@') else {
        return Ok((reference, None));
    };
    if revision.is_empty() || !revision.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok((reference, None));
    }
    let revision = revision
        .parse::<u64>()
        .map_err(|_| "credential reference revision is invalid".to_string())?;
    if source_id.is_empty() || revision == 0 {
        return Err("credential reference must name a source and positive revision".into());
    }
    Ok((source_id, Some(revision)))
}

#[cfg(feature = "authority")]
fn verify_exact_vault_revision(
    source: &awaken_credential_vault::CredentialSource,
    revision: Option<u64>,
) -> Result<(), String> {
    let Some(expected) = revision else {
        return Ok(());
    };
    let actual = u64::try_from(source.version)
        .map_err(|_| "credential material revision is invalid".to_string())?;
    if actual != expected {
        return Err("credential material revision mismatch".into());
    }
    Ok(())
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
                    awaken_provisioning_contract::SandboxError::new(
                        "credential-artifact registry lock is poisoned",
                    )
                })?
                .remove(reference)
                .ok_or_else(|| {
                    awaken_provisioning_contract::SandboxError::new(
                        "credential artifact is missing, expired, or already consumed",
                    )
                })?;
            if pending.expires_at_unix_ms <= unix_time_ms() {
                return Err(awaken_provisioning_contract::SandboxError::new(
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
                .map_err(awaken_provisioning_contract::SandboxError::new)?;
            return crate::credential_artifact::encode(pending.codec, material)
                .map(|artifact| artifact.bytes)
                .map_err(awaken_provisioning_contract::SandboxError::new);
        }
        #[cfg(not(feature = "authority"))]
        return Err(awaken_provisioning_contract::SandboxError::new(
            "database-less Worker cannot materialize a local Vault reference",
        ));
        #[cfg(feature = "authority")]
        {
            let (source_id, revision) = split_exact_vault_reference(reference)
                .map_err(awaken_provisioning_contract::SandboxError::new)?;
            let source = self.load_active_source(source_id).await.map_err(|error| {
                awaken_provisioning_contract::SandboxError::new(error.to_string())
            })?;
            verify_exact_vault_revision(&source, revision)
                .map_err(awaken_provisioning_contract::SandboxError::new)?;
            self.materialize_source(&source)
                .await
                .map(|secret| secret.expose_secret().as_bytes().to_vec())
                .map_err(|error| awaken_provisioning_contract::SandboxError::new(error.to_string()))
        }
    }

    async fn materialize_process(
        &self,
        reference: &str,
    ) -> Result<Vec<u8>, awaken_provisioning_contract::SandboxError> {
        if !reference.starts_with(PROCESS_SECRET_REFERENCE_PREFIX) {
            return Err(awaken_provisioning_contract::SandboxError::new(
                "process-secret reference is not a claim-fenced capability",
            ));
        }
        let pending = self
            .pending_process_secrets
            .lock()
            .map_err(|_| {
                awaken_provisioning_contract::SandboxError::new(
                    "process-secret registry lock is poisoned",
                )
            })?
            .remove(reference)
            .ok_or_else(|| {
                awaken_provisioning_contract::SandboxError::new(
                    "process-secret reference is missing, expired, or already consumed",
                )
            })?;
        if pending.expires_at_unix_ms <= unix_time_ms() {
            return Err(awaken_provisioning_contract::SandboxError::new(
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
        .map_err(awaken_provisioning_contract::SandboxError::new)
    }

    async fn write_back(
        &self,
        reference: &str,
        bytes: Vec<u8>,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        #[cfg(not(feature = "authority"))]
        {
            let _ = (reference, bytes);
            return Err(awaken_provisioning_contract::SandboxError::new(
                "database-less Worker cannot write a local Vault reference",
            ));
        }
        #[cfg(feature = "authority")]
        {
            let (source_id, revision) = split_exact_vault_reference(reference)
                .map_err(awaken_provisioning_contract::SandboxError::new)?;
            let source = self.load_active_source(source_id).await.map_err(|error| {
                awaken_provisioning_contract::SandboxError::new(error.to_string())
            })?;
            verify_exact_vault_revision(&source, revision)
                .map_err(awaken_provisioning_contract::SandboxError::new)?;
            source.material_ref.as_ref().ok_or_else(|| {
                awaken_provisioning_contract::SandboxError::new(format!(
                    "credential {} has no material",
                    source.id.0
                ))
            })?;
            let material = String::from_utf8(bytes).map_err(|_| {
                awaken_provisioning_contract::SandboxError::new(
                    "credential write-back is not valid UTF-8",
                )
            })?;
            let (credentials, secrets) = self.local_stores().ok_or_else(|| {
                awaken_provisioning_contract::SandboxError::new(
                    "credential write-back requires a local credential authority",
                )
            })?;
            awaken_credential_vault::repo::rotate_credential(
                &source.id,
                RedactedString::new(material),
                secrets.as_ref(),
                credentials.as_ref(),
            )
            .await
            .map(|_| ())
            .map_err(|error| awaken_provisioning_contract::SandboxError::new(error.to_string()))
        }
    }
}
