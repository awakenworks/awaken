// @formal entrypoint authz.agent_publisher_role_is_exact_workspace_agent_model_read_and_skill_only
// @formal entrypoint authz.application_protocol_is_scoped_by_application_guard_not_service_guard
// @formal entrypoint authz.credential_ingress_role_is_exact_workspace_apikey_and_provider_connect_only
// @formal entrypoint authz.hosted_workspace_and_runtime_member_grants_are_exact
// @formal entrypoint authz.legacy_workspace_binding_migration_is_authority_preserving_and_idempotent
// @formal entrypoint authz.run_backed_protocol_routes_use_the_exact_run_action_namespace
//! Pure authorization selectors shared by policy construction and route guards.
//!
//! Owned by `awaken-control`, their only consumer: hosted-platform role
//! vocabulary is a product concern, not a neutral contract (ADR-0081 §1).
//!
//! This leaf deliberately contains no HTTP framework or IAM implementation. It
//! turns finite product-owned policy choices into exact actions, which keeps the
//! executable selector small enough for exhaustive bounded verification.

/// Whether a request observes state or may change it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteAccess {
    Read,
    Write,
}

/// Product-owned actions used by the hosted Run route family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthorizationAction {
    RunRead,
    RunCreate,
    WorkspaceRead,
    WorkspaceWrite,
}

impl AuthorizationAction {
    /// Exact IAM action spelling consumed by the production policy engine.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RunRead => "run.read",
            Self::RunCreate => "run.create",
            Self::WorkspaceRead => "workspace.read",
            Self::WorkspaceWrite => "workspace.write",
        }
    }
}

/// Guard selected for a product route family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteGuardSelection {
    /// The application-token guard owns this family. A service guard must fail
    /// closed if composition accidentally sends the request there.
    Application,
    /// The hosted service guard checks the Run action while an embedded
    /// deployment checks the corresponding Workspace action.
    HostedRuntime {
        action: AuthorizationAction,
        embedded_action: AuthorizationAction,
    },
}

/// Select the exact hosted and embedded actions for a Run-backed route.
#[must_use]
pub const fn run_backed_route_policy(access: RouteAccess) -> RouteGuardSelection {
    match access {
        RouteAccess::Read => RouteGuardSelection::HostedRuntime {
            action: AuthorizationAction::RunRead,
            embedded_action: AuthorizationAction::WorkspaceRead,
        },
        RouteAccess::Write => RouteGuardSelection::HostedRuntime {
            action: AuthorizationAction::RunCreate,
            embedded_action: AuthorizationAction::WorkspaceWrite,
        },
    }
}

/// Select the application guard independently of the HTTP method.
#[must_use]
pub const fn application_route_policy(_access: RouteAccess) -> RouteGuardSelection {
    RouteGuardSelection::Application
}

/// Finite workspace-scoped authorities assigned to product integration roles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceRoleAuthority {
    WorkspaceConfigurationAll,
    WorkspaceApiKeyAll,
    WorkspaceModelSupplyRead,
    WorkspaceModelSupplyConnect,
    WorkspaceSkillAll,
}

impl WorkspaceRoleAuthority {
    /// Management-policy action pattern, when this authority belongs there.
    #[must_use]
    pub const fn management_action_pattern(self) -> Option<&'static str> {
        match self {
            Self::WorkspaceConfigurationAll => Some("workspace.*"),
            Self::WorkspaceApiKeyAll => Some("apikey.*"),
            Self::WorkspaceModelSupplyRead => Some("model_supply.read"),
            Self::WorkspaceModelSupplyConnect => Some("model_supply.connect"),
            Self::WorkspaceSkillAll => None,
        }
    }

    /// Resource-policy action pattern, when this authority belongs there.
    #[must_use]
    pub const fn resource_action_pattern(self) -> Option<&'static str> {
        match self {
            Self::WorkspaceSkillAll => Some("skill.*"),
            Self::WorkspaceConfigurationAll
            | Self::WorkspaceApiKeyAll
            | Self::WorkspaceModelSupplyRead
            | Self::WorkspaceModelSupplyConnect => None,
        }
    }
}

