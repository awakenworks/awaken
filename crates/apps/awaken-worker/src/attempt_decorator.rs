// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: The registration-bound Worker application assembly accepts one
// @formal reason: identity and one declared capability set, then rejects mixed
// @formal reason: sources or realized capability drift before Ready. The
// @formal reason: registered_application_factory and mutually-exclusive-source
// @formal reason: decision tests cover error, panic, drift and no physical setup;
// @formal reason: registry durability and transport remain outside this source.
//! Registration-bound attempt decoration assembly.

use std::sync::Arc;

use awaken_session_executor::AttemptExecutorDecorator;
use awaken_worker_contract::{RegisteredWorker, WorkerIdentity, WorkerSnapshot};
use awaken_worker_transport_security::WorkerUpstream;

/// Immutable application assembly context created only after the Coordinator
/// allocates this process's Worker identity.
#[derive(Clone)]
pub struct RegisteredWorkerContext {
    registration: RegisteredWorker,
    upstream: WorkerUpstream,
}

impl RegisteredWorkerContext {
    pub(crate) fn new(registration: RegisteredWorker, upstream: WorkerUpstream) -> Self {
        Self {
            registration,
            upstream,
        }
    }

    #[must_use]
    pub fn registration(&self) -> &RegisteredWorker {
        &self.registration
    }

    #[must_use]
    pub fn snapshot(&self) -> &WorkerSnapshot {
        &self.registration.snapshot
    }

    #[must_use]
    pub fn identity(&self) -> &WorkerIdentity {
        &self.registration.snapshot.identity
    }

    /// The identity-bound transport shared by dispatch, claimed commit, and
    /// optional attempt-decoration control requests.
    #[must_use]
    pub fn upstream(&self) -> &WorkerUpstream {
        &self.upstream
    }
}

/// Registration-time factory for the one neutral attempt decorator. It cannot
/// author Session state, place Work, or replace the backend executor registry.
pub type RegisteredAttemptDecoratorFactory =
    Arc<dyn Fn(&RegisteredWorkerContext) -> Result<AttemptExecutorDecorator, String> + Send + Sync>;

/// One dependency value installed by the canonical Worker after registration.
#[derive(Default)]
pub struct WorkerApplicationDependencies {
    pub attempt_decorator: Option<AttemptExecutorDecorator>,
    pub inference: Option<Arc<dyn awaken_inference_endpoint::InferenceExecutorMaterializer>>,
    pub web_providers: awaken_ext_builtin_tools::WebSearchProviderRegistry,
    pub checkpoint_store: Option<Arc<dyn awaken_provisioning_contract::SandboxCheckpointStore>>,
}

/// Secret-free facts used by the existing manifest owner before registration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerApplicationCapabilities {
    pub inference: bool,
    pub inference_schemes: std::collections::BTreeSet<String>,
    pub inference_credentials: awaken_credential_contract::CredentialRealizationCapabilities,
    pub checkpoint_store: bool,
    pub attempt_decorator: bool,
    pub web_search_schema: serde_json::Value,
    pub web_fetch_schema: serde_json::Value,
}

impl Default for WorkerApplicationCapabilities {
    fn default() -> Self {
        WorkerApplicationDependencies::default().capabilities()
    }
}

impl WorkerApplicationDependencies {
    pub fn capabilities(&self) -> WorkerApplicationCapabilities {
        WorkerApplicationCapabilities {
            inference: self.inference.is_some(),
            inference_schemes: self
                .inference
                .as_ref()
                .map(|value| {
                    value
                        .supported_access_schemes()
                        .iter()
                        .map(|s| (*s).to_owned())
                        .collect()
                })
                .unwrap_or_default(),
            inference_credentials: self
                .inference
                .as_ref()
                .map(|value| value.credential_realization_capabilities())
                .unwrap_or_default(),
            checkpoint_store: self.checkpoint_store.is_some(),
            attempt_decorator: self.attempt_decorator.is_some(),
            web_search_schema: self.web_providers.config_schema(),
            web_fetch_schema: self.web_providers.fetch_config_schema(),
        }
    }
}

pub type RegisteredWorkerApplicationFactory = Arc<
    dyn Fn(&RegisteredWorkerContext) -> Result<WorkerApplicationDependencies, String> + Send + Sync,
>;

pub(crate) enum WorkerApplicationSource {
    Static {
        dependencies: WorkerApplicationDependencies,
        decorator: Option<RegisteredAttemptDecoratorFactory>,
        selected: bool,
    },
    Registered {
        capabilities: WorkerApplicationCapabilities,
        factory: RegisteredWorkerApplicationFactory,
    },
    Conflict,
}

impl Default for WorkerApplicationSource {
    fn default() -> Self {
        Self::Static {
            dependencies: WorkerApplicationDependencies {
                web_providers: awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins(),
                ..Default::default()
            },
            decorator: None,
            selected: false,
        }
    }
}

impl WorkerApplicationSource {
    pub(crate) fn static_dependencies(&mut self) -> Option<&mut WorkerApplicationDependencies> {
        match self {
            Self::Static {
                dependencies,
                selected,
                ..
            } => {
                *selected = true;
                Some(dependencies)
            }
            _ => {
                *self = Self::Conflict;
                None
            }
        }
    }

    pub(crate) fn capabilities(&self) -> Result<WorkerApplicationCapabilities, String> {
        match self {
            Self::Static { dependencies, .. } => Ok(dependencies.capabilities()),
            Self::Registered { capabilities, .. } => Ok(capabilities.clone()),
            Self::Conflict => {
                Err("Worker application dependency sources are mutually exclusive".into())
            }
        }
    }

    pub(crate) fn realize(
        self,
        context: &RegisteredWorkerContext,
    ) -> Result<WorkerApplicationDependencies, String> {
        match self {
            Self::Static {
                mut dependencies,
                decorator,
                ..
            } => {
                if let Some(factory) = decorator {
                    dependencies.attempt_decorator = Some(factory(context)?);
                }
                Ok(dependencies)
            }
            Self::Registered {
                capabilities,
                factory,
            } => {
                let dependencies = factory(context)?;
                if dependencies.capabilities() != capabilities {
                    return Err("registered Worker application capabilities differ from the admitted declaration".into());
                }
                Ok(dependencies)
            }
            Self::Conflict => {
                Err("Worker application dependency sources are mutually exclusive".into())
            }
        }
    }
}
