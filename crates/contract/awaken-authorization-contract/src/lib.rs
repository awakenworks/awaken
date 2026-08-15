//! Pure authorization selectors shared by policy construction and route guards.
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
            | Self::WorkspaceModelSupplyRead => None,
        }
    }
}

/// Complete authority set for the credential-ingress role.
pub const CREDENTIAL_INGRESS_AUTHORITIES: &[WorkspaceRoleAuthority] =
    &[WorkspaceRoleAuthority::WorkspaceApiKeyAll];

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
    WorkspaceMember,
    /// Compatibility role used only when migrating the narrower legacy hosted
    /// administrator. Mapping it to `HostedAdmin` would add File/Skill power.
    LegacyHostedAdmin,
}

const HOSTED_ADMIN_AUTHORITIES: &[WorkspaceProfileAuthority] = &[
    WorkspaceProfileAuthority::WorkspaceAll,
    WorkspaceProfileAuthority::ApiKeyAll,
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
        WorkspaceProfileRole::WorkspaceMember => WORKSPACE_MEMBER_AUTHORITIES,
        WorkspaceProfileRole::LegacyHostedAdmin => LEGACY_HOSTED_ADMIN_AUTHORITIES,
    }
}

/// Membership predicate for exhaustive exact-set proofs.
#[must_use]
pub fn workspace_profile_role_contains(
    role: WorkspaceProfileRole,
    authority: WorkspaceProfileAuthority,
) -> bool {
    workspace_profile_role_authorities(role).contains(&authority)
}

/// Finite Hosted Runtime action-pattern vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostedRuntimeAuthority {
    RunAll,
    RunRead,
}