/// Complete authority set for the credential-ingress role.
pub const CREDENTIAL_INGRESS_AUTHORITIES: &[WorkspaceRoleAuthority] = &[
    WorkspaceRoleAuthority::WorkspaceApiKeyAll,
    WorkspaceRoleAuthority::WorkspaceModelSupplyConnect,
];

/// Complete authority set for the agent-publisher role across both policy
/// namespaces.
pub const AGENT_PUBLISHER_AUTHORITIES: &[WorkspaceRoleAuthority] = &[
    WorkspaceRoleAuthority::WorkspaceConfigurationAll,
    WorkspaceRoleAuthority::WorkspaceModelSupplyRead,
    WorkspaceRoleAuthority::WorkspaceSkillAll,
];

/// Membership predicate used when constructing credential-ingress grants.
#[must_use]
pub fn credential_ingress_role_contains(authority: WorkspaceRoleAuthority) -> bool {
    CREDENTIAL_INGRESS_AUTHORITIES.contains(&authority)
}

/// Membership predicate used when constructing agent-publisher grants.
#[must_use]
pub fn agent_publisher_role_contains(authority: WorkspaceRoleAuthority) -> bool {
    AGENT_PUBLISHER_AUTHORITIES.contains(&authority)
}

/// Finite action-pattern vocabulary owned by the unified Workspace profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum WorkspaceProfileAuthority {
    WorkspaceAll,
    WorkspaceRead,
    ApiKeyAll,
    ApiKeyRead,
    ModelSupplyAll,
    ModelSupplyRead,
    ModelSupplyConnect,
    ModelSupplyWrite,
    FileAll,
    FileRead,
    SkillAll,
    SkillRead,
}

impl WorkspaceProfileAuthority {
    #[must_use]
    pub const fn action_pattern(self) -> &'static str {
        match self {
            Self::WorkspaceAll => "workspace.*",
            Self::WorkspaceRead => "workspace.read",
            Self::ApiKeyAll => "apikey.*",
            Self::ApiKeyRead => "apikey.read",
            Self::ModelSupplyAll => "model_supply.*",
            Self::ModelSupplyRead => "model_supply.read",
            Self::ModelSupplyConnect => "model_supply.connect",
            Self::ModelSupplyWrite => "model_supply.write",
            Self::FileAll => "file.*",
            Self::FileRead => "file.read",
            Self::SkillAll => "skill.*",
            Self::SkillRead => "skill.read",
        }
    }

    const fn bit(self) -> u16 {
        1 << self as u8
    }
}

/// Product roles whose authority is not delegated to the external IAM preset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceProfileRole {
    HostedAdmin,
    HostedBuilder,
    WorkspaceMember,
    /// Compatibility role used only when migrating the narrower legacy hosted
    /// administrator. Mapping it to `HostedAdmin` would add File/Skill power.
    LegacyHostedAdmin,
}

const HOSTED_ADMIN_AUTHORITIES: &[WorkspaceProfileAuthority] = &[
    WorkspaceProfileAuthority::WorkspaceAll,
    WorkspaceProfileAuthority::ApiKeyAll,
    WorkspaceProfileAuthority::ModelSupplyRead,
    WorkspaceProfileAuthority::ModelSupplyConnect,
    WorkspaceProfileAuthority::ModelSupplyWrite,
    WorkspaceProfileAuthority::FileAll,
    WorkspaceProfileAuthority::SkillAll,
];
const HOSTED_BUILDER_AUTHORITIES: &[WorkspaceProfileAuthority] = &[
    WorkspaceProfileAuthority::WorkspaceAll,
    WorkspaceProfileAuthority::ModelSupplyRead,
    WorkspaceProfileAuthority::FileAll,
    WorkspaceProfileAuthority::SkillAll,
];
const WORKSPACE_MEMBER_AUTHORITIES: &[WorkspaceProfileAuthority] = &[
    WorkspaceProfileAuthority::FileRead,
    WorkspaceProfileAuthority::SkillRead,
    WorkspaceProfileAuthority::WorkspaceRead,
    WorkspaceProfileAuthority::ModelSupplyRead,
];
const LEGACY_HOSTED_ADMIN_AUTHORITIES: &[WorkspaceProfileAuthority] = &[
    WorkspaceProfileAuthority::WorkspaceAll,
    WorkspaceProfileAuthority::ApiKeyAll,
    WorkspaceProfileAuthority::ModelSupplyRead,
];

