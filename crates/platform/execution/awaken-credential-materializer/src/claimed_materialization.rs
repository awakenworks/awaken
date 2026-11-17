// @formal entrypoint credential_effect.boundary_preserves_exact_materialization_authority
// @formal entrypoint worker.secret_materialization_is_claim_fenced
//! Claim-bound provider and remote credential materialization.
//!
//! This module owns the ordered boundary from an immutable attempt binding to
//! current ownership, plaintext access, and the corresponding durable receipt.

use awaken_agent_contract::RedactedString;
use awaken_agent_contract::credential::CredentialAccess;
use awaken_credential_contract::CredentialRealizationKind;
use awaken_credential_contract::{AttemptCredentialBinding, AttemptCredentialRealization};
use awaken_runtime_contract::AttemptOwnershipError;
use awaken_runtime_contract::execution::check_attempt_ownership;
use awaken_runtime_contract::llm::Error as LlmError;
use awaken_runtime_contract::resolved::{ModelProvisioning, ResolvedModelCandidate};

use super::{PinnedCredentialMaterializer, ServedProviderMaterial};

#[derive(Debug, thiserror::Error)]
pub(super) enum ClaimedMaterializationError {
    #[error("{0}")]
    Rejected(String),
    #[error("runtime execution failed: Run attempt no longer owns external execution: {0}")]
    Ownership(#[from] AttemptOwnershipError),
}

impl From<String> for ClaimedMaterializationError {
    fn from(detail: String) -> Self {
        Self::Rejected(detail)
    }
}

impl From<ClaimedMaterializationError> for LlmError {
    fn from(error: ClaimedMaterializationError) -> Self {
        match error {
            ClaimedMaterializationError::Ownership(AttemptOwnershipError::Unavailable(_)) => {
                Self::Admission {
                    source: Box::new(Self::Provider(error.to_string())),
                }
            }
            _ => Self::Binding(error.to_string()),
        }
    }
}

impl PinnedCredentialMaterializer {
    pub(super) fn claimed_provider_binding<'a>(
        candidate: &'a ResolvedModelCandidate,
        context: &'a awaken_runtime_contract::RuntimeRunContext,
    ) -> Result<Option<(&'a CredentialAccess, &'a AttemptCredentialBinding)>, String> {
        let ModelProvisioning::Provider { credential, .. } = candidate.provisioning() else {
            return Err("model candidate has no provider provisioning".into());
        };
        Self::claimed_binding(candidate, context, credential.as_deref(), "provider")
    }

