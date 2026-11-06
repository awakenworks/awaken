// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: This source implements the repository-controlled remote worker product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! ACP launch-route selection for sandbox-backed channel sources.
//!
//! This module owns the configuration-plane decision of which registered ACP
//! CLI serves a run and projects that route into one concrete launch. Sandbox
//! realization remains in the parent module.

use std::collections::BTreeMap;
use std::sync::Arc;

use awaken_run_executor_acp::{AcpCli, AcpLaunch, LaunchResolver, OpenError, ResolvedLaunch};
use awaken_runtime_contract::activation::RunActivation;

type ResolvedRoute = (AcpCli, Arc<dyn LaunchResolver>, Option<Vec<String>>);

#[derive(Clone)]
pub(super) struct ProjectedLaunch {
    pub(super) cli: AcpCli,
    resolver: Arc<dyn LaunchResolver>,
    launch_argv: Option<Vec<String>>,
}

/// Exact `acp:<cli>` launch routes installed on one Worker.
#[derive(Clone)]
pub struct AcpLaunchRegistry {
    routes: Arc<BTreeMap<String, ProjectedLaunch>>,
}

impl AcpLaunchRegistry {
    pub fn new(routes: Vec<(AcpCli, Arc<dyn LaunchResolver>)>) -> Result<Self, String> {
        Self::with_resolved_argv(
            routes
                .into_iter()
                .map(|(cli, resolver)| (cli, resolver, None))
                .collect(),
        )
    }

    /// Install only exact catalog routes. A provided acquisition-pinned argv
    /// must be nonempty; `None` keeps the catalog's default launch argv.
    /// Process composition supplies the source; this constructor still owns the
    /// one duplicate/id/argv validation before any Session is served.
    pub fn with_resolved_argv(routes: Vec<ResolvedRoute>) -> Result<Self, String> {
        let mut indexed = BTreeMap::new();
        for (cli, resolver, launch_argv) in routes {
            if cli.id.trim().is_empty() {
                return Err("ACP launch route id must not be empty".into());
            }
            if launch_argv
                .as_ref()
                .is_some_and(|argv| argv.is_empty() || argv[0].trim().is_empty())
            {
                return Err(format!(
                    "ACP launch argv for `{}` must not be empty",
                    cli.id
                ));
            }
            let id = cli.id.to_string();
            if indexed
                .insert(
                    id.clone(),
                    ProjectedLaunch {
                        cli,
                        resolver,
                        launch_argv,
                    },
                )
                .is_some()
            {
                return Err(format!("duplicate ACP launch route `acp:{id}`"));
            }
        }
        if indexed.is_empty() {
            return Err("ACP launch registry must contain at least one route".into());
        }
        Ok(Self {
            routes: Arc::new(indexed),
        })
    }

    pub fn single(cli: AcpCli, resolver: Arc<dyn LaunchResolver>) -> Self {
        Self::new(vec![(cli, resolver)]).expect("one known ACP CLI is a valid launch registry")
    }

    pub fn single_with_resolved_argv(
        cli: AcpCli,
        resolver: Arc<dyn LaunchResolver>,
        launch_argv: Option<Vec<String>>,
    ) -> Result<Self, String> {
        Self::with_resolved_argv(vec![(cli, resolver, launch_argv)])
    }

    pub(super) fn selected(
        &self,
        backend: &awaken_runtime_contract::resolved::Backend,
    ) -> Result<&ProjectedLaunch, OpenError> {
        let awaken_runtime_contract::resolved::Backend::Acp(backend) = backend else {
            return Err(OpenError("run backend is not ACP".to_string()));
        };
        let id = backend.cli();
        self.routes.get(id).ok_or_else(|| {
            let available = self
                .routes
                .keys()
                .map(|cli| format!("acp:{cli}"))
                .collect::<Vec<_>>()
                .join(", ");
            OpenError(format!(
                "run selected `acp:{id}` but this worker only serves [{available}]"
            ))
        })
    }

    pub(crate) fn credential_realization_capabilities(
        &self,
        backend: &awaken_runtime_contract::resolved::Backend,
    ) -> Result<awaken_credential_contract::CredentialRealizationCapabilities, OpenError> {
        Ok(self
            .selected(backend)?
            .resolver
            .credential_realization_capabilities())
    }
}

/// How a sandboxed/containerized ACP source obtains a run's CLI launch: a fixed
/// test argv, or an exact per-run route from the Worker's launch registry.
///
/// This is the public factory input to the parent module's channel-source
/// builder: a process startup picks `Projected` to serve each run's
/// config-plane-selected CLI, or `Fixed` for a trusted/test single argv.
#[derive(Clone)]
pub enum LaunchSource {
    /// One newline-fixture CLI for every `acp:*` thread (explicit trusted/test
    /// startup only).
    Fixed(Box<AcpLaunch>),
    /// One official JSON-RPC ACP fixture for every `acp:*` thread. This differs
    /// only in codec selection; launch and Session-environment ownership stay on
    /// the same bound source as [`Self::Fixed`].
    FixedAcp(Box<AcpLaunch>),
    /// The run's config-plane-selected CLI, exact-routed by `acp:<cli>`.
    Projected(AcpLaunchRegistry),
}

