// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: Native owner ports and current IAM admission remain independent;
// @formal reason: this HTTP projection never treats a preference as a grant.
//! Bounded personal Agent directory over the canonical Config owner.
use awaken_agent_config::catalog_snapshot::AgentCatalogSnapshotRepository;
use awaken_agent_config::personal_selection::PersonalAgentSelectionRepository;
use awaken_iam_contract::ProductId;
use serde::Deserialize;
use std::sync::Arc;

mod authority;
pub(crate) mod legacy;
pub(crate) mod managed_definition_reads;
mod routes;
mod selection_reads;

/// Explicit product registration selected by deployment. Omission grants no
/// legacy Workspace role any new Agent authority and leaves this surface closed.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentGovernanceConfig {
    pub product_id: ProductId,
}

/// Mandatory read and preference views of the same Config adapter. Construction opens no
/// store, registers no policy, and authenticates no person.
#[derive(Clone)]
pub struct AgentGovernanceStores {
    pub(crate) catalog: Arc<dyn AgentCatalogSnapshotRepository>,
    pub(crate) selections: Arc<dyn PersonalAgentSelectionRepository>,
    pub(crate) config: Option<AgentGovernanceConfig>,
}
impl AgentGovernanceStores {
    pub fn new<T>(store: Arc<T>, config: Option<AgentGovernanceConfig>) -> Self
    where
        T: AgentCatalogSnapshotRepository + PersonalAgentSelectionRepository + 'static,
    {
        Self {
            catalog: store.clone(),
            selections: store,
            config,
        }
    }
}

pub(crate) use routes::router;

/// Canonical resource coordinate for explicit native product projections. The
/// caller must supply already verified identity coordinates; this pure function
/// neither verifies them nor registers any resource or grant.
pub fn agent_governance_resource_id(
    issuer: &str,
    workspace: &str,
    agent: &str,
) -> Result<awaken_iam_contract::ResourceId, String> {
    authority::resource_id(issuer, workspace, agent)
        .map_err(|_| "invalid native Agent resource coordinates".to_owned())
}

/// Explicit PAP candidate for operator adoption under the existing Workspace
/// namespace. This does not activate a profile, register a product/projection,
/// modify an old profile or add grants for the new actions. Native IAM validates
/// the product vocabulary and immutable owner binding when it is adopted.
pub fn agent_governance_authorization_profile(
    product: ProductId,
) -> awaken_iam_contract::CreateAuthorizationProfile {
    use awaken_iam_contract::{ActionScopeRule, ResourceType, ScopeKind};
    let mut candidate = crate::authz::workspace_authorization_profile();
    let model = agent_governance_resource_model();
    candidate.document.resource_product = Some(product);
    candidate.document.action_scope_rules.extend(
        model
            .resource_types
            .iter()
            .flat_map(|kind| kind.actions.iter())
            .map(|action| ActionScopeRule {
                action_pattern: action.0.clone(),
                allowed_scope_kinds: vec![ScopeKind::Resource {
                    resource_type: ResourceType(authority::RESOURCE_TYPE.into()),
                }],
            }),
    );
    candidate
        .document
        .resource_model
        .resource_types
        .extend(model.resource_types);
    candidate
}

/// Exact additional native product vocabulary. Legacy Workspace wildcard
/// profiles are PAP contracts and are deliberately not product registrations.
/// The deployment registers this vocabulary explicitly with its actual owner;
/// this pure declaration performs no registration or projection write.
pub fn agent_governance_resource_model() -> awaken_iam_contract::ResourceModelRegistration {
    use awaken_iam_contract::{
        ActionKey, NamespaceId, ResourceModelRegistration, ResourceType, ResourceTypeRegistration,
    };
    let actions = [
        authority::USE,
        authority::SELECTION_READ,
        authority::SELECTION_WRITE,
        authority::DEFINITION_READ,
        "agent.definition.write",
        "agent.definition.validate",
        "agent.definition.publish",
        "agent.definition.archive",
    ];
    ResourceModelRegistration {
        resource_types: vec![ResourceTypeRegistration {
            resource_type: ResourceType(authority::RESOURCE_TYPE.into()),
            parent_type: None,
            actions: actions
                .into_iter()
                .map(|action| {
                    ActionKey::in_namespace(&NamespaceId(authority::NAMESPACE.into()), action)
                })
                .collect(),
        }],
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_iam_contract::{ActionKey, NamespaceId, ResourceType, ScopeKind};

    #[test]
    fn resource_identity_is_exact_length_prefixed_issuer_workspace_and_agent() {
        let id =
            agent_governance_resource_id("https://native.example", "workspace", "agent").unwrap();
        for triple in [
            ("https://other.example", "workspace", "agent"),
            ("https://native.example", "other", "agent"),
            ("https://native.example", "workspace", "other"),
            ("https://native.example", "workspace/agent", "x"),
            ("https://native.example", "workspace", "agent/x"),
        ] {
            assert_ne!(
                agent_governance_resource_id(triple.0, triple.1, triple.2).unwrap(),
                id
            );
        }
        assert_ne!(
            agent_governance_resource_id("a", "b/c", "d").unwrap(),
            agent_governance_resource_id("a/b", "c", "d").unwrap()
        );
        assert_eq!(
            agent_governance_resource_id("原生", "本域", "助理").unwrap(),
            agent_governance_resource_id("原生", "本域", "助理").unwrap()
        );
        for invalid in ["", " space", "line\n"] {
            assert!(
                agent_governance_resource_id("https://native.example", "workspace", invalid)
                    .is_err()
            );
        }
    }

    #[test]
    fn explicit_profile_registers_fine_resource_actions_without_new_grants_or_legacy_adoption() {
        let before = crate::authz::workspace_authorization_profile();
        assert!(before.document.resource_product.is_none());
        let product = ProductId::new("test-operator-declared").unwrap();
        let profile = agent_governance_authorization_profile(product.clone());
        assert_eq!(profile.document.resource_product, Some(product));
        assert_eq!(profile.document.grants, before.document.grants);
        let kind = ResourceType("awaken.agent".into());
        let model = profile
            .document
            .resource_model
            .resource_types
            .iter()
            .find(|registration| registration.resource_type == kind)
            .unwrap();
        for action in [
            "agent.use",
            "agent.selection.read",
            "agent.selection.write",
            "agent.definition.read",
        ] {
            let qualified =
                ActionKey::in_namespace(&NamespaceId("awaken.workspace".into()), action);
            assert!(model.actions.contains(&qualified));
            assert!(
                profile
                    .document
                    .action_scope_rules
                    .iter()
                    .any(|rule| rule.action_pattern == qualified.0
                        && rule.allowed_scope_kinds
                            == vec![ScopeKind::Resource {
                                resource_type: kind.clone()
                            }])
            );
            assert!(
                !profile
                    .document
                    .grants
                    .iter()
                    .any(|grant| grant.action_pattern == qualified.0)
            );
        }
        assert_eq!(crate::authz::workspace_authorization_profile(), before);
        assert!(
            serde_json::from_str::<AgentGovernanceConfig>(
                r#"{"product_id":"test-operator-declared","grant":true}"#
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod native_http_tests;

/// Preserve official full Managed Session DTOs behind independent fine definition
/// admission; the original router continues to enforce its run/owner authority.
pub fn protect_managed_agent_definitions(
    router: axum::Router,
    iam: Option<Arc<crate::RemoteManagementAuthz>>,
    config: Option<AgentGovernanceConfig>,
) -> axum::Router {
    managed_definition_reads::protect(router, iam, config)
}
