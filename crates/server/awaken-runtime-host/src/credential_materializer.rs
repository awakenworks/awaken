//! Exact credential realization for publication-pinned runtime access.
//!
//! This host adapter owns no selection policy and cannot enumerate credentials. It
//! accepts one immutable [`ResolvedModelCandidate`], verifies its
//! Workspace/revision/usage pins against the persisted row, then materializes that exact secret. Native
//! provider execution and ACP provisioning share this adapter so they cannot drift.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use awaken_agent_contract::RedactedString;
use awaken_credential_vault::repo::CredentialRepo;
use awaken_credential_vault::{
    CredentialSource, CredentialSourceId, CredentialStatus, SecretRef, SecretStore,
};
use awaken_runtime_contract::resolved::{ModelProvisioning, ResolvedModelCandidate};
use awaken_runtime_contract::{
    AttemptCredentialBinding, CredentialAccess, CredentialAdmissionError,
    CredentialMaterialBinding, CredentialMaterialError, CredentialMaterialRequest,
    CredentialMaterialResolver, CredentialMaterialSource, CredentialRealizationCapabilities,
    CredentialRealizationKind, CredentialUsage, PlaintextHolder, ResolvedCredentialMaterial,
};

const PROCESS_SECRET_REFERENCE_PREFIX: &str = "awaken-process-secret://";
const CREDENTIAL_ARTIFACT_REFERENCE_PREFIX: &str = "awaken-credential-artifact://";
const PROCESS_SECRET_TTL_MS: u64 = 60_000;

#[derive(Clone)]
struct PendingProcessSecret {
    candidate: ResolvedModelCandidate,
    context: awaken_runtime_contract::RuntimeRunContext,
    expires_at_unix_ms: u64,
}

#[derive(Clone)]
struct PendingCredentialArtifact {
    candidate: ResolvedModelCandidate,
    context: awaken_runtime_contract::RuntimeRunContext,
    codec: awaken_run_executor_acp::CredentialArtifactCodec,
    expires_at_unix_ms: u64,
}

/// Worker/host-side realization of one already-published credential reference.
#[derive(Clone)]
pub struct PinnedCredentialMaterializer {
    credentials: Arc<dyn CredentialRepo>,
    secrets: Arc<dyn SecretStore>,
    external_material_resolver: Option<Arc<dyn CredentialMaterialResolver>>,
    pending_process_secrets: Arc<Mutex<HashMap<String, PendingProcessSecret>>>,
    pending_credential_artifacts: Arc<Mutex<HashMap<String, PendingCredentialArtifact>>>,
}

