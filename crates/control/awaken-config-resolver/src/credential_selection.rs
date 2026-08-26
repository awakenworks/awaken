//! Resolver-owned model dialect and credential-selection joins.
//!
//! This module is the single secret-free selection authority shared by
//! publication and permitted management materialization.

use crate::{ResolveError, SourceLookup};
#[cfg(test)]
use awaken_credential_vault::CredentialKind;
use awaken_credential_vault::{
    AvailabilityLedger, CredentialBinding, CredentialPool, CredentialPoolId, CredentialPoolMember,
    CredentialSource, CredentialStatus, SelectionPolicy,
};

/// May this credential authenticate the model provider?
#[must_use]
pub fn can_consume(
    offering_provider_id: &str,
    offering_endpoint_id: Option<&str>,
    source: &CredentialSource,
) -> bool {
    if source.descriptor.is_some() || source.is_claude_code_setup_token() {
        return false;
    }
    source
        .authorization_scope()
        .authorizes(offering_provider_id, offering_endpoint_id)
}

/// The resolver-owned provider/backend validity join used by both default
/// derivation and explicit bindings. Claude setup tokens are deliberately
/// narrower than ordinary provider credentials: only managed `acp:claude` may
/// consume them.
#[must_use]
pub fn credential_can_supply(
    offering_provider_id: &str,
    offering_endpoint_id: Option<&str>,
    backend_ref: &str,
    source: &CredentialSource,
) -> bool {
    if source.is_claude_code_setup_token() {
        return source.descriptor.is_none()
            && offering_provider_id == "anthropic"
            && backend_ref == "acp:claude"
            && source
                .authorization_scope()
                .authorizes("anthropic", offering_endpoint_id);
    }
    can_consume(offering_provider_id, offering_endpoint_id, source)
}

/// Whether this persisted source is executable now and compatible with one
/// exact provider/backend demand. This is the sole lifecycle + positive-source-
/// revision + material-origin + provider-compatibility predicate used by
/// selection and readiness views; callers may add Workspace or
/// consumer-specific constraints but must not reconstruct these gates.
#[must_use]
pub fn credential_is_executable_supply(
    offering_provider_id: &str,
    offering_endpoint_id: Option<&str>,
    backend_ref: &str,
    source: &CredentialSource,
) -> bool {
    source.status == CredentialStatus::Active
        && source.version > 0
        && source.is_executable_origin()
        && credential_can_supply(
            offering_provider_id,
            offering_endpoint_id,
            backend_ref,
            source,
        )
}

