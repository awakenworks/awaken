//! Claim-bound provider and remote credential materialization.
//!
//! This module owns the ordered boundary from an immutable attempt binding to
//! current ownership, plaintext access, and the corresponding durable receipt.

use awaken_agent_contract::RedactedString;
use awaken_runtime_contract::resolved::{ModelProvisioning, ResolvedModelCandidate};
use awaken_runtime_contract::{
    AttemptCredentialBinding, AttemptCredentialRealization, CredentialAccess,
    CredentialRealizationKind,
};

use super::PinnedCredentialMaterializer;

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
        let realization = context
            .credential_realization
            .as_ref()
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
    ) -> Result<&'a AttemptCredentialRealization, String> {
        if binding.selected_realization_kind != expected_kind {
            return Err(format!(
                "{kind} claim binding selects {:?}, expected {:?}",
                binding.selected_realization_kind, expected_kind
            ));
        }
        let realization = context
            .credential_realization
            .as_ref()
            .ok_or_else(|| format!("credential-bearing {kind} has no claim binding"))?;
        let ownership = context
            .ownership
            .as_deref()
            .ok_or_else(|| format!("credential-bearing {kind} has no claim fence"))?;
        awaken_runtime_contract::execution::verify_attempt_ownership(Some(ownership))
            .await
            .map_err(|error| error.to_string())?;
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
            .await?
            .map(|material| material.into_secret().map_err(|error| error.to_string()))
            .transpose()
    }

    /// Materialize the exact A2A HTTP-header credential frozen in the current
    /// claim. Anonymous remote publications return `None`; selection and Agent
    /// Card interpretation remain publication concerns.
    pub async fn materialize_claimed_remote(
        &self,
        candidate: &ResolvedModelCandidate,
        context: &awaken_runtime_contract::RuntimeRunContext,
    ) -> Result<Option<RedactedString>, String> {
        let Some((access, binding)) = Self::claimed_remote_binding(candidate, context)? else {
            return Ok(None);
        };
        let realization = Self::admit_claimed_materialization(
            binding,
            context,
            CredentialRealizationKind::WorkerRelay,
            "remote",
        )
        .await?;
        let delivered = realization
            .delivered_access_for(candidate, access)
            .await
            .map_err(|error| error.to_string())?;
        awaken_runtime_contract::execution::verify_attempt_ownership(context.ownership.as_deref())
            .await
            .map_err(|error| error.to_string())?;
        let secret = self
            .materialize_remote_for(
                candidate,
                delivered.as_ref(),
                &binding.selected_plaintext_holder,
                binding.selected_realization_kind,
            )
            .await?;
        realization
            .record(binding)
            .await
            .map_err(|error| error.to_string())?;
        secret
            .into_secret()
            .map(Some)
            .map_err(|error| error.to_string())
    }

    pub(super) async fn materialize_claimed_provider_material(
        &self,
        candidate: &ResolvedModelCandidate,
        context: &awaken_runtime_contract::RuntimeRunContext,
        expected_kind: CredentialRealizationKind,
    ) -> Result<Option<awaken_runtime_contract::CredentialMaterial>, String> {
        let Some((access, binding)) = Self::claimed_provider_binding(candidate, context)? else {
            return Ok(None);
        };
        let realization =
            Self::admit_claimed_materialization(binding, context, expected_kind, "provider")
                .await?;
        let delivered = realization
            .delivered_access_for(candidate, access)
            .await
            .map_err(|error| error.to_string())?;
        awaken_runtime_contract::execution::verify_attempt_ownership(context.ownership.as_deref())
            .await
            .map_err(|error| error.to_string())?;
        let secret = self
            .materialize_provider_for(
                candidate,
                delivered.as_ref(),
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
}