/// Complete, ordered authority projection consumed by the production profile.
#[must_use]
pub const fn workspace_profile_role_authorities(
    role: WorkspaceProfileRole,
) -> &'static [WorkspaceProfileAuthority] {
    match role {
        WorkspaceProfileRole::HostedAdmin => HOSTED_ADMIN_AUTHORITIES,
        WorkspaceProfileRole::HostedBuilder => HOSTED_BUILDER_AUTHORITIES,
        WorkspaceProfileRole::WorkspaceMember => WORKSPACE_MEMBER_AUTHORITIES,
        WorkspaceProfileRole::LegacyHostedAdmin => LEGACY_HOSTED_ADMIN_AUTHORITIES,
    }
}

/// Stable hosted human role identifiers consumed by platform composition.
/// Concrete Workspace scope always belongs to the IAM binding, never the id.
pub const AWAKEN_WORKSPACE_HOSTED_ADMIN_ROLE: &str = "awaken.workspace:hosted_admin";
pub const AWAKEN_WORKSPACE_HOSTED_BUILDER_ROLE: &str = "awaken.workspace:hosted_builder";
pub const AWAKEN_WORKSPACE_USER_ROLE: &str = "awaken.workspace:workspace_user";

/// Membership predicate for exhaustive exact-set proofs.
#[must_use]
pub fn workspace_profile_role_contains(
    role: WorkspaceProfileRole,
    authority: WorkspaceProfileAuthority,
) -> bool {
    workspace_profile_role_authorities(role).contains(&authority)
}

/// Finite role vocabulary accepted by the legacy split-profile migration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceBindingRole {
    Admin,
    Developer,
    Billing,
    User,
    ClaudeCodeUser,
    WorkspaceAdmin,
    WorkspaceDeveloper,
    WorkspaceRestrictedDeveloper,
    WorkspaceUser,
    WorkspaceBilling,
    AgentPublisher,
    CredentialIngress,
    HostedWorkspaceAdmin,
}

impl WorkspaceBindingRole {
    /// Canonical local role id. The legacy hosted administrator deliberately
    /// lands on a compatibility role with the exact old authority set.
    #[must_use]
    pub const fn canonical_local_name(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Developer => "developer",
            Self::Billing => "billing",
            Self::User => "user",
            Self::ClaudeCodeUser => "claude_code_user",
            Self::WorkspaceAdmin => "workspace_admin",
            Self::WorkspaceDeveloper => "workspace_developer",
            Self::WorkspaceRestrictedDeveloper => "workspace_restricted_developer",
            Self::WorkspaceUser => "workspace_user",
            Self::WorkspaceBilling => "workspace_billing",
            Self::AgentPublisher => "publisher",
            Self::CredentialIngress => "credential_ingress",
            Self::HostedWorkspaceAdmin => "legacy_hosted_admin",
        }
    }
}

const fn authority_mask(authorities: &[WorkspaceProfileAuthority]) -> u16 {
    let mut mask = 0;
    let mut index = 0;
    while index < authorities.len() {
        mask |= authorities[index].bit();
        index += 1;
    }
    mask
}

const fn legacy_management_authority_mask(role: WorkspaceBindingRole) -> u16 {
    use WorkspaceProfileAuthority as A;
    match role {
        WorkspaceBindingRole::Admin | WorkspaceBindingRole::WorkspaceAdmin => {
            A::WorkspaceAll.bit() | A::ApiKeyAll.bit() | A::ModelSupplyAll.bit()
        }
        WorkspaceBindingRole::Developer => A::ApiKeyAll.bit(),
        WorkspaceBindingRole::User => A::WorkspaceRead.bit() | A::ModelSupplyRead.bit(),
        WorkspaceBindingRole::WorkspaceDeveloper => {
            A::WorkspaceRead.bit() | A::ApiKeyAll.bit() | A::ModelSupplyRead.bit()
        }
        WorkspaceBindingRole::WorkspaceRestrictedDeveloper => {
            A::WorkspaceRead.bit() | A::ApiKeyRead.bit() | A::ModelSupplyRead.bit()
        }
        WorkspaceBindingRole::WorkspaceUser => A::WorkspaceRead.bit() | A::ModelSupplyRead.bit(),
        WorkspaceBindingRole::AgentPublisher => A::WorkspaceAll.bit() | A::ModelSupplyRead.bit(),
        WorkspaceBindingRole::CredentialIngress => A::ApiKeyAll.bit(),
        WorkspaceBindingRole::HostedWorkspaceAdmin => {
            authority_mask(LEGACY_HOSTED_ADMIN_AUTHORITIES)
        }
        WorkspaceBindingRole::Billing
        | WorkspaceBindingRole::ClaudeCodeUser
        | WorkspaceBindingRole::WorkspaceBilling => 0,
    }
}

