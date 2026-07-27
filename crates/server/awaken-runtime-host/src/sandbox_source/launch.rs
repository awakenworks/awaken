//! Exact ACP launch-route selection and projection.

use std::collections::BTreeMap;
use std::sync::Arc;

use awaken_provisioning_contract as pc;
use awaken_run_executor_acp::{AcpCli, AcpLaunch, LaunchResolver, OpenError};
use awaken_runtime_contract::activation::RunActivation;

#[derive(Clone)]
pub(super) struct ProjectedLaunch {
    pub(super) cli: AcpCli,
    resolver: Arc<dyn LaunchResolver>,
}

/// Exact `acp:<cli>` launch routes installed on one Worker.
#[derive(Clone)]
pub struct AcpLaunchRegistry {
    routes: Arc<BTreeMap<String, ProjectedLaunch>>,
    default_cli: Option<String>,
}

impl AcpLaunchRegistry {
    pub fn new(
        routes: Vec<(AcpCli, Arc<dyn LaunchResolver>)>,
        default_cli: Option<String>,
    ) -> Result<Self, String> {
        let mut indexed = BTreeMap::new();
        for (cli, resolver) in routes {
            if cli.id.trim().is_empty() {
                return Err("ACP launch route id must not be empty".into());
            }
            let id = cli.id.to_string();
            if indexed
                .insert(id.clone(), ProjectedLaunch { cli, resolver })
                .is_some()
            {
                return Err(format!("duplicate ACP launch route `acp:{id}`"));
            }
        }
        if indexed.is_empty() {
            return Err("ACP launch registry must contain at least one route".into());
        }
        if let Some(default) = &default_cli
            && !indexed.contains_key(default)
        {
            return Err(format!(
                "default ACP CLI `acp:{default}` has no launch route"
            ));
        }
        Ok(Self {
            routes: Arc::new(indexed),
            default_cli,
        })
    }

    pub fn single(cli: AcpCli, resolver: Arc<dyn LaunchResolver>) -> Self {
        let default = cli.id.to_string();
        Self::new(vec![(cli, resolver)], Some(default))
            .expect("one known ACP CLI is a valid launch registry")
    }

    pub(super) fn selected(
        &self,
        backend: &awaken_runtime_contract::resolved::Backend,
    ) -> Result<&ProjectedLaunch, OpenError> {
        let awaken_runtime_contract::resolved::Backend::Acp { cli } = backend else {
            return Err(OpenError("run backend is not ACP".to_string()));
        };
        let id = if cli.is_empty() {
            self.default_cli.as_deref().ok_or_else(|| {
                OpenError("bare `acp` has no configured default CLI route".to_string())
            })?
        } else {
            cli.as_str()
        };
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
    ) -> Result<awaken_runtime_contract::CredentialRealizationCapabilities, OpenError> {
        Ok(self
            .selected(backend)?
            .resolver
            .credential_realization_capabilities())
    }

    pub(super) fn combined_credential_realization_capabilities(
        &self,
    ) -> awaken_runtime_contract::CredentialRealizationCapabilities {
        let mut combined = awaken_runtime_contract::CredentialRealizationCapabilities::default();
        for route in self.routes.values() {
            combined.merge(&route.resolver.credential_realization_capabilities());
        }
        combined
    }
}

/// How a sandboxed/containerized ACP source obtains a run's CLI launch.
#[derive(Clone)]
pub enum LaunchSource {
    /// One CLI for every `acp:*` thread (explicit trusted/test composition only).
    Fixed(AcpLaunch),
    /// The run's config-plane-selected CLI, exact-routed by `acp:<cli>`.
    Projected(AcpLaunchRegistry),
}

pub(super) struct ResolvedLaunch {
    pub(super) launch: AcpLaunch,
    pub(super) credential_artifact: Option<awaken_run_executor_acp::CredentialArtifactRequirement>,
    pub(super) secret_broker: Option<Arc<dyn pc::SecretBroker>>,
}

impl LaunchSource {
    pub(crate) fn combined_credential_realization_capabilities(
        &self,
    ) -> awaken_runtime_contract::CredentialRealizationCapabilities {
        match self {
            Self::Fixed(_) => Default::default(),
            Self::Projected(registry) => registry.combined_credential_realization_capabilities(),
        }
    }

    pub(crate) fn credential_realization_capabilities(
        &self,
        backend: &awaken_runtime_contract::resolved::Backend,
    ) -> Result<awaken_runtime_contract::CredentialRealizationCapabilities, OpenError> {
        match self {
            Self::Fixed(_) => Ok(Default::default()),
            Self::Projected(registry) => registry.credential_realization_capabilities(backend),
        }
    }

    /// Resolve the fixed launch or the run's exact projected ACP route.
    pub(super) fn resolve(
        &self,
        activation: &RunActivation,
        backend: &awaken_runtime_contract::resolved::Backend,
        context: &awaken_runtime_contract::runtime_context::RuntimeRunContext,
    ) -> Result<ResolvedLaunch, OpenError> {
        match self {
            LaunchSource::Fixed(launch) => Ok(ResolvedLaunch {
                launch: launch.clone(),
                credential_artifact: None,
                secret_broker: None,
            }),
            LaunchSource::Projected(registry) => {
                let selected = registry.selected(backend)?;
                let model = selected.resolver.model(activation, context)?;
                let credential_artifact = model.credential_artifact.clone();
                let extra_env = selected.resolver.extra_env(activation)?;
                let window = awaken_run_executor_acp::AcpSettings::from_plugin_config(
                    &activation.snapshot.resolved_spec.plugin_config,
                )
                .compact_window;
                let launch = selected.cli.try_project(&model, window, &extra_env)?;
                Ok(ResolvedLaunch {
                    launch,
                    credential_artifact,
                    secret_broker: selected.resolver.secret_broker(),
                })
            }
        }
    }

    pub(super) fn cli(
        &self,
        backend: &awaken_runtime_contract::resolved::Backend,
    ) -> Result<Option<&AcpCli>, OpenError> {
        match self {
            LaunchSource::Fixed(_) => Ok(None),
            LaunchSource::Projected(registry) => {
                registry.selected(backend).map(|route| Some(&route.cli))
            }
        }
    }
}
