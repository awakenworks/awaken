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
}