impl HostedRuntimeAuthority {
    #[must_use]
    pub const fn action_pattern(self) -> &'static str {
        match self {
            Self::RunAll => "run.*",
            Self::RunRead => "run.read",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostedRuntimeRole {
    WorkspaceAdmin,
    WorkspaceMember,
    AgentExecutor,
}

const RUNTIME_FULL_AUTHORITIES: &[HostedRuntimeAuthority] = &[HostedRuntimeAuthority::RunAll];
const RUNTIME_MEMBER_AUTHORITIES: &[HostedRuntimeAuthority] = &[HostedRuntimeAuthority::RunRead];

/// Complete, ordered Runtime authority projection consumed by production.
#[must_use]
pub const fn hosted_runtime_role_authorities(
    role: HostedRuntimeRole,
) -> &'static [HostedRuntimeAuthority] {
    match role {
        HostedRuntimeRole::WorkspaceAdmin | HostedRuntimeRole::AgentExecutor => {
            RUNTIME_FULL_AUTHORITIES
        }
        HostedRuntimeRole::WorkspaceMember => RUNTIME_MEMBER_AUTHORITIES,
    }
}

#[must_use]
pub fn hosted_runtime_role_contains(
    role: HostedRuntimeRole,
    authority: HostedRuntimeAuthority,
) -> bool {
    hosted_runtime_role_authorities(role).contains(&authority)
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceBindingMigrationState {
    Legacy {
        role: WorkspaceBindingRole,
        has_management_binding: bool,
        has_resource_binding: bool,
    },
    Canonical(WorkspaceBindingRole),
}

/// Pure migration transition. Canonical states and unsafe/incomplete legacy
/// pairs are fixed points; exact-authority legacy sets converge in one step.
#[must_use]
pub const fn migrate_workspace_binding_state(
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

#[cfg(kani)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_and_grant_spellings_are_exact() {
        // String constants are asserted concretely instead of symbolically:
        // keeping string machinery outside CBMC makes the enum proofs fast.
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
            hosted_runtime_role_authorities(HostedRuntimeRole::WorkspaceMember)
                .iter()
                .map(|authority| authority.action_pattern())
                .collect::<Vec<_>>(),
            ["run.read"]
        );
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    fn symbolic_access() -> RouteAccess {
        if kani::any() {
            RouteAccess::Read
        } else {
            RouteAccess::Write
        }
    }

    fn symbolic_authority() -> WorkspaceRoleAuthority {
        match kani::any::<u8>() % 4 {
            0 => WorkspaceRoleAuthority::WorkspaceConfigurationAll,
            1 => WorkspaceRoleAuthority::WorkspaceApiKeyAll,
            2 => WorkspaceRoleAuthority::WorkspaceModelSupplyRead,
            _ => WorkspaceRoleAuthority::WorkspaceSkillAll,
        }
    }

    fn symbolic_workspace_profile_authority() -> WorkspaceProfileAuthority {
        match kani::any::<u8>() % 10 {
            0 => WorkspaceProfileAuthority::WorkspaceAll,
            1 => WorkspaceProfileAuthority::WorkspaceRead,
            2 => WorkspaceProfileAuthority::ApiKeyAll,
            3 => WorkspaceProfileAuthority::ApiKeyRead,
            4 => WorkspaceProfileAuthority::ModelSupplyAll,
            5 => WorkspaceProfileAuthority::ModelSupplyRead,
            6 => WorkspaceProfileAuthority::FileAll,
            7 => WorkspaceProfileAuthority::FileRead,
            8 => WorkspaceProfileAuthority::SkillAll,
            _ => WorkspaceProfileAuthority::SkillRead,
        }
    }

    fn symbolic_binding_role() -> WorkspaceBindingRole {
        match kani::any::<u8>() % 13 {
            0 => WorkspaceBindingRole::Admin,
            1 => WorkspaceBindingRole::Developer,
            2 => WorkspaceBindingRole::Billing,
            3 => WorkspaceBindingRole::User,
            4 => WorkspaceBindingRole::ClaudeCodeUser,
            5 => WorkspaceBindingRole::WorkspaceAdmin,
            6 => WorkspaceBindingRole::WorkspaceDeveloper,
            7 => WorkspaceBindingRole::WorkspaceRestrictedDeveloper,
            8 => WorkspaceBindingRole::WorkspaceUser,
            9 => WorkspaceBindingRole::WorkspaceBilling,
            10 => WorkspaceBindingRole::AgentPublisher,
            11 => WorkspaceBindingRole::CredentialIngress,
            _ => WorkspaceBindingRole::HostedWorkspaceAdmin,
        }
    }

    #[kani::proof]
    fn run_backed_route_policy_uses_exact_run_actions() {
        let access = symbolic_access();
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

    #[kani::proof]
    fn application_route_policy_never_enters_the_service_guard() {
        assert_eq!(
            application_route_policy(symbolic_access()),
            RouteGuardSelection::Application
        );
    }

    #[kani::proof]
    fn credential_ingress_role_contains_only_workspace_apikey_authority() {
        let authority = symbolic_authority();
        assert_eq!(
            credential_ingress_role_contains(authority),
            authority == WorkspaceRoleAuthority::WorkspaceApiKeyAll
        );
    }

    #[kani::proof]
    fn agent_publisher_role_contains_only_workspace_model_read_and_skill_authority() {
        let authority = symbolic_authority();
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

    #[kani::proof]
    fn hosted_admin_role_contains_exactly_its_five_workspace_authorities() {
        let authority = symbolic_workspace_profile_authority();
        assert_eq!(
            workspace_profile_role_contains(WorkspaceProfileRole::HostedAdmin, authority),
            matches!(
                authority,
                WorkspaceProfileAuthority::WorkspaceAll
                    | WorkspaceProfileAuthority::ApiKeyAll
                    | WorkspaceProfileAuthority::ModelSupplyRead
                    | WorkspaceProfileAuthority::FileAll
                    | WorkspaceProfileAuthority::SkillAll
            )
        );
    }

    #[kani::proof]
    fn workspace_member_role_contains_exactly_read_only_workspace_authorities() {
        let authority = symbolic_workspace_profile_authority();
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

    #[kani::proof]
    fn runtime_member_role_contains_exactly_run_read() {
        let authority = if kani::any() {
            HostedRuntimeAuthority::RunAll
        } else {
            HostedRuntimeAuthority::RunRead
        };
        assert_eq!(
            hosted_runtime_role_contains(HostedRuntimeRole::WorkspaceMember, authority),
            authority == HostedRuntimeAuthority::RunRead
        );
    }

    #[kani::proof]
    fn legacy_workspace_binding_migration_is_idempotent_and_authority_exact() {
        let state = WorkspaceBindingMigrationState::Legacy {
            role: symbolic_binding_role(),
            has_management_binding: kani::any(),
            has_resource_binding: kani::any(),
        };
        let migrated = migrate_workspace_binding_state(state);
        assert_eq!(
            migration_state_authority_mask(migrated),
            migration_state_authority_mask(state)
        );
        assert_eq!(migrate_workspace_binding_state(migrated), migrated);
    }
}
