//! Hosted route-profile projection from the authorization route authority.

use std::num::NonZeroU64;

use super::ROUTE_POLICIES;

/// Canonical flat route classification retained beside its IAM policy.
///
/// The public hosted profile derives both ingress spellings from this one
/// value. It deliberately does not store a second workspace-prefixed path.
#[derive(Debug, Clone, Copy)]
pub(super) enum HostedRuntimeRouteDescriptor {
    PathPrefix(&'static str),
    PathTemplate(&'static str),
}

impl HostedRuntimeRouteDescriptor {
    fn export(self) -> [HostedRuntimePathMatch; 2] {
        let flat = match self {
            Self::PathPrefix(path) => HostedRuntimePathMatch::PathPrefix {
                path: path.to_owned(),
            },
            Self::PathTemplate(path_template) => HostedRuntimePathMatch::PathTemplate {
                path_template: path_template.to_owned(),
            },
        };
        let canonical = match self {
            Self::PathPrefix(path) | Self::PathTemplate(path) => path,
        };
        let suffix = canonical
            .strip_prefix("/v1")
            .expect("hosted runtime route descriptors are canonical /v1 paths");
        let workspace = HostedRuntimePathMatch::PathTemplate {
            path_template: format!("/v1/workspaces/{{workspace_id}}{suffix}"),
        };
        [flat, workspace]
    }
}

/// One path matcher in the split-hosted Control-to-Coordinator facade.
///
/// `PathPrefix` maps directly to prefix-routing gateways. `PathTemplate` keeps
/// a shared family exact: `{name}` denotes one non-empty path segment, so a
/// deployment can compile it to its gateway's native matcher without routing
/// the Control-owned siblings beside it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "match", rename_all = "snake_case")]
pub enum HostedRuntimePathMatch {
    PathPrefix { path: String },
    PathTemplate { path_template: String },
}

/// Deterministic release contract consumed by hosted deployment routing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct HostedRuntimeRouteProfile {
    pub schema_version: u32,
    pub application_access_max_ttl_seconds: NonZeroU64,
    pub routes: Vec<HostedRuntimePathMatch>,
}

/// Project the Coordinator-owned browser surface from the same descriptors
/// that authorize it and the application-access limit supplied by the product
/// composition root. This is deliberately a function rather than a public
/// mutable registry so the Awaken release remains the only route authority and
/// Control does not acquire a dependency on Coordinator.
pub fn hosted_runtime_route_profile(
    application_access_max_ttl_seconds: NonZeroU64,
) -> HostedRuntimeRouteProfile {
    HostedRuntimeRouteProfile {
        schema_version: 2,
        application_access_max_ttl_seconds,
        routes: ROUTE_POLICIES
            .iter()
            .filter_map(|descriptor| descriptor.hosted_runtime_route)
            .flat_map(HostedRuntimeRouteDescriptor::export)
            .collect(),
    }
}
