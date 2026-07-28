//! Deterministic authorization profiles owned by Awaken.

use awaken_iam_contract::{
    ActionKey, ActionScopeRule, AuthorizationProfileDocument, CreateAuthorizationProfile,
    GrantEffect, GrantSnapshot, GrantSubjectRef, NamespaceId, ResourceModelRegistration, ScopeKind,
    ScopeRef, Timestamp,
};
use awaken_iam_core::RoleId;
use awaken_iam_preset::named_role_catalog;

pub const MANAGEMENT_POLICY_NAMESPACE: &str = "awaken.runtime.management";
/// Qualified role intended for an external product that publishes Agent
/// configuration but must never administer API credentials.
pub const MANAGEMENT_AGENT_PUBLISHER_ROLE: &str = "awaken.runtime.management:agent_publisher";
pub const HOSTED_RUNTIME_POLICY_NAMESPACE: &str = "awaken.runtime";
pub const HOSTED_RUNTIME_WORKSPACE_ADMIN_ROLE: &str = "awaken.runtime:workspace_admin";
pub const HOSTED_RUNTIME_AGENT_EXECUTOR_ROLE: &str = "awaken.runtime:agent_executor";
const RESOURCE_POLICY_NAMESPACE: &str = "awaken.runtime.resources";
pub(super) const AUTHORIZATION_PROFILE_EPOCH: &str = "2020-01-01T00:00:00Z";

pub(super) fn qualify_action(action: &str) -> ActionKey {
    ActionKey::in_namespace(&NamespaceId(MANAGEMENT_POLICY_NAMESPACE.to_owned()), action)
}

pub(super) fn qualify_resource_action(action: &str) -> ActionKey {
    ActionKey::in_namespace(&NamespaceId(RESOURCE_POLICY_NAMESPACE.to_owned()), action)
}

fn qualify_hosted_runtime_action(action: &str) -> ActionKey {
    ActionKey::in_namespace(
        &NamespaceId(HOSTED_RUNTIME_POLICY_NAMESPACE.to_owned()),
        action,
    )
}

pub(super) fn qualify_role(role: &str) -> RoleId {
    let prefix = format!("{MANAGEMENT_POLICY_NAMESPACE}:");
    if role.starts_with(&prefix) {
        RoleId(role.to_owned())
    } else {
        RoleId(format!("{prefix}{role}"))
    }
}

pub(super) fn qualify_resource_role(role: &str) -> RoleId {
    let prefix = format!("{RESOURCE_POLICY_NAMESPACE}:");
    if role.starts_with(&prefix) {
        RoleId(role.to_owned())
    } else {
        RoleId(format!("{prefix}{role}"))
    }
}

/// The immutable authorization contract owned by the Awaken Management
/// bounded context.
///
/// Embedded IAM and hosted deployment tooling consume this same value. The
/// fixed timestamp makes the release projection byte-stable; it is contract
/// metadata, not an activation time.
pub fn management_authorization_profile() -> CreateAuthorizationProfile {
    let created_at = Timestamp(AUTHORIZATION_PROFILE_EPOCH.to_owned());
    let mut grants = Vec::new();
    for role in named_role_catalog(&created_at) {
        for (index, pattern) in role.action_patterns.iter().enumerate() {
            if !(pattern.0.starts_with("workspace.") || pattern.0.starts_with("apikey.")) {
                continue;
            }
            grants.push(GrantSnapshot {
                id: format!(
                    "{MANAGEMENT_POLICY_NAMESPACE}:grant:role:{}:{index}",
                    role.id.0
                ),
                subject: GrantSubjectRef::Role {
                    role_id: qualify_role(&role.id.0).0,
                },
                action_pattern: qualify_action(&pattern.0).0,
                scope: ScopeRef::Global,
                effect: GrantEffect::Allow,
            });
        }
    }
    grants.push(GrantSnapshot {
        id: format!("{MANAGEMENT_POLICY_NAMESPACE}:grant:role:agent_publisher"),
        subject: GrantSubjectRef::Role {
            role_id: MANAGEMENT_AGENT_PUBLISHER_ROLE.to_owned(),
        },
        action_pattern: qualify_action("workspace.*").0,
        scope: ScopeRef::Global,
        effect: GrantEffect::Allow,
    });

    CreateAuthorizationProfile {
        namespace: NamespaceId(MANAGEMENT_POLICY_NAMESPACE.to_owned()),
        document: AuthorizationProfileDocument {
            resource_model: ResourceModelRegistration {
                actions: ["workspace.*", "apikey.*"]
                    .into_iter()
                    .map(qualify_action)
                    .collect(),
                ..ResourceModelRegistration::default()
            },
            action_scope_rules: ["workspace.*", "apikey.*"]
                .into_iter()
                .map(|pattern| ActionScopeRule {
                    action_pattern: qualify_action(pattern).0,
                    allowed_scope_kinds: vec![ScopeKind::Workspace],
                })
                .collect(),
            grants,
            ..AuthorizationProfileDocument::default()
        },
        created_at,
    }
}