/// Derive the default, non-persisted vendor pool for one Workspace and
/// executable backend.
#[must_use]
pub fn derive_vendor_pool(
    workspace_id: &str,
    offering_provider_id: &str,
    offering_endpoint_id: Option<&str>,
    backend_ref: &str,
    sources: &[CredentialSource],
) -> CredentialPool {
    let mut eligible = sources
        .iter()
        .filter(|source| {
            source.workspace_id == workspace_id
                && credential_is_executable_supply(
                    offering_provider_id,
                    offering_endpoint_id,
                    backend_ref,
                    source,
                )
        })
        .collect::<Vec<_>>();
    // Every candidate passed the canonical provider/endpoint join; preserve
    // deterministic identity order without reinterpreting authorization scope.
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

/// Resolver-bound selection inputs that do not own credential or catalog data.
/// Keeping this bundle explicit prevents publication and preview callers from
/// growing parallel positional parameter conventions.
#[derive(Clone, Copy, Default)]
pub struct CredentialSelectionContext<'a> {
    pub offering_provider: Option<&'a str>,
    pub offering_endpoint: Option<&'a str>,
    pub backend_ref: Option<&'a str>,
    pub availability: Option<(&'a AvailabilityLedger, u64)>,
    pub expected_workspace: Option<&'a str>,
    pub selection_sequence: u64,
}

/// Resolve one binding into a canonical, policy-ordered source set without
/// materializing secrets. `selection_sequence` is meaningful only for
/// `RotateSpread`; publication/application orchestration owns that sequence.
pub fn credential_candidates<'a>(
    binding: &CredentialBinding,
    sources: &'a dyn SourceLookup,
    context: CredentialSelectionContext<'_>,
) -> Result<CredentialCandidateSet<'a>, ResolveError> {
    let CredentialSelectionContext {
        offering_provider,
        offering_endpoint,
        backend_ref,
        availability,
        expected_workspace,
        selection_sequence,
    } = context;
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
                && !credential_can_supply(
                    provider,
                    offering_endpoint,
                    backend_ref.unwrap_or("genai"),
                    source,
                )
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
                        credential_can_supply(
                            provider,
                            offering_endpoint,
                            backend_ref.unwrap_or("genai"),
                            source,
                        )
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

    // Cause/effect decision table for D5 default vendor-pool derivation:
    // R1 same Workspace + Active persisted material + matching provider -> member;
    // R2 wrong Workspace/provider, inactive/nonpositive revision, legacy Env,
    // or a described source whose Provider/A2A target compiler is intentionally
    // unsupported -> excluded;
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
            id: awaken_credential_contract::CredentialSourceId(id.into()),
            replacement_of: None,
            workspace_id: workspace.into(),
            kind,
            descriptor: None,
            provider_id: Some(provider.into()),
            protocol_endpoint_id: None,
            env_key: env_key.map(str::to_string),
            material_ref: None,
            auxiliary_material_refs: Default::default(),
            oauth_command: None,
            worker_local_binding: None,
            status,
            version: 1,
        };
        let mut described = source(
            "cred:described",
            "ws",
            "anthropic",
            CredentialKind::Vault,
            CredentialStatus::Active,
            Some(awaken_credential_vault::CLAUDE_CODE_SETUP_TOKEN_ENV),
        );
        described.provider_id = None;
        described.descriptor = Some(awaken_credential_contract::CredentialDescriptor::new(
            "anthropic",
            awaken_credential_contract::CredentialMaterialDescriptor::structured(
                awaken_credential_contract::HTTP_BASIC_MATERIAL_TYPE,
                ["password", "username"],
            ),
            [awaken_credential_contract::CredentialTargetContract::new(
                awaken_credential_contract::CredentialTarget::new(
                    awaken_credential_contract::CredentialPurpose::RepositoryTransport,
                    awaken_credential_contract::repository_transport_audience(
                        "https://github.com/awaken/example.git",
                    )
                    .expect("described target"),
                ),
                awaken_credential_contract::CredentialUsage::HttpBasicAuth,
            )],
        ));
        let mut zero_revision = source(
            "cred:zero",
            "ws",
            "anthropic",
            CredentialKind::Vault,
            CredentialStatus::Active,
            None,
        );
        zero_revision.version = 0;
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
            zero_revision,
            described,
        ];
        let ids = |backend_ref| {
            derive_vendor_pool("ws", "anthropic", None, backend_ref, &sources)
                .selection_order()
                .into_iter()
                .map(|member| member.credential_source_id.0.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids("acp:claude"), ["cred:setup", "cred:api"], "R1-R3");
        assert_eq!(ids("genai"), ["cred:api"], "R1/R2/R4");
    }

    #[test]
    fn endpoint_scope_is_part_of_the_single_credential_validity_join() {
        // Cause/effect decision table:
        // R1 provider-wide source + matching provider + any endpoint -> eligible;
        // R2 endpoint-scoped source + exact provider/endpoint -> eligible;
        // R3 endpoint-scoped source + same provider/different or absent endpoint
        // -> ineligible; R4 any provider mismatch -> ineligible; R5 malformed
        // endpoint scope without a provider -> ineligible. This proves
        // discovery and publication can reuse one provider×endpoint join without
        // deriving dialect or credential-delivery policy from the endpoint.
        let source = |provider: &str, endpoint: Option<&str>| CredentialSource {
            id: awaken_credential_contract::CredentialSourceId("credential".into()),
            replacement_of: None,
            workspace_id: "workspace".into(),
            kind: CredentialKind::Vault,
            descriptor: None,
            provider_id: Some(provider.into()),
            protocol_endpoint_id: endpoint.map(str::to_string),
            env_key: None,
            material_ref: None,
            auxiliary_material_refs: Default::default(),
            oauth_command: None,
            worker_local_binding: None,
            status: CredentialStatus::Active,
            version: 1,
        };
        let provider_wide = source("openai", None);
        let endpoint_scoped = source("openai", Some("openai.open_ai_chat.primary"));

        assert!(can_consume("openai", Some("other"), &provider_wide), "R1");
        assert!(
            can_consume(
                "openai",
                Some("openai.open_ai_chat.primary"),
                &endpoint_scoped,
            ),
            "R2"
        );
        assert!(
            !can_consume(
                "openai",
                Some("openai.open_ai_chat.backup"),
                &endpoint_scoped
            ),
            "R3"
        );
        assert!(!can_consume("openai", None, &endpoint_scoped), "R3");
        assert!(
            !can_consume(
                "anthropic",
                Some("openai.open_ai_chat.primary"),
                &endpoint_scoped,
            ),
            "R4"
        );
        let malformed = source("openai", Some("openai.open_ai_chat.primary"));
        let malformed = CredentialSource {
            provider_id: None,
            ..malformed
        };
        assert!(
            !can_consume("openai", Some("openai.open_ai_chat.primary"), &malformed,),
            "R5"
        );
    }

    #[test]
    fn unscoped_material_is_not_a_provider_wildcard() {
        /* Cause/effect graph: C1=credential explicitly binds the offering
         * provider; C2=workspace/status/origin are eligible. Effects: E1=pool
         * membership; E2=exclude. Rules A1 C1+C2=>E1; A2 !C1+C2=>E2. */
        let scoped = CredentialSource {
            id: awaken_credential_contract::CredentialSourceId("cred:z-provider".into()),
            replacement_of: None,
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            descriptor: None,
            provider_id: Some("anthropic".into()),
            protocol_endpoint_id: None,
            env_key: None,
            material_ref: None,
            auxiliary_material_refs: Default::default(),
            oauth_command: None,
            worker_local_binding: None,
            status: CredentialStatus::Active,
            version: 1,
        };
        let unscoped = CredentialSource {
            provider_id: None,
            id: awaken_credential_contract::CredentialSourceId("cred:a-runtime".into()),
            ..scoped.clone()
        };
        let ids = derive_vendor_pool("ws", "anthropic", None, "genai", &[unscoped, scoped])
            .selection_order()
            .into_iter()
            .map(|member| member.credential_source_id.0.clone())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["cred:z-provider"], "A1/A2");
    }
}
