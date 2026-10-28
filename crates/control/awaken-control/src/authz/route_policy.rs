//! Sole bounded route-family policy table shared by the PEP and Hosted export.

use super::*;

pub(super) const ROUTE_POLICIES: &[RoutePolicyDescriptor] = &[
    RoutePolicyDescriptor::hosted_runtime("/v1/sessions", HOSTED_RUN_POLICY),
    // Fleet readiness is an internal Flow-to-Coordinator read of a
    // deployment-wide projection. It still requires the caller's hosted Run
    // authority, but must not enter the public hosted-runtime route export.
    RoutePolicyDescriptor {
        prefix: "/v1/workers/readiness",
        policy: HOSTED_RUN_POLICY,
        hosted_runtime_route: None,
    },
    RoutePolicyDescriptor::hosted_runtime(
        "/v1/dreams",
        RouteFamilyPolicy::Resource {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::hosted_runtime("/v1/a2a", HOSTED_RUN_POLICY),
    RoutePolicyDescriptor::hosted_runtime("/v1/message:send", HOSTED_RUN_POLICY),
    RoutePolicyDescriptor::hosted_runtime("/v1/message:stream", HOSTED_RUN_POLICY),
    RoutePolicyDescriptor::hosted_runtime("/v1/ai-sdk", RouteFamilyPolicy::Application),
    RoutePolicyDescriptor::hosted_runtime("/v1/ag-ui", RouteFamilyPolicy::Application),
    RoutePolicyDescriptor::hosted_runtime("/v1/durable", HOSTED_RUN_POLICY),
    RoutePolicyDescriptor::hosted_runtime(
        "/v1/files",
        RouteFamilyPolicy::Resource {
            read: FILE_READ,
            write: FILE_WRITE,
        },
    ),
    RoutePolicyDescriptor::hosted_runtime(
        "/v1/awaken/files",
        RouteFamilyPolicy::Resource {
            read: FILE_READ,
            write: FILE_WRITE,
        },
    ),
    RoutePolicyDescriptor::hosted_runtime(
        "/v1/skills",
        RouteFamilyPolicy::Resource {
            read: SKILL_READ,
            write: SKILL_WRITE,
        },
    ),
    RoutePolicyDescriptor::hosted_runtime(
        "/v1/memory_stores",
        RouteFamilyPolicy::Resource {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::hosted_runtime(
        "/v1/models",
        RouteFamilyPolicy::Resource {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::hosted_runtime(
        "/v1/awaken/repositories",
        RouteFamilyPolicy::Resource {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::hosted_runtime("/v1/awaken/sessions", HOSTED_RUN_POLICY),
    RoutePolicyDescriptor::control("/v1/config/iam/tokens", RouteFamilyPolicy::TokenAdmin),
    RoutePolicyDescriptor::hosted_runtime("/v1/application-access-tokens", HOSTED_RUN_POLICY),
    RoutePolicyDescriptor::control(
        "/v1/config/credentials",
        RouteFamilyPolicy::Scoped {
            read: APIKEY_READ,
            write: APIKEY_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/workspace-context",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/credential-pools",
        RouteFamilyPolicy::Scoped {
            read: APIKEY_READ,
            write: APIKEY_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/vaults",
        RouteFamilyPolicy::Scoped {
            read: APIKEY_READ,
            write: APIKEY_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/mcp-servers",
        RouteFamilyPolicy::Scoped {
            read: APIKEY_READ,
            write: APIKEY_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/mcp-catalog",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/telemetry",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/application-mcp-credentials",
        RouteFamilyPolicy::Scoped {
            read: APIKEY_READ,
            write: APIKEY_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/application-credentials",
        RouteFamilyPolicy::Scoped {
            read: APIKEY_READ,
            write: APIKEY_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/provider-connections",
        RouteFamilyPolicy::Scoped {
            read: MODEL_SUPPLY_READ,
            write: MODEL_SUPPLY_CONNECT,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/provider-descriptors",
        RouteFamilyPolicy::Scoped {
            read: MODEL_SUPPLY_READ,
            write: MODEL_SUPPLY_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/executable-models",
        RouteFamilyPolicy::Scoped {
            read: MODEL_SUPPLY_READ,
            write: MODEL_SUPPLY_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/model-attributes",
        RouteFamilyPolicy::Scoped {
            read: MODEL_SUPPLY_READ,
            write: MODEL_SUPPLY_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/catalog",
        RouteFamilyPolicy::Scoped {
            read: MODEL_SUPPLY_READ,
            write: MODEL_SUPPLY_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/capabilities",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/brokered-models",
        RouteFamilyPolicy::Scoped {
            read: MODEL_SUPPLY_READ,
            write: MODEL_SUPPLY_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/inference-profiles",
        RouteFamilyPolicy::Scoped {
            read: MODEL_SUPPLY_READ,
            write: MODEL_SUPPLY_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/inference",
        RouteFamilyPolicy::Scoped {
            read: MODEL_SUPPLY_READ,
            write: MODEL_SUPPLY_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/agents",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/agent-previews",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/publications",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/config/webhook-subscriptions",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/user_profiles",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    // MCP Tunnels are Cloud-owned resources exposed through the canonical
    // Management router.  Keep their public ACL in the same Workspace policy
    // namespace as the injected application boundary: reads may inspect only the
    // authenticated Workspace and every lifecycle operation is a write.  An
    // absent Cloud application still leaves the routes unmounted.
    RoutePolicyDescriptor::control(
        "/v1/tunnels",
        RouteFamilyPolicy::Scoped {
            read: TUNNEL_MANAGE,
            write: TUNNEL_MANAGE,
        },
    ),
    // Deprecated organization-shaped Tunnel wire. The credential remains
    // workspace-bound in Awaken, providing a deterministic migration mapping
    // without selecting an arbitrary Workspace from an Organization.
    RoutePolicyDescriptor::control(
        "/v1/organizations/tunnels",
        RouteFamilyPolicy::Scoped {
            read: TUNNEL_MANAGE,
            write: TUNNEL_MANAGE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/agents",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::hosted_runtime(
        "/v1/deployments",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::hosted_runtime(
        "/v1/deployment_runs",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    // Environment definitions are authored by Control, while only the
    // per-environment Work subtree is execution-owned by Coordinator.
    RoutePolicyDescriptor::control_with_hosted_subtree(
        "/v1/environments",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
        "/v1/environments/{environment_id}/work",
    ),
    RoutePolicyDescriptor::control(
        "/v1/awaken/sandbox-execution-policies",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/awaken/environments",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::hosted_runtime(
        "/v1/awaken/memory-stores",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
    RoutePolicyDescriptor::control(
        "/v1/capabilities",
        RouteFamilyPolicy::Scoped {
            read: WORKSPACE_READ,
            write: WORKSPACE_WRITE,
        },
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_worker_readiness_uses_run_read_without_exporting_fleet_routes() {
        // Cause/effect rules: R1 exact GET readiness + hosted Run authority ->
        // E1 the canonical guard selects run.read at the token Workspace;
        // R2 an unrelated Worker path -> E2 unmapped/denied; R3 registered
        // internal readiness -> E3 no public hosted-runtime route projection.
        // This is one policy table, not a second allowlist in Cloud or Flow.
        use axum::http::Method;
        assert!(matches!(
            action_for(&Method::GET, "/v1/workers/readiness"),
            Some(RouteAuthz::HostedRuntime {
                action: RUN_READ,
                scope: ScopeClass::Workspace,
                ..
            })
        ));
        assert!(action_for(&Method::GET, "/v1/workers/identities").is_none());
        assert!(ROUTE_POLICIES.iter().any(|descriptor| {
            descriptor.prefix == "/v1/workers/readiness"
                && descriptor.hosted_runtime_route.is_none()
        }));
    }

    #[test]
    fn mcp_configuration_routes_keep_catalog_and_credential_permissions_separate() {
        // A1 definition/candidate/probe/default/pin-preview -> existing credential
        // authority; A2 cloud source management -> Workspace configuration authority.
        // No new unauthenticated route or model-supply privilege is implied.
        use axum::http::Method;
        assert!(matches!(
            action_for(&Method::GET, "/v1/config/mcp-servers"),
            Some(RouteAuthz::Scoped {
                action: APIKEY_READ,
                ..
            })
        ));
        for path in [
            "/v1/config/mcp-servers/local:x/inspect",
            "/v1/config/mcp-servers/local:x/prepare",
            "/v1/config/mcp-servers/local:x/default-credential",
        ] {
            assert!(matches!(
                action_for(&Method::POST, path),
                Some(RouteAuthz::Scoped {
                    action: APIKEY_WRITE,
                    ..
                })
            ));
        }
        assert!(matches!(
            action_for(&Method::PUT, "/v1/config/mcp-catalog/source"),
            Some(RouteAuthz::Scoped {
                action: WORKSPACE_WRITE,
                ..
            })
        ));
    }

    #[test]
    fn telemetry_narrowing_is_workspace_configuration() {
        // Reading and tightening the Workspace capture narrowing use the
        // Workspace configuration authority, never a credential or IAM one.
        use axum::http::Method;
        for (method, action) in [
            (Method::GET, WORKSPACE_READ),
            (Method::PUT, WORKSPACE_WRITE),
        ] {
            assert!(matches!(
                action_for(&method, "/v1/config/telemetry"),
                Some(RouteAuthz::Scoped { action: found, .. }) if found == action
            ));
        }
    }
}