impl LaunchSource {
    pub(super) fn check_tool_narrowing(
        &self,
        codec: awaken_run_executor_acp::Codec,
        narrowing: &awaken_runtime_contract::permission::ToolCapabilityNarrowing,
    ) -> Result<(), OpenError> {
        match self {
            Self::Fixed(launch) | Self::FixedAcp(launch) => {
                launch.check_tool_narrowing(codec, narrowing)
            }
            Self::Projected(_) => awaken_run_executor_acp::opaque_tool_narrowing(narrowing),
        }
    }

    pub(crate) fn credential_realization_capabilities(
        &self,
        backend: &awaken_runtime_contract::resolved::Backend,
    ) -> Result<awaken_credential_contract::CredentialRealizationCapabilities, OpenError> {
        match self {
            Self::Fixed(_) | Self::FixedAcp(_) => Ok(Default::default()),
            Self::Projected(registry) => registry.credential_realization_capabilities(backend),
        }
    }

    /// Projects the run's exact selected route into one concrete launch.
    pub(super) async fn resolve(
        &self,
        activation: &RunActivation,
        backend: &awaken_runtime_contract::resolved::Backend,
        context: &awaken_runtime_contract::runtime_context::RuntimeRunContext,
    ) -> Result<ResolvedLaunch, OpenError> {
        match self {
            LaunchSource::Fixed(launch) | LaunchSource::FixedAcp(launch) => Ok(ResolvedLaunch {
                launch: launch.as_ref().clone(),
                credential_artifact: None,
                secret_broker: None,
            }),
            LaunchSource::Projected(registry) => {
                let selected = registry.selected(backend)?;
                awaken_run_executor_acp::project_launch(
                    &selected.cli,
                    selected.resolver.as_ref(),
                    activation,
                    context,
                    selected.launch_argv.as_deref(),
                )
                .await
            }
        }
    }

    pub(super) fn cli(
        &self,
        backend: &awaken_runtime_contract::resolved::Backend,
    ) -> Result<Option<&AcpCli>, OpenError> {
        match self {
            LaunchSource::Fixed(_) | LaunchSource::FixedAcp(_) => Ok(None),
            LaunchSource::Projected(registry) => {
                registry.selected(backend).map(|route| Some(&route.cli))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeResolver;

    #[async_trait::async_trait]
    impl LaunchResolver for FakeResolver {
        async fn model(
            &self,
            _activation: &RunActivation,
            _context: &awaken_runtime_contract::RuntimeRunContext,
        ) -> Result<awaken_run_executor_acp::ResolvedModel, OpenError> {
            unreachable!("route-selection tests do not resolve a model")
        }
    }

    #[test]
    fn exact_routes_multiple_clis_and_rejects_bare_acp() {
        // Cause graph: C1=the backend is an exact `acp:<cli>`; C2=that CLI has an
        // installed route. E1=C1+C2 selects that route; E2=!C1|!C2 fails closed.
        // Constraint: model-default policy applies only after exact CLI selection;
        // the launch registry never owns an implicit/default CLI.
        // Decision table:
        // D1 exact ACP + known CLI   -> selected route
        // D2 exact ACP + unknown CLI -> error
        // D3 bare/invalid ACP        -> error before route lookup
        let routes = ["claude", "codex"]
            .into_iter()
            .map(|id| {
                (
                    *awaken_run_executor_acp::acp_cli(id).expect("known test ACP CLI"),
                    Arc::new(FakeResolver) as Arc<dyn LaunchResolver>,
                )
            })
            .collect();
        let registry = AcpLaunchRegistry::new(routes).expect("two exact ACP routes");
        let selected = registry
            .selected(&awaken_runtime_contract::resolved::Backend::from_ref(
                "acp:claude",
            ))
            .expect("known exact route");
        assert_eq!(selected.cli.id, "claude");
        assert!(
            registry
                .selected(&awaken_runtime_contract::resolved::Backend::from_ref("acp"))
                .is_err(),
            "bare ACP is not an execution route"
        );
        assert!(
            registry
                .selected(&awaken_runtime_contract::resolved::Backend::from_ref(
                    "acp:gemini",
                ))
                .is_err()
        );
    }

    #[test]
    fn single_route_preserves_acquired_argv_and_rejects_empty_executables() {
        let cli = *awaken_run_executor_acp::acp_cli("claude").unwrap();
        let registry = AcpLaunchRegistry::single_with_resolved_argv(
            cli,
            Arc::new(FakeResolver),
            Some(vec!["/opt/awaken/claude-agent-acp".to_string()]),
        )
        .unwrap();
        let selected = registry
            .selected(&awaken_runtime_contract::resolved::Backend::from_ref(
                "acp:claude",
            ))
            .unwrap();
        assert_eq!(
            selected.launch_argv.as_deref(),
            Some(["/opt/awaken/claude-agent-acp".to_string()].as_slice())
        );

        for invalid in [Vec::new(), vec!["   ".to_string()]] {
            assert!(
                AcpLaunchRegistry::single_with_resolved_argv(
                    cli,
                    Arc::new(FakeResolver),
                    Some(invalid),
                )
                .is_err()
            );
        }
    }
}
