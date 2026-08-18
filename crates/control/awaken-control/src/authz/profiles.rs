//! Deterministic authorization profiles owned by Awaken.

use awaken_authorization_contract::{
    AGENT_PUBLISHER_AUTHORITIES, CREDENTIAL_INGRESS_AUTHORITIES, HostedRuntimeRole,
    WorkspaceProfileRole, WorkspaceRoleAuthority, hosted_runtime_role_authorities,
    workspace_profile_role_authorities,
};
use awaken_iam_contract::{
    ActionKey, ActionScopeRule, AuthorizationProfileDocument, CreateAuthorizationProfile,
    GrantEffect, GrantSnapshot, GrantSubjectRef, NamespaceId, ResourceModelRegistration, ScopeKind,
    ScopeRef, Timestamp,
};
use awaken_iam_core::RoleId;
use awaken_iam_preset::named_role_catalog;

pub const AWAKEN_WORKSPACE_POLICY_NAMESPACE: &str = "awaken.workspace";
/// Qualified role for an external product that discovers executable model
/// supply, materializes Skills, and publishes Agent configuration.
pub const AWAKEN_WORKSPACE_PUBLISHER_ROLE: &str = "awaken.workspace:publisher";
/// Qualified role intended for an external product that ingresses generic
/// business credentials through the canonical Workspace Credential Vault.
pub const AWAKEN_WORKSPACE_CREDENTIAL_INGRESS_ROLE: &str = "awaken.workspace:credential_ingress";
/// Hosted tenant administrator: ordinary Workspace/API-key administration and
/// Resources access plus read-only platform model supply. Cloud binds this role
/// instead of local `workspace_admin`, whose BYOK authority remains self-hosted.
pub const AWAKEN_WORKSPACE_HOSTED_ADMIN_ROLE: &str = "awaken.workspace:hosted_admin";
/// Least-privilege role for a workload-identity principal admitted to the
/// research-preview MCP Tunnel API. Cloud binds this role after validating its
/// external `workspace:manage_tunnels` authority; it grants no ordinary
/// Workspace, credential, model, File, Skill, or Run operation.
pub const AWAKEN_WORKSPACE_TUNNEL_MANAGER_ROLE: &str = "awaken.workspace:tunnel_manager";
pub(super) const AWAKEN_WORKSPACE_LEGACY_HOSTED_ADMIN_ROLE: &str =
    "awaken.workspace:legacy_hosted_admin";
/// Hosted human member: the existing read-only Workspace role, fully qualified
/// under the one Awaken Workspace authorization language.
pub const AWAKEN_WORKSPACE_USER_ROLE: &str = "awaken.workspace:workspace_user";
pub const HOSTED_RUNTIME_POLICY_NAMESPACE: &str = "awaken.runtime";
pub const HOSTED_RUNTIME_WORKSPACE_ADMIN_ROLE: &str = "awaken.runtime:workspace_admin";
pub const HOSTED_RUNTIME_WORKSPACE_USER_ROLE: &str = "awaken.runtime:workspace_user";
pub const HOSTED_RUNTIME_AGENT_EXECUTOR_ROLE: &str = "awaken.runtime:agent_executor";
pub(super) const LEGACY_MANAGEMENT_POLICY_NAMESPACE: &str = "awaken.runtime.management";
pub(super) const LEGACY_RESOURCE_POLICY_NAMESPACE: &str = "awaken.runtime.resources";
pub(super) const AUTHORIZATION_PROFILE_EPOCH: &str = "2020-01-01T00:00:00Z";

pub(super) fn qualify_action(action: &str) -> ActionKey {
    ActionKey::in_namespace(
        &NamespaceId(AWAKEN_WORKSPACE_POLICY_NAMESPACE.to_owned()),
        action,
    )
}

pub(super) fn qualify_hosted_runtime_action(action: &str) -> ActionKey {
    ActionKey::in_namespace(
        &NamespaceId(HOSTED_RUNTIME_POLICY_NAMESPACE.to_owned()),
        action,
    )
}

pub(super) fn qualify_role(role: &str) -> RoleId {
    let prefix = format!("{AWAKEN_WORKSPACE_POLICY_NAMESPACE}:");
    if role.starts_with(&prefix) {
        RoleId(role.to_owned())
    } else {
        RoleId(format!("{prefix}{role}"))
    }
}

fn workspace_action_pattern(authority: WorkspaceRoleAuthority) -> &'static str {
    match authority.management_action_pattern() {
        Some(pattern) => pattern,
        None => authority
            .resource_action_pattern()
            .expect("every integration-role authority belongs to the Workspace profile"),
    }
}