    fn claimed_remote_binding<'a>(
        candidate: &'a ResolvedModelCandidate,
        context: &'a awaken_runtime_contract::RuntimeRunContext,
    ) -> Result<Option<(&'a CredentialAccess, &'a AttemptCredentialBinding)>, String> {
        let ModelProvisioning::Remote { credential, .. } = candidate.provisioning() else {
            return Err("model candidate has no remote provisioning".into());
        };
        Self::claimed_binding(candidate, context, credential.as_deref(), "remote")
    }

    fn claimed_binding<'a>(
        candidate: &'a ResolvedModelCandidate,
        context: &'a awaken_runtime_contract::RuntimeRunContext,
        credential: Option<&'a CredentialAccess>,
        kind: &str,
    ) -> Result<Option<(&'a CredentialAccess, &'a AttemptCredentialBinding)>, String> {
        let Some(access) = credential else {
            return Ok(None);
        };
        let realization = AttemptCredentialRealization::from_context(context)
            .ok_or_else(|| format!("credential-bearing {kind} has no claim binding"))?;
        let binding = realization
            .binding_for(candidate)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("{kind} candidate has no exact claim binding"))?;
        if binding.credential != access.credential {
            return Err(format!(
                "{kind} claim binding selects a different credential"
            ));
        }
        Ok(Some((access, binding)))
    }

    async fn admit_claimed_materialization<'a>(
        binding: &AttemptCredentialBinding,
        context: &'a awaken_runtime_contract::RuntimeRunContext,
        expected_kind: CredentialRealizationKind,
        kind: &str,
    ) -> Result<&'a AttemptCredentialRealization, ClaimedMaterializationError> {
        if binding.selected_realization_kind != expected_kind {
            return Err(format!(
                "{kind} claim binding selects {:?}, expected {:?}",
                binding.selected_realization_kind, expected_kind
            )
            .into());
        }
        let realization = AttemptCredentialRealization::from_context(context)
            .ok_or_else(|| format!("credential-bearing {kind} has no claim binding"))?;
        let ownership = context
            .ownership
            .as_deref()
            .ok_or_else(|| format!("credential-bearing {kind} has no claim fence"))?;
        check_attempt_ownership(Some(ownership)).await?;
        Ok(realization)
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
            .await
            .map_err(|error| error.to_string())?
            .map(|material| material.into_secret().map_err(|error| error.to_string()))
            .transpose()
    }

    /// The claimed provider material together with the exact resolution that
    /// served it, for an executor that presents it to the provider and must
    /// report an authentication refusal back to that resolution.
    pub(crate) async fn materialize_claimed_provider_served(
        &self,
        candidate: &ResolvedModelCandidate,
        context: &awaken_runtime_contract::RuntimeRunContext,
        expected_kind: CredentialRealizationKind,
    ) -> Result<
        Option<(
            awaken_credential_contract::CredentialMaterial,
            ServedProviderMaterial,
        )>,
        ClaimedMaterializationError,
    > {
        let Some((material, served)) = self
            .materialize_claimed_material(
                candidate,
                context,
                Self::claimed_provider_binding(candidate, context)?,
                expected_kind,
                "provider",
            )
            .await?
        else {
            return Ok(None);
        };
        let served =
            served.ok_or_else(|| "provider material has no serving resolution".to_string())?;
        Ok(Some((material, served)))
    }

    /// Materialize the exact A2A HTTP-header credential frozen in the current
    /// claim. Anonymous remote publications return `None`; selection and Agent
    /// Card interpretation remain publication concerns.
    pub async fn materialize_claimed_remote(
        &self,
        candidate: &ResolvedModelCandidate,
        context: &awaken_runtime_contract::RuntimeRunContext,
    ) -> Result<Option<RedactedString>, String> {
        self.materialize_claimed_material(
            candidate,
            context,
            Self::claimed_remote_binding(candidate, context)?,
            CredentialRealizationKind::WorkerRelay,
            "remote",
        )
        .await
        .map_err(|error| error.to_string())?
        .map(|(material, _)| material.into_secret().map_err(|error| error.to_string()))
        .transpose()
    }

    pub(super) async fn materialize_claimed_provider_material(
        &self,
        candidate: &ResolvedModelCandidate,
        context: &awaken_runtime_contract::RuntimeRunContext,
        expected_kind: CredentialRealizationKind,
    ) -> Result<Option<awaken_credential_contract::CredentialMaterial>, ClaimedMaterializationError>
    {
        Ok(self
            .materialize_claimed_material(
                candidate,
                context,
                Self::claimed_provider_binding(candidate, context)?,
                expected_kind,
                "provider",
            )
            .await?
            .map(|(material, _)| material))
    }

    async fn materialize_claimed_material(
        &self,
        candidate: &ResolvedModelCandidate,
        context: &awaken_runtime_contract::RuntimeRunContext,
        selected: Option<(&CredentialAccess, &AttemptCredentialBinding)>,
        expected_kind: CredentialRealizationKind,
        kind: &str,
    ) -> Result<
        Option<(
            awaken_credential_contract::CredentialMaterial,
            Option<ServedProviderMaterial>,
        )>,
        ClaimedMaterializationError,
    > {
        let Some((access, binding)) = selected else {
            return Ok(None);
        };
        let realization =
            Self::admit_claimed_materialization(binding, context, expected_kind, kind).await?;
        let delivered = realization
            .delivered_access_for(candidate, access)
            .await
            .map_err(|error| error.to_string())?;
        check_attempt_ownership(context.ownership.as_deref()).await?;
        let secret = match candidate.provisioning() {
            ModelProvisioning::Provider { .. } => {
                let (material, served) = self
                    .materialize_provider_for(
                        candidate,
                        delivered.as_ref(),
                        &binding.selected_plaintext_holder,
                        binding.selected_realization_kind,
                    )
                    .await?;
                (material, Some(served))
            }
            ModelProvisioning::Remote { .. } => (
                self.materialize_remote_for(
                    candidate,
                    delivered.as_ref(),
                    &binding.selected_plaintext_holder,
                    binding.selected_realization_kind,
                )
                .await?,
                None,
            ),
            ModelProvisioning::HostExecutor | ModelProvisioning::BackendOwned { .. } => {
                return Err(format!("model candidate has no {kind} provisioning").into());
            }
        };
        // The original read remains owned until it settles. Its returned
        // bytes cannot authorize the receipt after a claim/tenure handoff.
        check_attempt_ownership(context.ownership.as_deref()).await?;
        realization
            .record(binding)
            .await
            .map_err(|error| error.to_string())?;
        // A receipt may have committed before its acknowledgement was delayed.
        // Retain that fact, but release no material under superseded authority.
        check_attempt_ownership(context.ownership.as_deref()).await?;
        Ok(Some(secret))
    }
}