const fn legacy_resource_authority_mask(role: WorkspaceBindingRole) -> u16 {
    use WorkspaceProfileAuthority as A;
    match role {
        WorkspaceBindingRole::Admin | WorkspaceBindingRole::WorkspaceAdmin => {
            A::WorkspaceAll.bit() | A::FileAll.bit() | A::SkillAll.bit()
        }
        WorkspaceBindingRole::User => A::WorkspaceRead.bit(),
        WorkspaceBindingRole::WorkspaceDeveloper
        | WorkspaceBindingRole::WorkspaceRestrictedDeveloper => {
            A::WorkspaceRead.bit() | A::FileAll.bit() | A::SkillAll.bit()
        }
        WorkspaceBindingRole::WorkspaceUser => {
            A::WorkspaceRead.bit() | A::FileRead.bit() | A::SkillRead.bit()
        }
        WorkspaceBindingRole::AgentPublisher => A::SkillAll.bit(),
        WorkspaceBindingRole::Developer
        | WorkspaceBindingRole::Billing
        | WorkspaceBindingRole::ClaudeCodeUser
        | WorkspaceBindingRole::WorkspaceBilling
        | WorkspaceBindingRole::CredentialIngress
        | WorkspaceBindingRole::HostedWorkspaceAdmin => 0,
    }
}

const fn canonical_authority_mask(role: WorkspaceBindingRole) -> u16 {
    if matches!(role, WorkspaceBindingRole::CredentialIngress) {
        let mut mask = 0;
        let mut index = 0;
        while index < CREDENTIAL_INGRESS_AUTHORITIES.len() {
            mask |= match CREDENTIAL_INGRESS_AUTHORITIES[index] {
                WorkspaceRoleAuthority::WorkspaceConfigurationAll => {
                    WorkspaceProfileAuthority::WorkspaceAll.bit()
                }
                WorkspaceRoleAuthority::WorkspaceApiKeyAll => {
                    WorkspaceProfileAuthority::ApiKeyAll.bit()
                }
                WorkspaceRoleAuthority::WorkspaceModelSupplyRead => {
                    WorkspaceProfileAuthority::ModelSupplyRead.bit()
                }
                WorkspaceRoleAuthority::WorkspaceModelSupplyConnect => {
                    WorkspaceProfileAuthority::ModelSupplyConnect.bit()
                }
                WorkspaceRoleAuthority::WorkspaceSkillAll => {
                    WorkspaceProfileAuthority::SkillAll.bit()
                }
            };
            index += 1;
        }
        return mask;
    }
    legacy_management_authority_mask(role) | legacy_resource_authority_mask(role)
}

