//! Resolver-owned model dialect and credential-selection joins.
//!
//! This module is the single secret-free selection authority shared by
//! publication and permitted management materialization.

#[cfg(test)]
use awaken_credential_vault::CredentialKind;
use awaken_credential_vault::{
    AvailabilityLedger, CredentialBinding, CredentialMaterialOrigin, CredentialPool,
    CredentialPoolId, CredentialPoolMember, CredentialSource, CredentialStatus, SelectionPolicy,
};
use awaken_model_catalog::ApiDialect;

use crate::{ResolveError, SourceLookup};

/// Resolver-owned projection from executor identity to model API dialect.
///
/// This table joins two independent axes. It intentionally does not live on
/// the ACP executor catalog: that leaf consumes resolved model strings and
/// must not depend on `awaken-model-catalog`.
#[must_use]
pub fn expected_acp_dialect(backend_ref: &str) -> Option<ApiDialect> {
    match backend_ref {
        "acp:claude" => Some(ApiDialect::AnthropicMessages),
        "acp:codex" => Some(ApiDialect::OpenAiChat),
        "acp:gemini" => Some(ApiDialect::Gemini),
        _ => None,
    }
}

/// Assert that an ACP executor can speak one catalog Offering's model API
/// dialect. Native/A2A coordinates are outside this join and pass unchanged.
pub fn validate_acp_dialect(backend_ref: &str, actual: ApiDialect) -> Result<(), ResolveError> {
    if !backend_ref.starts_with("acp:") {
        return Ok(());
    }
    let expected = expected_acp_dialect(backend_ref)
        .ok_or_else(|| ResolveError::AcpDialectUnknown(backend_ref.to_string()))?;
    if expected != actual {
        return Err(ResolveError::DialectIncompatible {
            backend_ref: backend_ref.to_string(),
            expected: expected.as_str(),
            actual: actual.as_str(),
        });
    }
    Ok(())
}

/// May this credential authenticate the model provider?
#[must_use]
pub fn can_consume(offering_provider_id: &str, source: &CredentialSource) -> bool {
    if source.is_claude_code_setup_token() {
        return false;
    }
    match (source.material_origin(), source.provider_id.as_deref()) {
        (CredentialMaterialOrigin::WorkerLocal, None) => false,
        (_, None) => true,
        (_, Some(scoped)) => scoped == offering_provider_id,
    }
}

/// The resolver-owned provider/backend validity join used by both default
/// derivation and explicit bindings. Claude setup tokens are deliberately
/// narrower than ordinary provider credentials: only managed `acp:claude` may
/// consume them.
#[must_use]
pub fn credential_can_supply(
    offering_provider_id: &str,
    backend_ref: &str,
    source: &CredentialSource,
) -> bool {
    if source.is_claude_code_setup_token() {
        return offering_provider_id == "anthropic"
            && backend_ref == "acp:claude"
            && source.provider_id.as_deref() == Some("anthropic");
    }
    can_consume(offering_provider_id, source)
}

/// Derive the default, non-persisted vendor pool for one Workspace and
/// executable backend.
#[must_use]
pub fn derive_vendor_pool(
    workspace_id: &str,
    offering_provider_id: &str,
    backend_ref: &str,
    sources: &[CredentialSource],
) -> CredentialPool {
    let mut eligible = sources
        .iter()
        .filter(|source| {
            source.workspace_id == workspace_id
                && source.status == CredentialStatus::Active
                && source.is_executable_origin()
                && credential_can_supply(offering_provider_id, backend_ref, source)
        })
        .collect::<Vec<_>>();
    eligible.sort_by_key(|source| (!source.is_claude_code_setup_token(), source.id.0.as_str()));
    let members = eligible
        .into_iter()
        .enumerate()
        .map(|(ordinal, source)| CredentialPoolMember {
            credential_source_id: source.id.clone(),
            ordinal: u32::try_from(ordinal).unwrap_or(u32::MAX),
            enabled: true,
            selection_weight: 0,
        })
        .collect();
    CredentialPool {
        id: CredentialPoolId(format!(
            "derived:{workspace_id}:{offering_provider_id}:{backend_ref}"
        )),
        workspace_id: workspace_id.to_string(),
        members,
        policy: SelectionPolicy::FirstHealthy,
    }
}