/// The immutable authorization contract for Management-owned File and Skill
/// resources. Embedded IAM and hosted deployments consume this same value.
pub fn management_resource_authorization_profile() -> CreateAuthorizationProfile {
    let created_at = Timestamp(AUTHORIZATION_PROFILE_EPOCH.to_owned());
    let patterns = ["workspace.*", "file.*", "skill.*"];
    let mut grants = Vec::new();
    for role in named_role_catalog(&created_at) {
        for (index, pattern) in role.action_patterns.iter().enumerate() {
            if !patterns
                .iter()
                .any(|prefix| pattern.0.starts_with(prefix.trim_end_matches('*')))
            {
                continue;
            }
            grants.push(GrantSnapshot {
                id: format!(
                    "{RESOURCE_POLICY_NAMESPACE}:grant:role:{}:{index}",
                    role.id.0
                ),
                subject: GrantSubjectRef::Role {
                    role_id: qualify_resource_role(&role.id.0).0,
                },
                action_pattern: qualify_resource_action(&pattern.0).0,
                scope: ScopeRef::Global,
                effect: GrantEffect::Allow,
            });
        }
    }
    CreateAuthorizationProfile {
        namespace: NamespaceId(RESOURCE_POLICY_NAMESPACE.to_owned()),
        document: AuthorizationProfileDocument {
            resource_model: ResourceModelRegistration {
                actions: patterns
                    .iter()
                    .map(|pattern| qualify_resource_action(pattern))
                    .collect(),
                ..ResourceModelRegistration::default()
            },
            action_scope_rules: patterns
                .iter()
                .map(|pattern| ActionScopeRule {
                    action_pattern: qualify_resource_action(pattern).0,
                    allowed_scope_kinds: vec![ScopeKind::Workspace],
                })
                .collect(),
            grants,
            ..AuthorizationProfileDocument::default()
        },
        created_at,
    }
}

/// Immutable Hosted Run lifecycle authorization contract.
///
/// Awaken owns the `run.*` vocabulary and the roles that may invoke it. A
/// hosting platform owns profile activation and exact Workspace bindings.
pub fn hosted_runtime_authorization_profile() -> CreateAuthorizationProfile {
    let created_at = Timestamp(AUTHORIZATION_PROFILE_EPOCH.to_owned());
    let action_pattern = qualify_hosted_runtime_action("run.*").0;
    CreateAuthorizationProfile {
        namespace: NamespaceId(HOSTED_RUNTIME_POLICY_NAMESPACE.to_owned()),
        document: AuthorizationProfileDocument {
            resource_model: ResourceModelRegistration {
                actions: ["run.create", "run.read", "run.resume", "run.cancel"]
                    .into_iter()
                    .map(qualify_hosted_runtime_action)
                    .collect(),
                ..ResourceModelRegistration::default()
            },
            action_scope_rules: vec![ActionScopeRule {
                action_pattern: action_pattern.clone(),
                allowed_scope_kinds: vec![ScopeKind::Workspace],
            }],
            grants: [
                HOSTED_RUNTIME_WORKSPACE_ADMIN_ROLE,
                HOSTED_RUNTIME_AGENT_EXECUTOR_ROLE,
            ]
            .into_iter()
            .map(|role_id| GrantSnapshot {
                id: format!("{HOSTED_RUNTIME_POLICY_NAMESPACE}:grant:role:{role_id}"),
                subject: GrantSubjectRef::Role {
                    role_id: role_id.to_owned(),
                },
                action_pattern: action_pattern.clone(),
                scope: ScopeRef::Global,
                effect: GrantEffect::Allow,
            })
            .collect(),
            ..AuthorizationProfileDocument::default()
        },
        created_at,
    }
}