impl PinnedCredentialMaterializer {
    #[must_use]
    pub fn new(credentials: Arc<dyn CredentialRepo>, secrets: Arc<dyn SecretStore>) -> Self {
        Self {
            credentials,
            secrets,
            external_material_resolver: None,
            pending_process_secrets: Arc::new(Mutex::new(HashMap::new())),
            pending_credential_artifacts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Install the sole non-local material-source adapter. It handles exact
    /// Worker references and recipient-bound envelopes through the same neutral
    /// resolver contract; absence fails closed and never falls back to Vault.
    #[must_use]
    pub fn with_external_material_resolver(
        mut self,
        resolver: Arc<dyn CredentialMaterialResolver>,
    ) -> Self {
        self.external_material_resolver = Some(resolver);
        self
    }

    /// Installed source evidence used by standard Worker manifest derivation.
    #[must_use]
    pub fn material_source_capabilities(
        &self,
    ) -> (std::collections::BTreeSet<CredentialMaterialSource>, bool) {
        let mut sources =
            std::collections::BTreeSet::from([CredentialMaterialSource::ControlPlaneReference]);
        let envelopes = self
            .external_material_resolver
            .as_ref()
            .is_some_and(|resolver| {
                sources.extend(resolver.supported_material_sources());
                resolver.supports_recipient_bound_envelopes()
            });
        (sources, envelopes)
    }

    fn claimed_provider_binding<'a>(
        candidate: &'a ResolvedModelCandidate,
        context: &'a awaken_runtime_contract::RuntimeRunContext,
    ) -> Result<Option<(&'a CredentialAccess, &'a AttemptCredentialBinding)>, String> {
        let ModelProvisioning::Provider { credential, .. } = &candidate.provisioning else {
            return Err("model candidate has no provider provisioning".into());
        };
        let Some(access) = credential else {
            return Ok(None);
        };
        let realization = context
            .credential_realization
            .as_ref()
            .ok_or_else(|| "credential-bearing provider has no claim binding".to_string())?;
        let binding = realization
            .binding_for(candidate)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "provider candidate has no exact claim binding".to_string())?;
        if binding.credential != access.credential {
            return Err("provider claim binding selects a different credential".into());
        }
        Ok(Some((access, binding)))
    }

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
        let now = unix_time_ms();
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
        delivery: awaken_run_executor_acp::ManagedCredentialDelivery,
    ) -> Result<Option<awaken_run_executor_acp::CredentialArtifactRequirement>, String> {
        let Some((access, binding)) = Self::claimed_provider_binding(candidate, context)? else {
            return Ok(None);
        };
        if binding.selected_realization_kind != CredentialRealizationKind::WorkerProviderAdapter {
            return Ok(None);
        }
        let Some(artifact) = delivery.credential_artifact(access.refresh.is_some()) else {
            return Ok(None);
        };
        let now = unix_time_ms();
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
            awaken_run_executor_acp::CredentialArtifactRequirement::new(
                reference,
                artifact.relative_path,
            ),
        ))
    }

    /// Materialize the provider credential selected by the current durable claim.
    ///
    /// This is the sole inference/ACP realization path: exact binding, mechanism,
    /// ownership, secret access, and durable receipt are one ordered fail-closed
    /// operation. Callers receive no plaintext unless every cause succeeds.
    pub async fn materialize_claimed_provider(
        &self,
        candidate: &ResolvedModelCandidate,
        context: &awaken_runtime_contract::RuntimeRunContext,
        expected_kind: CredentialRealizationKind,
    ) -> Result<Option<RedactedString>, String> {
        self.materialize_claimed_provider_material(candidate, context, expected_kind)
            .await?
            .map(|material| material.into_bearer().map_err(|error| error.to_string()))
            .transpose()
    }

    async fn materialize_claimed_provider_material(
        &self,
        candidate: &ResolvedModelCandidate,
        context: &awaken_runtime_contract::RuntimeRunContext,
        expected_kind: CredentialRealizationKind,
    ) -> Result<Option<awaken_runtime_contract::CredentialMaterial>, String> {
        let Some((_access, binding)) = Self::claimed_provider_binding(candidate, context)? else {
            return Ok(None);
        };
        if binding.selected_realization_kind != expected_kind {
            return Err(format!(
                "provider claim binding selects {:?}, expected {:?}",
                binding.selected_realization_kind, expected_kind
            ));
        }
        let realization = context
            .credential_realization
            .as_ref()
            .expect("claimed_provider_binding requires realization");
        context
            .ownership
            .as_ref()
            .ok_or_else(|| "credential-bearing provider has no claim fence".to_string())?
            .verify_current()
            .await
            .map_err(|error| error.to_string())?;
        let secret = self
            .materialize_provider_for(
                candidate,
                &binding.selected_plaintext_holder,
                binding.selected_realization_kind,
            )
            .await?;
        realization
            .record(binding)
            .await
            .map_err(|error| error.to_string())?;
        Ok(Some(secret))
    }

    /// Materialize exactly the provider credential frozen in `access`.
    ///
    /// Catalog lookup, default selection and failover are intentionally absent.
    /// Every mismatch is terminal for this pin; the caller may only try another
    /// complete candidate that was already included in the published snapshot.
    #[cfg(test)]
    async fn materialize_provider(
        &self,
        candidate: &ResolvedModelCandidate,
    ) -> Result<RedactedString, String> {
        self.materialize_provider_for(
            candidate,
            &PlaintextHolder::new(
                awaken_runtime_contract::PlaintextBoundary::Worker,
                awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
            ),
            CredentialRealizationKind::WorkerProviderAdapter,
        )
        .await?
        .into_bearer()
        .map_err(|error| error.to_string())
    }

    /// Materialize for the exact holder and mechanism selected by admission.
    async fn materialize_provider_for(
        &self,
        candidate: &ResolvedModelCandidate,
        selected_holder: &PlaintextHolder,
        realization: CredentialRealizationKind,
    ) -> Result<awaken_runtime_contract::CredentialMaterial, String> {
        let ModelProvisioning::Provider {
            provider_ref,
            scope_id,
            credential,
            endpoint,
            ..
        } = &candidate.provisioning
        else {
            return Err("model candidate does not require a provider credential".into());
        };
        let provider = provider_ref
            .split_once('@')
            .map(|(provider, _)| provider)
            .ok_or_else(|| "published model candidate has no versioned provider pin".to_string())?;
        let credential = credential
            .as_ref()
            .ok_or_else(|| "published model candidate has no credential pin".to_string())?;
        if !matches!(
            credential.usage,
            CredentialUsage::ProviderAdapter | CredentialUsage::EnvironmentVariable { .. }
        ) {
            return Err("published credential injection contract is invalid".to_string());
        }
        let (material_sources, recipient_bound_envelopes) = self.material_source_capabilities();
        admit_exact_adapter(
            credential,
            selected_holder,
            realization,
            material_sources,
            recipient_bound_envelopes,
        )
        .map_err(|error| error.to_string())?;
        let resolved = self
            .resolve_validated(
                credential,
                selected_holder,
                CredentialMaterialBinding::for_target(
                    scope_id.as_str(),
                    &(provider_ref, endpoint),
                    &credential.usage,
                ),
                Some(provider),
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(resolved.material)
    }

    async fn resolve_validated(
        &self,
        access: &CredentialAccess,
        selected_holder: &PlaintextHolder,
        binding: CredentialMaterialBinding,
        expected_provider: Option<&str>,
    ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError> {
        binding.validate()?;
        if !access
            .policy
            .allowed_plaintext_holders
            .contains(selected_holder)
        {
            return Err(CredentialMaterialError::RecipientMismatch);
        }
        if let Some(envelope) = &access.envelope {
            envelope.validate_for_holder(selected_holder, unix_time_ms())?;
        }
        if access.envelope.is_some()
            || access.material_source == CredentialMaterialSource::WorkerReference
        {
            let resolver = self
                .external_material_resolver
                .as_ref()
                .ok_or(CredentialMaterialError::Unavailable)?;
            if !resolver
                .supported_material_sources()
                .contains(&access.material_source)
                || (access.envelope.is_some() && !resolver.supports_recipient_bound_envelopes())
            {
                return Err(CredentialMaterialError::Unavailable);
            }
            let resolved = resolver
                .resolve_exact(CredentialMaterialRequest {
                    access,
                    selected_holder,
                    binding: &binding,
                })
                .await?;
            if resolved.credential != access.credential || resolved.holder != *selected_holder {
                return Err(CredentialMaterialError::ResolverMismatch);
            }
            return Ok(resolved);
        }
        if access.material_source != CredentialMaterialSource::ControlPlaneReference {
            return Err(CredentialMaterialError::Unavailable);
        }
        let source = self.load_active_source(&access.credential.id).await?;
        let revision =
            u64::try_from(source.version).map_err(|_| CredentialMaterialError::RevisionMismatch)?;
        if revision != access.credential.revision {
            return Err(CredentialMaterialError::RevisionMismatch);
        }
        if source.workspace_id != binding.workspace_id {
            return Err(CredentialMaterialError::RecipientMismatch);
        }
        if expected_provider.is_some_and(|provider| {
            source
                .provider_id
                .as_deref()
                .is_some_and(|configured| configured != provider)
        }) {
            return Err(CredentialMaterialError::RecipientMismatch);
        }
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
                .map_err(|_| CredentialMaterialError::Unavailable)?;
            awaken_runtime_contract::CredentialMaterial::OAuth(
                awaken_runtime_contract::OAuthCredentialMaterial {
                    access_token,
                    refresh_token,
                    expires_at_unix_ms: refresh.expires_at_unix_ms,
                    account_id: refresh.account_id.clone(),
                    account_plan: refresh.account_plan.clone(),
                },
            )
        } else {
            awaken_runtime_contract::CredentialMaterial::bearer(access_token)
        };
        Ok(ResolvedCredentialMaterial {
            credential: access.credential.clone(),
            holder: selected_holder.clone(),
            material,
        })
    }

    pub(crate) async fn resolve_for_workspace(
        &self,
        access: &CredentialAccess,
        selected_holder: &PlaintextHolder,
        realization: CredentialRealizationKind,
        workspace: &str,
        target: &impl serde::Serialize,
    ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError> {
        let (material_sources, recipient_bound_envelopes) = self.material_source_capabilities();
        admit_exact_adapter(
            access,
            selected_holder,
            realization,
            material_sources,
            recipient_bound_envelopes,
        )
        .map_err(|_| CredentialMaterialError::Unavailable)?;
        self.resolve_validated(
            access,
            selected_holder,
            CredentialMaterialBinding::for_target(workspace, target, &access.usage),
            None,
        )
        .await
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
        if source.status != CredentialStatus::Active {
            return Err(CredentialMaterialError::Unavailable);
        }
        Ok(source)
    }

    async fn materialize_source(
        &self,
        source: &CredentialSource,
    ) -> Result<RedactedString, CredentialMaterialError> {
        awaken_credential_vault::materialize(source, self.secrets.as_ref())
            .await
            .map_err(|_| CredentialMaterialError::Unavailable)
    }

    pub(crate) fn secret_store(&self) -> Arc<dyn SecretStore> {
        self.secrets.clone()
    }
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

/// Validate one explicit adapter mechanism without introducing a capability
/// search. The caller is the installed adapter; this function only prevents its
/// Native, ACP, MCP, and Resource entry points from drifting in admission rules.
fn admit_exact_adapter(
    access: &CredentialAccess,
    selected_holder: &PlaintextHolder,
    realization: CredentialRealizationKind,
    material_sources: std::collections::BTreeSet<CredentialMaterialSource>,
    recipient_bound_envelopes: bool,
) -> Result<(), CredentialAdmissionError> {
    access.admit(
        selected_holder,
        realization,
        &CredentialRealizationCapabilities {
            holders: [selected_holder.clone()].into_iter().collect(),
            material_sources,
            realization_kinds: [realization].into_iter().collect(),
            recipient_bound_envelopes,
            alternatives: Vec::new(),
        },
        unix_time_ms(),
    )?;
    Ok(())
}

#[async_trait::async_trait]
impl CredentialMaterialResolver for PinnedCredentialMaterializer {
    fn supported_material_sources(&self) -> std::collections::BTreeSet<CredentialMaterialSource> {
        self.material_source_capabilities().0
    }

    fn supports_recipient_bound_envelopes(&self) -> bool {
        self.material_source_capabilities().1
    }

    async fn resolve_exact(
        &self,
        request: CredentialMaterialRequest<'_>,
    ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError> {
        self.resolve_validated(
            request.access,
            request.selected_holder,
            request.binding.clone(),
            None,
        )
        .await
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
                    CredentialRealizationKind::WorkerProviderAdapter,
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
        let source = self
            .load_active_source(reference)
            .await
            .map_err(|error| awaken_provisioning_contract::SandboxError::new(error.to_string()))?;
        self.materialize_source(&source)
            .await
            .map(|secret| secret.expose_secret().as_bytes().to_vec())
            .map_err(|error| awaken_provisioning_contract::SandboxError::new(error.to_string()))
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
        let source = self
            .load_active_source(reference)
            .await
            .map_err(|error| awaken_provisioning_contract::SandboxError::new(error.to_string()))?;
        let material_ref = source.material_ref.as_ref().ok_or_else(|| {
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
        self.secrets
            .put(material_ref, RedactedString::new(material))
            .await
            .map_err(|error| awaken_provisioning_contract::SandboxError::new(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_credential_vault::repo::{CredentialRepo, InMemoryCredentialRepo, enter_credential};
    use awaken_credential_vault::{CredentialCreateParams, CredentialKind, InMemorySecretStore};
    use awaken_provisioning_contract::SecretBroker;
    use awaken_runtime_contract::{
        CredentialEnvelope, CredentialExecutionPolicy, CredentialMaterial, CredentialRef,
        CredentialRefreshAccess, CredentialUsage, InferenceEndpoint, ModelBinding,
        ModelExposurePolicy, PlaintextBoundary, SealedCredentialEnvelopeRef, TokenEndpointAuth,
    };

    #[derive(Clone)]
    struct ExactExternalResolver {
        expected_binding: CredentialMaterialBinding,
        expected_payload_fingerprint: &'static str,
        returned_holder: PlaintextHolder,
    }

    #[async_trait::async_trait]
    impl CredentialMaterialResolver for ExactExternalResolver {
        fn supported_material_sources(
            &self,
        ) -> std::collections::BTreeSet<CredentialMaterialSource> {
            std::collections::BTreeSet::from([
                CredentialMaterialSource::ControlPlaneReference,
                CredentialMaterialSource::WorkerReference,
            ])
        }

        fn supports_recipient_bound_envelopes(&self) -> bool {
            true
        }

        async fn resolve_exact(
            &self,
            request: CredentialMaterialRequest<'_>,
        ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError> {
            request.binding.validate()?;
            if request.binding != &self.expected_binding {
                return Err(CredentialMaterialError::BindingMismatch);
            }
            if let Some(envelope) = &request.access.envelope {
                let payload_fingerprint = match envelope {
                    CredentialEnvelope::SealedForWorker { envelope_ref, .. }
                    | CredentialEnvelope::SealedForWorkload { envelope_ref, .. } => {
                        &envelope_ref.payload_fingerprint
                    }
                };
                if payload_fingerprint != self.expected_payload_fingerprint {
                    return Err(CredentialMaterialError::PayloadMismatch);
                }
            }
            Ok(ResolvedCredentialMaterial {
                credential: request.access.credential.clone(),
                holder: self.returned_holder.clone(),
                material: awaken_runtime_contract::CredentialMaterial::bearer(RedactedString::new(
                    "external-sealed-material",
                )),
            })
        }
    }

    #[derive(Clone)]
    struct ResolutionRule {
        id: &'static str,
        control_source: bool,
        no_envelope: bool,
        holder_allowed: bool,
        source_exists: bool,
        source_active: bool,
        revision_exact: bool,
        workspace_exact: bool,
        provider_exact: bool,
        material_available: bool,
        expected: Result<(), CredentialMaterialError>,
    }

    fn selected_holder() -> PlaintextHolder {
        PlaintextHolder::new(
            PlaintextBoundary::Worker,
            awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
        )
    }

    /// Cause-effect graph:
    ///
    /// C0 control-plane material source -> C1 no sealed envelope
    ///  -> C2 selected holder authorized -> C3 source exists -> C4 source active
    ///  -> C5 exact revision -> C6 exact Workspace -> C7 exact provider
    ///  -> C8 material opens -> E1 exact material returned.
    ///
    /// A failed cause terminates resolution with its stable, secret-free E2
    /// error. The table uses `T` for a satisfied cause, `F` for the one failed
    /// cause, and `-` for causes that are not evaluated.
    ///
    /// | Rule | C0 | C1 | C2 | C3 | C4 | C5 | C6 | C7 | C8 | Result |
    /// |---|---|---|---|---|---|---|---|---|---|---|
    /// | R1 | T | T | T | T | T | T | T | T | T | material |
    /// | R2 | F | - | - | - | - | - | - | - | - | unavailable |
    /// | R3 | T | F | - | - | - | - | - | - | - | unavailable |
    /// | R4 | T | T | F | - | - | - | - | - | - | recipient mismatch |
    /// | R5 | T | T | T | F | - | - | - | - | - | unavailable |
    /// | R6 | T | T | T | T | F | - | - | - | - | unavailable |
    /// | R7 | T | T | T | T | T | F | - | - | - | revision mismatch |
    /// | R8 | T | T | T | T | T | T | F | - | - | recipient mismatch |
    /// | R9 | T | T | T | T | T | T | T | F | - | recipient mismatch |
    /// | R10 | T | T | T | T | T | T | T | T | F | unavailable |
    #[tokio::test]
    async fn exact_resolution_tests_are_generated_from_the_decision_table() {
        let valid = ResolutionRule {
            id: "R1",
            control_source: true,
            no_envelope: true,
            holder_allowed: true,
            source_exists: true,
            source_active: true,
            revision_exact: true,
            workspace_exact: true,
            provider_exact: true,
            material_available: true,
            expected: Ok(()),
        };
        let rules = [
            valid.clone(),
            ResolutionRule {
                id: "R2",
                control_source: false,
                expected: Err(CredentialMaterialError::Unavailable),
                ..valid.clone()
            },
            ResolutionRule {
                id: "R3",
                no_envelope: false,
                expected: Err(CredentialMaterialError::Unavailable),
                ..valid.clone()
            },
            ResolutionRule {
                id: "R4",
                holder_allowed: false,
                expected: Err(CredentialMaterialError::RecipientMismatch),
                ..valid.clone()
            },
            ResolutionRule {
                id: "R5",
                source_exists: false,
                expected: Err(CredentialMaterialError::Unavailable),
                ..valid.clone()
            },
            ResolutionRule {
                id: "R6",
                source_active: false,
                expected: Err(CredentialMaterialError::Unavailable),
                ..valid.clone()
            },
            ResolutionRule {
                id: "R7",
                revision_exact: false,
                expected: Err(CredentialMaterialError::RevisionMismatch),
                ..valid.clone()
            },
            ResolutionRule {
                id: "R8",
                workspace_exact: false,
                expected: Err(CredentialMaterialError::RecipientMismatch),
                ..valid.clone()
            },
            ResolutionRule {
                id: "R9",
                provider_exact: false,
                expected: Err(CredentialMaterialError::RecipientMismatch),
                ..valid.clone()
            },
            ResolutionRule {
                id: "R10",
                material_available: false,
                expected: Err(CredentialMaterialError::Unavailable),
                ..valid
            },
        ];

        for rule in rules {
            let credentials = Arc::new(InMemoryCredentialRepo::new());
            let secrets = Arc::new(InMemorySecretStore::new());
            let mut credential_id = "missing-credential".to_string();
            if rule.source_exists {
                let mut source = enter_credential(
                    CredentialCreateParams {
                        workspace_id: if rule.workspace_exact {
                            "workspace-a".into()
                        } else {
                            "workspace-b".into()
                        },
                        kind: CredentialKind::Vault,
                        provider_id: Some(if rule.provider_exact {
                            "anthropic".into()
                        } else {
                            "other-provider".into()
                        }),
                        env_key: None,
                        secret: Some(RedactedString::new("decision-table-secret")),
                        oauth_command: None,
                    },
                    secrets.as_ref(),
                    credentials.as_ref(),
                )
                .await
                .expect("fixture credential");
                credential_id.clone_from(&source.id.0);
                if !rule.source_active {
                    source.status = CredentialStatus::Disabled;
                }
                if !rule.material_available {
                    source.material_ref = None;
                }
                credentials.put(source).await.expect("fixture row");
            }

            let holder = selected_holder();
            let policy_holder = if rule.holder_allowed {
                holder.clone()
            } else {
                PlaintextHolder::new(PlaintextBoundary::Platform, "another-domain")
            };
            let mut access = CredentialAccess::new(
                CredentialRef {
                    id: credential_id,
                    revision: if rule.revision_exact { 1 } else { 2 },
                },
                if rule.control_source {
                    CredentialMaterialSource::ControlPlaneReference
                } else {
                    CredentialMaterialSource::WorkerReference
                },
                CredentialUsage::ProviderAdapter,
                CredentialExecutionPolicy::exact(policy_holder, ModelExposurePolicy::Forbidden),
            );
            if !rule.no_envelope {
                access = access.with_envelope(CredentialEnvelope::SealedForWorker {
                    envelope_ref: SealedCredentialEnvelopeRef {
                        id: "sealed-1".into(),
                        payload_fingerprint: "fp".into(),
                    },
                    recipient: holder.trust_domain.clone(),
                    expires_at_unix_ms: u64::MAX,
                });
            }
            let resolver = PinnedCredentialMaterializer::new(credentials, secrets);
            let result = resolver
                .resolve_validated(
                    &access,
                    &holder,
                    CredentialMaterialBinding::for_target(
                        "workspace-a",
                        &"anthropic",
                        &access.usage,
                    ),
                    Some("anthropic"),
                )
                .await;
            assert!(
                !format!("{result:?}").contains("decision-table-secret"),
                "{} keeps errors and debug output secret-free",
                rule.id
            );
            match (rule.expected, result) {
                (Ok(()), Ok(material)) => {
                    assert_eq!(material.credential, access.credential, "{}", rule.id);
                    assert_eq!(material.holder, holder, "{}", rule.id);
                    assert_eq!(
                        material.material.access_token().expose_secret(),
                        "decision-table-secret"
                    );
                }
                (Err(expected), Err(actual)) => assert_eq!(actual, expected, "{}", rule.id),
                (expected, actual) => panic!(
                    "{}: expected {expected:?}, got {:?}",
                    rule.id,
                    actual.map(|_| ())
                ),
            }
        }
    }

    #[tokio::test]
    async fn exact_control_reference_resolves_one_oauth_bundle_for_the_provider_driver() {
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let secrets = Arc::new(InMemorySecretStore::new());
        let source = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("openai".into()),
                env_key: None,
                secret: Some(RedactedString::new("access-token")),
                oauth_command: None,
            },
            secrets.as_ref(),
            credentials.as_ref(),
        )
        .await
        .expect("access-token credential");
        let access_ref = source
            .material_ref
            .as_ref()
            .expect("vault reference")
            .0
            .clone();
        let refresh_ref = SecretRef("oauth-refresh-token".into());
        secrets
            .put(&refresh_ref, RedactedString::new("refresh-token"))
            .await
            .expect("refresh-token material");

        let holder = selected_holder();
        let access = CredentialAccess::new(
            CredentialRef {
                id: source.id.0,
                revision: u64::try_from(source.version).expect("positive revision"),
            },
            CredentialMaterialSource::ControlPlaneReference,
            CredentialUsage::ProviderAdapter,
            CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::Forbidden),
        )
        .with_refresh(
            CredentialRefreshAccess::new(
                u64::try_from(source.version).expect("positive revision"),
                "https://auth.openai.com/oauth/token".into(),
                "client-id".into(),
                TokenEndpointAuth::None,
                None,
                refresh_ref.0,
                access_ref,
                Some("openid profile offline_access".into()),
                None,
            )
            .with_provider_metadata(
                Some("account-1".into()),
                Some("pro".into()),
                Some(u64::MAX),
            ),
        );

        let resolved = PinnedCredentialMaterializer::new(credentials, secrets)
            .resolve_validated(
                &access,
                &holder,
                CredentialMaterialBinding::for_target("workspace-a", &"openai", &access.usage),
                Some("openai"),
            )
            .await
            .expect("exact OAuth bundle");

        let CredentialMaterial::OAuth(bundle) = resolved.material else {
            panic!("OAuth access must not degrade to a bearer-only material")
        };
        assert_eq!(bundle.access_token.expose_secret(), "access-token");
        assert_eq!(bundle.refresh_token.expose_secret(), "refresh-token");
        assert_eq!(bundle.account_id.as_deref(), Some("account-1"));
        assert_eq!(bundle.account_plan.as_deref(), Some("pro"));
        assert_eq!(bundle.expires_at_unix_ms, Some(u64::MAX));
    }

    /// Cause-effect graph for the one external material-source path:
    ///
    /// C1 external resolver installed -> C2 envelope recipient/live
    /// -> C3 exact target/use binding -> C4 exact payload fingerprint
    /// -> C5 resolver returns the selected credential/holder -> E1 material.
    /// A WorkerReference without an envelope uses the same C1/C3/C5 path. No
    /// failed external cause falls back to the local Vault row.
    ///
    /// | Rule | C1 | C2 | C3 | C4 | C5 | Source | Result |
    /// |---|---|---|---|---|---|---|---|
    /// | S1 | T | T | T | T | T | envelope | material |
    /// | S2 | F | - | - | - | - | envelope | unavailable |
    /// | S3 | T | T | F | - | - | envelope | binding mismatch |
    /// | S4 | T | T | T | F | - | envelope | payload mismatch |
    /// | S5 | T | T | T | T | F | envelope | resolver mismatch |
    /// | S6 | T | F(expired) | - | - | - | envelope | expired |
    /// | S7 | T | F(recipient) | - | - | - | envelope | recipient mismatch |
    /// | S8 | T | - | T | - | T | worker reference | material |
    /// | S9 | T | T | T | T | T | inference envelope | material |
    #[tokio::test]
    async fn external_resolution_tests_are_generated_from_the_decision_table() {
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let secrets = Arc::new(InMemorySecretStore::new());
        let source = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: None,
                env_key: None,
                secret: Some(RedactedString::new("must-not-fallback-to-local")),
                oauth_command: None,
            },
            secrets.as_ref(),
            credentials.as_ref(),
        )
        .await
        .expect("local fallback trap");
        let holder = selected_holder();
        let binding = CredentialMaterialBinding::for_target(
            "workspace-a",
            &"https://service.example/mcp",
            &CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            },
        );
        let access = |recipient: &str, expiry: u64, fingerprint: &str| {
            CredentialAccess::new(
                CredentialRef {
                    id: source.id.0.clone(),
                    revision: 1,
                },
                CredentialMaterialSource::ControlPlaneReference,
                CredentialUsage::HttpHeader {
                    name: "authorization".into(),
                    scheme: Some("Bearer".into()),
                },
                CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::Forbidden),
            )
            .with_envelope(CredentialEnvelope::SealedForWorker {
                envelope_ref: SealedCredentialEnvelopeRef {
                    id: "sealed-1".into(),
                    payload_fingerprint: fingerprint.into(),
                },
                recipient: awaken_runtime_contract::TrustDomainRef(recipient.into()),
                expires_at_unix_ms: expiry,
            })
        };
        let resolver = |expected_binding: CredentialMaterialBinding,
                        expected_payload_fingerprint: &'static str,
                        returned_holder: PlaintextHolder| {
            Arc::new(ExactExternalResolver {
                expected_binding,
                expected_payload_fingerprint,
                returned_holder,
            }) as Arc<dyn CredentialMaterialResolver>
        };
        let base = PinnedCredentialMaterializer::new(credentials, secrets);

        let exact_access = access(&holder.trust_domain.0, u64::MAX, "sha256:sealed-payload");
        let exact = base.clone().with_external_material_resolver(resolver(
            binding.clone(),
            "sha256:sealed-payload",
            holder.clone(),
        ));
        let resolved = exact
            .resolve_validated(&exact_access, &holder, binding.clone(), None)
            .await
            .expect("S1 exact envelope");
        assert_eq!(
            resolved.material.access_token().expose_secret(),
            "external-sealed-material"
        );

        assert_eq!(
            base.resolve_validated(&exact_access, &holder, binding.clone(), None)
                .await
                .unwrap_err(),
            CredentialMaterialError::Unavailable,
            "S2"
        );
        let other_binding = CredentialMaterialBinding::for_target(
            "workspace-a",
            &"https://other.example/mcp",
            &exact_access.usage,
        );
        assert_eq!(
            exact
                .resolve_validated(&exact_access, &holder, other_binding, None)
                .await
                .unwrap_err(),
            CredentialMaterialError::BindingMismatch,
            "S3"
        );
        let bad_payload = access(&holder.trust_domain.0, u64::MAX, "sha256:tampered");
        assert_eq!(
            exact
                .resolve_validated(&bad_payload, &holder, binding.clone(), None)
                .await
                .unwrap_err(),
            CredentialMaterialError::PayloadMismatch,
            "S4"
        );
        let wrong_return = base.clone().with_external_material_resolver(resolver(
            binding.clone(),
            "sha256:sealed-payload",
            PlaintextHolder::new(PlaintextBoundary::Worker, "another-worker"),
        ));
        assert_eq!(
            wrong_return
                .resolve_validated(&exact_access, &holder, binding.clone(), None)
                .await
                .unwrap_err(),
            CredentialMaterialError::ResolverMismatch,
            "S5"
        );
        let expired = access(&holder.trust_domain.0, 0, "sha256:sealed-payload");
        assert_eq!(
            exact
                .resolve_validated(&expired, &holder, binding.clone(), None)
                .await
                .unwrap_err(),
            CredentialMaterialError::EnvelopeExpired,
            "S6"
        );
        let wrong_recipient = access("another-worker", u64::MAX, "sha256:sealed-payload");
        assert_eq!(
            exact
                .resolve_validated(&wrong_recipient, &holder, binding.clone(), None)
                .await
                .unwrap_err(),
            CredentialMaterialError::RecipientMismatch,
            "S7"
        );
        let worker_access = CredentialAccess::new(
            exact_access.credential.clone(),
            CredentialMaterialSource::WorkerReference,
            exact_access.usage.clone(),
            exact_access.policy.clone(),
        );
        let worker_resolved = exact
            .resolve_validated(&worker_access, &holder, binding, None)
            .await
            .expect("S8 WorkerReference delegates through the same port");
        assert_eq!(
            worker_resolved.material.access_token().expose_secret(),
            "external-sealed-material"
        );

        let provider_access = CredentialAccess::new(
            exact_access.credential.clone(),
            CredentialMaterialSource::ControlPlaneReference,
            CredentialUsage::ProviderAdapter,
            exact_access.policy.clone(),
        )
        .with_envelope(CredentialEnvelope::SealedForWorker {
            envelope_ref: SealedCredentialEnvelopeRef {
                id: "sealed-provider".into(),
                payload_fingerprint: "sha256:provider-payload".into(),
            },
            recipient: holder.trust_domain.clone(),
            expires_at_unix_ms: u64::MAX,
        });
        let endpoint = InferenceEndpoint {
            adapter_kind: "anthropic".into(),
            api_dialect: "anthropic_messages".into(),
            base_url: "https://provider.example".into(),
            upstream_model: "model".into(),
        };
        let provider_ref = "anthropic@1";
        let provider_binding = CredentialMaterialBinding::for_target(
            "workspace-a",
            &(provider_ref, &endpoint),
            &provider_access.usage,
        );
        let provider_materializer = base.with_external_material_resolver(resolver(
            provider_binding,
            "sha256:provider-payload",
            holder,
        ));
        let candidate = ResolvedModelCandidate::provider(
            ModelBinding::new("anthropic", "model", "native"),
            provider_ref,
            "endpoint@1",
            "workspace-a",
            Some(provider_access),
            endpoint,
        );
        assert_eq!(
            provider_materializer
                .materialize_provider(&candidate)
                .await
                .expect("S9 inference uses the same exact resolver")
                .expose_secret(),
            "external-sealed-material",
            "S9"
        );
    }

    /// Adapter-admission cause graph: published source -> installed source
    /// capability -> material lookup. The decision table distinguishes a
    /// supported-but-missing Control reference from a Worker reference this
    /// adapter must reject before any lookup.
    ///
    /// | Rule | Control reference | Row exists | Result |
    /// |---|---|---|---|
    /// | A1 | T | F | material unavailable |
    /// | A2 | F | - | material source unsupported |
    #[tokio::test]
    async fn provider_adapter_capabilities_match_the_decision_table() {
        for (rule, source, expected) in [
            (
                "A1",
                CredentialMaterialSource::ControlPlaneReference,
                "credential material unavailable",
            ),
            (
                "A2",
                CredentialMaterialSource::WorkerReference,
                "credential material source is unsupported",
            ),
        ] {
            let candidate = ResolvedModelCandidate::provider(
                ModelBinding::new("anthropic", "model", "native"),
                "anthropic@1",
                "endpoint@1",
                "workspace-a",
                Some(CredentialAccess::new(
                    CredentialRef {
                        id: "missing".into(),
                        revision: 1,
                    },
                    source,
                    CredentialUsage::ProviderAdapter,
                    CredentialExecutionPolicy::self_hosted_provider(),
                )),
                InferenceEndpoint {
                    adapter_kind: "anthropic".into(),
                    api_dialect: "anthropic_messages".into(),
                    base_url: "https://example.invalid".into(),
                    upstream_model: "model".into(),
                },
            );
            let materializer = PinnedCredentialMaterializer::new(
                Arc::new(InMemoryCredentialRepo::new()),
                Arc::new(InMemorySecretStore::new()),
            );
            assert_eq!(
                materializer
                    .materialize_provider(&candidate)
                    .await
                    .unwrap_err(),
                expected,
                "{rule}"
            );
        }
    }

    #[tokio::test]
    async fn credential_file_broker_reuses_the_persisted_vault() {
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let secrets = Arc::new(InMemorySecretStore::new());
        let source = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("tool".into()),
                env_key: None,
                secret: Some(RedactedString::new("before")),
                oauth_command: None,
            },
            secrets.as_ref(),
            credentials.as_ref(),
        )
        .await
        .unwrap();
        let broker = PinnedCredentialMaterializer::new(credentials, secrets);

        assert_eq!(broker.materialize(&source.id.0).await.unwrap(), b"before");
        assert!(
            broker.materialize_process(&source.id.0).await.is_err(),
            "a durable file credential id is never a process capability"
        );
        broker
            .write_back(&source.id.0, b"after".to_vec())
            .await
            .unwrap();
        assert_eq!(broker.materialize(&source.id.0).await.unwrap(), b"after");
    }
}