/// Select a canonical replacement only when its complete authority set equals
/// the union of the legacy bindings that actually exist. `None` means the
/// legacy rows and profiles must remain active for a lossless, fail-closed
/// operator-assisted migration.
#[must_use]
pub const fn legacy_workspace_binding_migration_target(
    role: WorkspaceBindingRole,
    has_management_binding: bool,
    has_resource_binding: bool,
) -> Option<WorkspaceBindingRole> {
    let source = (if has_management_binding {
        legacy_management_authority_mask(role)
    } else {
        0
    }) | (if has_resource_binding {
        legacy_resource_authority_mask(role)
    } else {
        0
    });
    if (has_management_binding || has_resource_binding) && source == canonical_authority_mask(role)
    {
        Some(role)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_ingress_connect_is_exact_and_not_a_legacy_migration_grant() {
        // C1 current ingress role -> E1 API keys plus exact connect, no wildcard.
        // C2 legacy management/resource binding presence -> E2 no automatic
        // upgrade: none of the four legacy combinations includes connect.
        assert_eq!(
            CREDENTIAL_INGRESS_AUTHORITIES
                .iter()
                .map(|a| a.management_action_pattern())
                .collect::<Vec<_>>(),
            [Some("apikey.*"), Some("model_supply.connect")]
        );
        for management in [false, true] {
            for resources in [false, true] {
                assert_eq!(
                    legacy_workspace_binding_migration_target(
                        WorkspaceBindingRole::CredentialIngress,
                        management,
                        resources
                    ),
                    None
                );
            }
        }
    }

    #[test]
    fn action_and_grant_spellings_are_exact() {
        // Cause/effect decision table for the human role contract:
        // Viewer -> read-only authorities; Builder -> Workspace/File/Skill
        // writes plus model read and no API keys; Administrator -> Builder plus
        // API keys. Exact stable ids are the only values a host may compose.
        // String constants stay outside CBMC so the finite enum proofs are fast.
        assert_eq!(AuthorizationAction::RunRead.as_str(), "run.read");
        assert_eq!(AuthorizationAction::RunCreate.as_str(), "run.create");
        assert_eq!(
            AuthorizationAction::WorkspaceRead.as_str(),
            "workspace.read"
        );
        assert_eq!(
            AuthorizationAction::WorkspaceWrite.as_str(),
            "workspace.write"
        );
        assert_eq!(
            WorkspaceRoleAuthority::WorkspaceConfigurationAll.management_action_pattern(),
            Some("workspace.*")
        );
        assert_eq!(
            WorkspaceRoleAuthority::WorkspaceApiKeyAll.management_action_pattern(),
            Some("apikey.*")
        );
        assert_eq!(
            WorkspaceRoleAuthority::WorkspaceModelSupplyRead.management_action_pattern(),
            Some("model_supply.read")
        );
        assert_eq!(
            WorkspaceRoleAuthority::WorkspaceSkillAll.resource_action_pattern(),
            Some("skill.*")
        );
        assert_eq!(
            workspace_profile_role_authorities(WorkspaceProfileRole::WorkspaceMember)
                .iter()
                .map(|authority| authority.action_pattern())
                .collect::<Vec<_>>(),
            [
                "file.read",
                "skill.read",
                "workspace.read",
                "model_supply.read"
            ]
        );
        assert_eq!(
            workspace_profile_role_authorities(WorkspaceProfileRole::HostedBuilder)
                .iter()
                .map(|authority| authority.action_pattern())
                .collect::<Vec<_>>(),
            ["workspace.*", "model_supply.read", "file.*", "skill.*"]
        );
        assert_eq!(
            AWAKEN_WORKSPACE_HOSTED_BUILDER_ROLE,
            "awaken.workspace:hosted_builder"
        );
    }
}

// Every input below is a closed enum or a bool, so each test enumerates its whole
// domain. Each `every_*` list is checked against a total `match` (no `_` arm):
// a new variant fails to compile until it is listed.
#[cfg(test)]
mod exhaustive {
    use super::*;

    // The legacy binding migration is decided by the production selector
    // `legacy_workspace_binding_migration_target`. This private state machine
    // exists only to state its safety law: applying the selector preserves the
    // authority mask and is a fixed point on its own result.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum WorkspaceBindingMigrationState {
        Legacy {
            role: WorkspaceBindingRole,
            has_management_binding: bool,
            has_resource_binding: bool,
        },
        Canonical(WorkspaceBindingRole),
    }

    /// Pure migration transition. Canonical states and unsafe/incomplete legacy
    /// pairs are fixed points; exact-authority legacy sets converge in one step.
    const fn migrate_workspace_binding_state(
        state: WorkspaceBindingMigrationState,
    ) -> WorkspaceBindingMigrationState {
        match state {
            WorkspaceBindingMigrationState::Canonical(_) => state,
            WorkspaceBindingMigrationState::Legacy {
                role,
                has_management_binding,
                has_resource_binding,
            } => match legacy_workspace_binding_migration_target(
                role,
                has_management_binding,
                has_resource_binding,
            ) {
                Some(role) => WorkspaceBindingMigrationState::Canonical(role),
                None => state,
            },
        }
    }

    const fn migration_state_authority_mask(state: WorkspaceBindingMigrationState) -> u16 {
        match state {
            WorkspaceBindingMigrationState::Legacy {
                role,
                has_management_binding,
                has_resource_binding,
            } => {
                (if has_management_binding {
                    legacy_management_authority_mask(role)
                } else {
                    0
                }) | (if has_resource_binding {
                    legacy_resource_authority_mask(role)
                } else {
                    0
                })
            }
            WorkspaceBindingMigrationState::Canonical(role) => canonical_authority_mask(role),
        }
    }

    const fn listed_access(access: RouteAccess) -> usize {
        match access {
            RouteAccess::Read => 0,
            RouteAccess::Write => 1,
        }
    }
    const EVERY_ACCESS: [RouteAccess; 2] = [RouteAccess::Read, RouteAccess::Write];

    const fn listed_authority(authority: WorkspaceRoleAuthority) -> usize {
        match authority {
            WorkspaceRoleAuthority::WorkspaceConfigurationAll => 0,
            WorkspaceRoleAuthority::WorkspaceApiKeyAll => 1,
            WorkspaceRoleAuthority::WorkspaceModelSupplyRead => 2,
            WorkspaceRoleAuthority::WorkspaceModelSupplyConnect => 3,
            WorkspaceRoleAuthority::WorkspaceSkillAll => 4,
        }
    }
    const EVERY_AUTHORITY: [WorkspaceRoleAuthority; 5] = [
        WorkspaceRoleAuthority::WorkspaceConfigurationAll,
        WorkspaceRoleAuthority::WorkspaceApiKeyAll,
        WorkspaceRoleAuthority::WorkspaceModelSupplyRead,
        WorkspaceRoleAuthority::WorkspaceModelSupplyConnect,
        WorkspaceRoleAuthority::WorkspaceSkillAll,
    ];

    const fn listed_profile_authority(authority: WorkspaceProfileAuthority) -> usize {
        match authority {
            WorkspaceProfileAuthority::WorkspaceAll => 0,
            WorkspaceProfileAuthority::WorkspaceRead => 1,
            WorkspaceProfileAuthority::ApiKeyAll => 2,
            WorkspaceProfileAuthority::ApiKeyRead => 3,
            WorkspaceProfileAuthority::ModelSupplyAll => 4,
            WorkspaceProfileAuthority::ModelSupplyRead => 5,
            WorkspaceProfileAuthority::FileAll => 6,
            WorkspaceProfileAuthority::FileRead => 7,
            WorkspaceProfileAuthority::SkillAll => 8,
            WorkspaceProfileAuthority::SkillRead => 9,
            WorkspaceProfileAuthority::ModelSupplyConnect => 10,
            WorkspaceProfileAuthority::ModelSupplyWrite => 11,
        }
    }
    const EVERY_PROFILE_AUTHORITY: [WorkspaceProfileAuthority; 12] = [
        WorkspaceProfileAuthority::WorkspaceAll,
        WorkspaceProfileAuthority::WorkspaceRead,
        WorkspaceProfileAuthority::ApiKeyAll,
        WorkspaceProfileAuthority::ApiKeyRead,
        WorkspaceProfileAuthority::ModelSupplyAll,
        WorkspaceProfileAuthority::ModelSupplyRead,
        WorkspaceProfileAuthority::FileAll,
        WorkspaceProfileAuthority::FileRead,
        WorkspaceProfileAuthority::SkillAll,
        WorkspaceProfileAuthority::SkillRead,
        WorkspaceProfileAuthority::ModelSupplyConnect,
        WorkspaceProfileAuthority::ModelSupplyWrite,
    ];

    const fn listed_binding_role(role: WorkspaceBindingRole) -> usize {
        match role {
            WorkspaceBindingRole::Admin => 0,
            WorkspaceBindingRole::Developer => 1,
            WorkspaceBindingRole::Billing => 2,
            WorkspaceBindingRole::User => 3,
            WorkspaceBindingRole::ClaudeCodeUser => 4,
            WorkspaceBindingRole::WorkspaceAdmin => 5,
            WorkspaceBindingRole::WorkspaceDeveloper => 6,
            WorkspaceBindingRole::WorkspaceRestrictedDeveloper => 7,
            WorkspaceBindingRole::WorkspaceUser => 8,
            WorkspaceBindingRole::WorkspaceBilling => 9,
            WorkspaceBindingRole::AgentPublisher => 10,
            WorkspaceBindingRole::CredentialIngress => 11,
            WorkspaceBindingRole::HostedWorkspaceAdmin => 12,
        }
    }
    const EVERY_BINDING_ROLE: [WorkspaceBindingRole; 13] = [
        WorkspaceBindingRole::Admin,
        WorkspaceBindingRole::Developer,
        WorkspaceBindingRole::Billing,
        WorkspaceBindingRole::User,
        WorkspaceBindingRole::ClaudeCodeUser,
        WorkspaceBindingRole::WorkspaceAdmin,
        WorkspaceBindingRole::WorkspaceDeveloper,
        WorkspaceBindingRole::WorkspaceRestrictedDeveloper,
        WorkspaceBindingRole::WorkspaceUser,
        WorkspaceBindingRole::WorkspaceBilling,
        WorkspaceBindingRole::AgentPublisher,
        WorkspaceBindingRole::CredentialIngress,
        WorkspaceBindingRole::HostedWorkspaceAdmin,
    ];

    #[test]
    fn every_list_names_each_variant_once_in_match_order() {
        for (index, value) in EVERY_ACCESS.into_iter().enumerate() {
            assert_eq!(listed_access(value), index);
        }
        for (index, value) in EVERY_AUTHORITY.into_iter().enumerate() {
            assert_eq!(listed_authority(value), index);
        }
        for (index, value) in EVERY_PROFILE_AUTHORITY.into_iter().enumerate() {
            assert_eq!(listed_profile_authority(value), index);
        }
        for (index, value) in EVERY_BINDING_ROLE.into_iter().enumerate() {
            assert_eq!(listed_binding_role(value), index);
        }
    }

    // Hosted Runtime and Workspace role vocabulary is owned by its only consumer.
    /// @exhaustive RouteAccess(2) = 2
    /// @obligation authz.run_backed_protocol_routes_use_the_exact_run_action_namespace
    #[test]
    fn run_backed_route_policy_uses_exact_run_actions() {
        for access in EVERY_ACCESS {
            let selected = run_backed_route_policy(access);
            match (access, selected) {
                (
                    RouteAccess::Read,
                    RouteGuardSelection::HostedRuntime {
                        action,
                        embedded_action,
                    },
                ) => {
                    assert_eq!(action, AuthorizationAction::RunRead);
                    assert_eq!(embedded_action, AuthorizationAction::WorkspaceRead);
                }
                (
                    RouteAccess::Write,
                    RouteGuardSelection::HostedRuntime {
                        action,
                        embedded_action,
                    },
                ) => {
                    assert_eq!(action, AuthorizationAction::RunCreate);
                    assert_eq!(embedded_action, AuthorizationAction::WorkspaceWrite);
                }
                (_, RouteGuardSelection::Application) => unreachable!(),
            }
        }
    }

    /// @exhaustive RouteAccess(2) = 2
    /// @obligation authz.application_protocol_is_scoped_by_application_guard_not_service_guard
    #[test]
    fn application_route_policy_never_enters_the_service_guard() {
        for access in EVERY_ACCESS {
            assert_eq!(
                application_route_policy(access),
                RouteGuardSelection::Application
            );
        }
    }

    /// @exhaustive WorkspaceRoleAuthority(5) = 5
    /// @obligation authz.credential_ingress_role_is_exact_workspace_apikey_and_provider_connect_only
    #[test]
    fn credential_ingress_role_contains_only_apikey_and_provider_connect_authority() {
        for authority in EVERY_AUTHORITY {
            assert_eq!(
                credential_ingress_role_contains(authority),
                matches!(
                    authority,
                    WorkspaceRoleAuthority::WorkspaceApiKeyAll
                        | WorkspaceRoleAuthority::WorkspaceModelSupplyConnect
                )
            );
        }
    }

    /// @exhaustive WorkspaceRoleAuthority(5) = 5
    /// @obligation authz.agent_publisher_role_is_exact_workspace_agent_model_read_and_skill_only
    #[test]
    fn agent_publisher_role_contains_only_workspace_model_read_and_skill_authority() {
        for authority in EVERY_AUTHORITY {
            assert_eq!(
                agent_publisher_role_contains(authority),
                matches!(
                    authority,
                    WorkspaceRoleAuthority::WorkspaceConfigurationAll
                        | WorkspaceRoleAuthority::WorkspaceModelSupplyRead
                        | WorkspaceRoleAuthority::WorkspaceSkillAll
                )
            );
        }
    }

    /// @exhaustive WorkspaceProfileAuthority(12) = 12
    /// @obligation authz.hosted_workspace_and_runtime_member_grants_are_exact
    #[test]
    fn hosted_admin_role_contains_exactly_its_seven_workspace_authorities() {
        for authority in EVERY_PROFILE_AUTHORITY {
            assert_eq!(
                workspace_profile_role_contains(WorkspaceProfileRole::HostedAdmin, authority),
                matches!(
                    authority,
                    WorkspaceProfileAuthority::WorkspaceAll
                        | WorkspaceProfileAuthority::ApiKeyAll
                        | WorkspaceProfileAuthority::ModelSupplyRead
                        | WorkspaceProfileAuthority::ModelSupplyConnect
                        | WorkspaceProfileAuthority::ModelSupplyWrite
                        | WorkspaceProfileAuthority::FileAll
                        | WorkspaceProfileAuthority::SkillAll
                )
            );
        }
    }

    /// @exhaustive WorkspaceProfileAuthority(12) = 12
    /// @obligation authz.hosted_workspace_and_runtime_member_grants_are_exact
    #[test]
    fn hosted_builder_role_has_writes_without_apikey_or_model_administration() {
        for authority in EVERY_PROFILE_AUTHORITY {
            assert_eq!(
                workspace_profile_role_contains(WorkspaceProfileRole::HostedBuilder, authority),
                matches!(
                    authority,
                    WorkspaceProfileAuthority::WorkspaceAll
                        | WorkspaceProfileAuthority::ModelSupplyRead
                        | WorkspaceProfileAuthority::FileAll
                        | WorkspaceProfileAuthority::SkillAll
                )
            );
        }
    }

    /// @exhaustive WorkspaceProfileAuthority(12) = 12
    /// @obligation authz.hosted_workspace_and_runtime_member_grants_are_exact
    #[test]
    fn workspace_member_role_contains_exactly_read_only_workspace_authorities() {
        for authority in EVERY_PROFILE_AUTHORITY {
            assert_eq!(
                workspace_profile_role_contains(WorkspaceProfileRole::WorkspaceMember, authority),
                matches!(
                    authority,
                    WorkspaceProfileAuthority::WorkspaceRead
                        | WorkspaceProfileAuthority::ModelSupplyRead
                        | WorkspaceProfileAuthority::FileRead
                        | WorkspaceProfileAuthority::SkillRead
                )
            );
        }
    }

    /// @exhaustive WorkspaceBindingRole(13) x bool(2) x bool(2) = 52
    /// @obligation authz.legacy_workspace_binding_migration_is_authority_preserving_and_idempotent
    #[test]
    fn legacy_workspace_binding_migration_is_idempotent_and_authority_exact() {
        for role in EVERY_BINDING_ROLE {
            for has_management_binding in [false, true] {
                for has_resource_binding in [false, true] {
                    let state = WorkspaceBindingMigrationState::Legacy {
                        role,
                        has_management_binding,
                        has_resource_binding,
                    };
                    let migrated = migrate_workspace_binding_state(state);
                    assert_eq!(
                        migration_state_authority_mask(migrated),
                        migration_state_authority_mask(state)
                    );
                    assert_eq!(migrate_workspace_binding_state(migrated), migrated);
                }
            }
        }
    }
}