/// The immutable Workspace authorization contract shared by the Control and
/// Resources PEP boundary without merging their domain/data ownership.
///
/// Embedded IAM and hosted deployment tooling consume this same value. The
/// fixed timestamp makes the release projection byte-stable; it is contract
/// metadata, not an activation time.
pub fn workspace_authorization_profile() -> CreateAuthorizationProfile {
    let created_at = Timestamp(AUTHORIZATION_PROFILE_EPOCH.to_owned());
    let action_patterns = [
        "workspace.*",
        "apikey.*",
        "model_supply.*",
        "file.*",
        "skill.*",
        "tunnel.manage",
    ];
    let mut grants = Vec::new();
    for role in named_role_catalog(&created_at) {
        if role.id.0 == "workspace_user" {
            for (index, authority) in
                workspace_profile_role_authorities(WorkspaceProfileRole::WorkspaceMember)
                    .iter()
                    .enumerate()
            {
                grants.push(GrantSnapshot {
                    id: format!(
                        "{AWAKEN_WORKSPACE_POLICY_NAMESPACE}:grant:role:workspace_user:{index}"
                    ),
                    subject: GrantSubjectRef::Role {
                        role_id: AWAKEN_WORKSPACE_USER_ROLE.to_owned(),
                    },
                    action_pattern: qualify_action(authority.action_pattern()).0,
                    scope: ScopeRef::Global,
                    effect: GrantEffect::Allow,
                });
            }
            continue;
        }
        for (index, pattern) in role.action_patterns.iter().enumerate() {
            if !["workspace.", "apikey.", "file.", "skill."]
                .iter()
                .any(|prefix| pattern.0.starts_with(prefix))
            {
                continue;
            }
            grants.push(GrantSnapshot {
                id: format!(
                    "{AWAKEN_WORKSPACE_POLICY_NAMESPACE}:grant:role:{}:{index}",
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
        let administers_workspace = matches!(role.id.0.as_str(), "admin" | "workspace_admin");
        if administers_workspace {
            grants.push(GrantSnapshot {
                id: format!(
                    "{AWAKEN_WORKSPACE_POLICY_NAMESPACE}:grant:role:{}:model-admin",
                    role.id.0
                ),
                subject: GrantSubjectRef::Role {
                    role_id: qualify_role(&role.id.0).0,
                },
                action_pattern: qualify_action("model_supply.*").0,
                scope: ScopeRef::Global,
                effect: GrantEffect::Allow,
            });
            grants.push(GrantSnapshot {
                id: format!(
                    "{AWAKEN_WORKSPACE_POLICY_NAMESPACE}:grant:role:{}:tunnel-manage",
                    role.id.0
                ),
                subject: GrantSubjectRef::Role {
                    role_id: qualify_role(&role.id.0).0,
                },
                action_pattern: qualify_action("tunnel.manage").0,
                scope: ScopeRef::Global,
                effect: GrantEffect::Allow,
            });
        } else if role
            .action_patterns
            .iter()
            .any(|pattern| pattern.0 == "workspace.read" || pattern.0 == "workspace.*")
        {
            grants.push(GrantSnapshot {
                id: format!(
                    "{AWAKEN_WORKSPACE_POLICY_NAMESPACE}:grant:role:{}:model-read",
                    role.id.0
                ),
                subject: GrantSubjectRef::Role {
                    role_id: qualify_role(&role.id.0).0,
                },
                action_pattern: qualify_action("model_supply.read").0,
                scope: ScopeRef::Global,
                effect: GrantEffect::Allow,
            });
        }
    }
    for (index, authority) in AGENT_PUBLISHER_AUTHORITIES.iter().copied().enumerate() {
        let pattern = workspace_action_pattern(authority);
        grants.push(GrantSnapshot {
            id: format!("{AWAKEN_WORKSPACE_POLICY_NAMESPACE}:grant:role:publisher:{index}"),
            subject: GrantSubjectRef::Role {
                role_id: AWAKEN_WORKSPACE_PUBLISHER_ROLE.to_owned(),
            },
            action_pattern: qualify_action(pattern).0,
            scope: ScopeRef::Global,
            effect: GrantEffect::Allow,
        });
    }
    for authority in CREDENTIAL_INGRESS_AUTHORITIES {
        let pattern = workspace_action_pattern(*authority);
        grants.push(GrantSnapshot {
            id: format!("{AWAKEN_WORKSPACE_POLICY_NAMESPACE}:grant:role:credential_ingress"),
            subject: GrantSubjectRef::Role {
                role_id: AWAKEN_WORKSPACE_CREDENTIAL_INGRESS_ROLE.to_owned(),
            },
            action_pattern: qualify_action(pattern).0,
            scope: ScopeRef::Global,
            effect: GrantEffect::Allow,
        });
    }
    for (profile_role, role_id, id) in [
        (
            WorkspaceProfileRole::HostedAdmin,
            AWAKEN_WORKSPACE_HOSTED_ADMIN_ROLE,
            "hosted_admin",
        ),
        (
            WorkspaceProfileRole::LegacyHostedAdmin,
            AWAKEN_WORKSPACE_LEGACY_HOSTED_ADMIN_ROLE,
            "legacy_hosted_admin",
        ),
    ] {
        for (index, authority) in workspace_profile_role_authorities(profile_role)
            .iter()
            .enumerate()
        {
            grants.push(GrantSnapshot {
                id: format!("{AWAKEN_WORKSPACE_POLICY_NAMESPACE}:grant:role:{id}:{index}"),
                subject: GrantSubjectRef::Role {
                    role_id: role_id.to_owned(),
                },
                action_pattern: qualify_action(authority.action_pattern()).0,
                scope: ScopeRef::Global,
                effect: GrantEffect::Allow,
            });
        }
        if profile_role == WorkspaceProfileRole::HostedAdmin {
            grants.push(GrantSnapshot {
                id: format!("{AWAKEN_WORKSPACE_POLICY_NAMESPACE}:grant:role:{id}:tunnel-manage"),
                subject: GrantSubjectRef::Role {
                    role_id: role_id.to_owned(),
                },
                action_pattern: qualify_action("tunnel.manage").0,
                scope: ScopeRef::Global,
                effect: GrantEffect::Allow,
            });
        }
    }
    grants.push(GrantSnapshot {
        id: format!("{AWAKEN_WORKSPACE_POLICY_NAMESPACE}:grant:role:tunnel_manager:tunnel-manage"),
        subject: GrantSubjectRef::Role {
            role_id: AWAKEN_WORKSPACE_TUNNEL_MANAGER_ROLE.to_owned(),
        },
        action_pattern: qualify_action("tunnel.manage").0,
        scope: ScopeRef::Global,
        effect: GrantEffect::Allow,
    });

    CreateAuthorizationProfile {
        namespace: NamespaceId(AWAKEN_WORKSPACE_POLICY_NAMESPACE.to_owned()),
        document: AuthorizationProfileDocument {
            resource_model: ResourceModelRegistration {
                actions: action_patterns.into_iter().map(qualify_action).collect(),
                ..ResourceModelRegistration::default()
            },
            action_scope_rules: action_patterns
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

/// Immutable Hosted Run lifecycle authorization contract.
///
/// Awaken owns the `run.*` vocabulary and the roles that may invoke it. A
/// hosting platform owns profile activation and exact Workspace bindings.
pub fn hosted_runtime_authorization_profile() -> CreateAuthorizationProfile {
    let created_at = Timestamp(AUTHORIZATION_PROFILE_EPOCH.to_owned());
    let actions = ["run.create", "run.read", "run.resume", "run.cancel"];
    let mut grants = Vec::new();
    for (profile_role, role_id, id) in [
        (
            HostedRuntimeRole::WorkspaceAdmin,
            HOSTED_RUNTIME_WORKSPACE_ADMIN_ROLE,
            "workspace_admin",
        ),
        (
            HostedRuntimeRole::WorkspaceMember,
            HOSTED_RUNTIME_WORKSPACE_USER_ROLE,
            "workspace_user",
        ),
        (
            HostedRuntimeRole::AgentExecutor,
            HOSTED_RUNTIME_AGENT_EXECUTOR_ROLE,
            "agent_executor",
        ),
    ] {
        for authority in hosted_runtime_role_authorities(profile_role) {
            grants.push(GrantSnapshot {
                id: format!("{HOSTED_RUNTIME_POLICY_NAMESPACE}:grant:role:{id}"),
                subject: GrantSubjectRef::Role {
                    role_id: role_id.to_owned(),
                },
                action_pattern: qualify_hosted_runtime_action(authority.action_pattern()).0,
                scope: ScopeRef::Global,
                effect: GrantEffect::Allow,
            });
        }
    }
    CreateAuthorizationProfile {
        namespace: NamespaceId(HOSTED_RUNTIME_POLICY_NAMESPACE.to_owned()),
        document: AuthorizationProfileDocument {
            resource_model: ResourceModelRegistration {
                actions: actions
                    .into_iter()
                    .map(qualify_hosted_runtime_action)
                    .collect(),
                ..ResourceModelRegistration::default()
            },
            action_scope_rules: actions
                .into_iter()
                .map(|action| ActionScopeRule {
                    action_pattern: qualify_hosted_runtime_action(action).0,
                    allowed_scope_kinds: vec![ScopeKind::Workspace],
                })
                .collect(),
            grants,
            ..AuthorizationProfileDocument::default()
        },
        created_at,
    }
}