/// Secret-free result of resolving a [`CredentialBinding`] to its ordered
/// candidate sources.
#[derive(Debug)]
pub enum CredentialCandidateSet<'a> {
    None,
    Brokered,
    Direct {
        sources: Vec<&'a CredentialSource>,
        pool_id: Option<String>,
        total: usize,
        cooled: usize,
    },
}

impl<'a> CredentialCandidateSet<'a> {
    /// Return the first candidate accepted by the consumer-specific realization
    /// constraint while retaining the resolver-owned policy order.
    #[must_use]
    pub fn first_eligible(
        &self,
        mut eligible: impl FnMut(&CredentialSource) -> bool,
    ) -> Option<&'a CredentialSource> {
        match self {
            Self::Direct { sources, .. } => sources.iter().copied().find(|source| eligible(source)),
            Self::None | Self::Brokered => None,
        }
    }
}

/// Resolve one binding into a canonical, policy-ordered source set without
/// materializing secrets. `selection_sequence` is meaningful only for
/// `RotateSpread`; publication/application orchestration owns that sequence.
pub fn credential_candidates<'a>(
    binding: &CredentialBinding,
    sources: &'a dyn SourceLookup,
    offering_provider: Option<&str>,
    backend_ref: Option<&str>,
    availability: Option<(&AvailabilityLedger, u64)>,
    expected_workspace: Option<&str>,
    selection_sequence: u64,
) -> Result<CredentialCandidateSet<'a>, ResolveError> {
    match binding {
        CredentialBinding::None => Ok(CredentialCandidateSet::None),
        CredentialBinding::Brokered => Ok(CredentialCandidateSet::Brokered),
        CredentialBinding::Exact {
            credential_source_id,
        } => {
            let source = sources
                .get(credential_source_id.0.as_str())
                .ok_or_else(|| ResolveError::SourceMissing(credential_source_id.0.clone()))?;
            if expected_workspace.is_some_and(|workspace| source.workspace_id != workspace) {
                return Err(ResolveError::SourceMissing(credential_source_id.0.clone()));
            }
            if let Some(provider) = offering_provider
                && !credential_can_supply(provider, backend_ref.unwrap_or("genai"), source)
            {
                return Err(ResolveError::IncompatibleCredential {
                    source_id: credential_source_id.0.clone(),
                    provider_id: provider.to_string(),
                });
            }
            Ok(CredentialCandidateSet::Direct {
                sources: vec![source],
                pool_id: None,
                total: 1,
                cooled: 0,
            })
        }
        CredentialBinding::OneOfCredentialPool { credential_pool_id } => {
            let pool = sources
                .get_pool(credential_pool_id.0.as_str())
                .ok_or_else(|| ResolveError::PoolMissing(credential_pool_id.0.clone()))?;
            if expected_workspace.is_some_and(|workspace| pool.workspace_id != workspace) {
                return Err(ResolveError::PoolMissing(credential_pool_id.0.clone()));
            }
            let full = pool.selection_order_at(selection_sequence);
            let total = full.len();
            let order = match availability {
                Some((ledger, now_ms)) => {
                    pool.eligible_order_at(ledger, now_ms, selection_sequence)
                }
                None => full,
            };
            let cooled = total - order.len();
            let candidates = order
                .into_iter()
                .filter_map(|member| sources.get(member.credential_source_id.0.as_str()))
                .filter(|source| source.workspace_id == pool.workspace_id)
                .filter(|source| {
                    offering_provider.is_none_or(|provider| {
                        credential_can_supply(provider, backend_ref.unwrap_or("genai"), source)
                    })
                })
                .collect();
            Ok(CredentialCandidateSet::Direct {
                sources: candidates,
                pool_id: Some(credential_pool_id.0.clone()),
                total,
                cooled,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Cause/effect decision table for executor×model-dialect reconciliation:
    // R1 non-ACP backend + any dialect -> outside the join, accept;
    // R2 known ACP + expected dialect -> accept;
    // R3 known ACP + different dialect -> DialectIncompatible;
    // R4 unknown/bare ACP mapping -> AcpDialectUnknown.
    #[test]
    fn acp_executor_and_model_api_dialect_are_reconciled_independently() {
        assert!(
            validate_acp_dialect("genai", ApiDialect::Gemini).is_ok(),
            "R1"
        );
        assert!(
            validate_acp_dialect("acp:claude", ApiDialect::AnthropicMessages).is_ok(),
            "R2"
        );
        assert!(matches!(
            validate_acp_dialect("acp:claude", ApiDialect::OpenAiChat),
            Err(ResolveError::DialectIncompatible {
                backend_ref,
                expected: "anthropic_messages",
                actual: "open_ai_chat",
            }) if backend_ref == "acp:claude"
        ));
        assert!(matches!(
            validate_acp_dialect("acp:opencode", ApiDialect::OpenAiChat),
            Err(ResolveError::AcpDialectUnknown(backend)) if backend == "acp:opencode"
        ));
    }

    // Cause/effect decision table for D5 default vendor-pool derivation:
    // R1 same Workspace + Active persisted material + matching provider -> member;
    // R2 wrong Workspace/provider, inactive, or legacy Env -> excluded;
    // R3 Claude setup token + acp:claude -> eligible and ordered before API key;
    // R4 Claude setup token + native/other ACP -> excluded.
    #[test]
    fn vendor_pool_is_derived_once_from_counterparty_and_backend_constraints() {
        let source = |id: &str,
                      workspace: &str,
                      provider: &str,
                      kind: CredentialKind,
                      status: CredentialStatus,
                      env_key: Option<&str>| CredentialSource {
            id: awaken_credential_vault::CredentialSourceId(id.into()),
            workspace_id: workspace.into(),
            kind,
            provider_id: Some(provider.into()),
            env_key: env_key.map(str::to_string),
            material_ref: None,
            auxiliary_material_refs: Default::default(),
            oauth_command: None,
            worker_local_binding: None,
            status,
            version: 1,
        };
        let sources = vec![
            source(
                "cred:api",
                "ws",
                "anthropic",
                CredentialKind::Vault,
                CredentialStatus::Active,
                Some("ANTHROPIC_API_KEY"),
            ),
            source(
                "cred:setup",
                "ws",
                "anthropic",
                CredentialKind::Vault,
                CredentialStatus::Active,
                Some(awaken_credential_vault::CLAUDE_CODE_SETUP_TOKEN_ENV),
            ),
            source(
                "cred:other-workspace",
                "other",
                "anthropic",
                CredentialKind::Vault,
                CredentialStatus::Active,
                None,
            ),
            source(
                "cred:other-provider",
                "ws",
                "openai",
                CredentialKind::Vault,
                CredentialStatus::Active,
                None,
            ),
            source(
                "cred:disabled",
                "ws",
                "anthropic",
                CredentialKind::Vault,
                CredentialStatus::Disabled,
                None,
            ),
            source(
                "cred:env",
                "ws",
                "anthropic",
                CredentialKind::Env,
                CredentialStatus::Active,
                None,
            ),
        ];
        let ids = |backend_ref| {
            derive_vendor_pool("ws", "anthropic", backend_ref, &sources)
                .selection_order()
                .into_iter()
                .map(|member| member.credential_source_id.0.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids("acp:claude"), ["cred:setup", "cred:api"], "R1-R3");
        assert_eq!(ids("genai"), ["cred:api"], "R1/R2/R4");
    }
}
