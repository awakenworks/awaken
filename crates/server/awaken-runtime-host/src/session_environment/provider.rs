//! Selection and lifecycle construction for a Session's single environment.

use super::{HandExecutorFactory, SessionEnvironment, container_files};
use crate::deployment_config::ContainerHandResidency;
use std::sync::Arc;

use awaken_provisioning_contract as pc;
use awaken_sandbox_local::{LocalProvider, NamespaceProvider};

/// Creates/adopts the one Session environment while auxiliary housekeeping Runs
/// may continue using their deliberately-fresh LocalProvider.
pub(crate) enum SessionEnvironmentProvider {
    Workdir(LocalProvider),
    Namespace {
        provider: NamespaceProvider,
        hand_factory: Arc<dyn HandExecutorFactory>,
        hand_bin: String,
        hand_idle_after: std::time::Duration,
    },
    Container {
        provider: Arc<dyn awaken_sandbox_container::ContainerEnvironmentProvider>,
        capacity: Option<Arc<dyn awaken_sandbox_container::ContainerEnvironmentCapacity>>,
        extra_mounts: Vec<pc::MountRequirement>,
        hand_factory: Arc<dyn HandExecutorFactory>,
        hand_bin: String,
        hand_idle_after: std::time::Duration,
        hand_residency: ContainerHandResidency,
    },
}

pub(crate) struct SessionEnvironmentAdoptionLayout {
    pub(crate) spec: pc::SandboxSpec,
    pub(crate) historical_owned_paths: Option<Vec<String>>,
}

impl SessionEnvironmentProvider {
    /// Construct the in-process adapter for a non-container Deployment tier.
    /// Both initial Host construction and asynchronous provider selection use this one mapping;
    /// container tiers return `None` because their provider requires async setup.
    pub(crate) fn for_host_tier(
        tier: crate::SandboxTier,
        base: impl Into<std::path::PathBuf>,
        inherit_agent_stderr: bool,
        hand_factory: Option<Arc<dyn HandExecutorFactory>>,
        namespace_hand_bin: impl Into<String>,
        hand_idle_after: std::time::Duration,
    ) -> Option<Self> {
        let base = base.into();
        match tier {
            crate::SandboxTier::Local => {
                Some(Self::workdir_with_agent_stderr(base, inherit_agent_stderr))
            }
            crate::SandboxTier::Namespace => {
                let hand_factory = hand_factory.unwrap_or_else(|| {
                    panic!("namespace Session environments require a hand executor factory")
                });
                Some(Self::namespace_with_agent_stderr(
                    base,
                    inherit_agent_stderr,
                    hand_factory,
                    namespace_hand_bin,
                    hand_idle_after,
                ))
            }
            crate::SandboxTier::Docker | crate::SandboxTier::Podman | crate::SandboxTier::K8s => {
                None
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn supports_host_identity(&self) -> bool {
        matches!(self, Self::Workdir(_))
    }

    pub(crate) fn capabilities(&self) -> pc::SandboxCapabilities {
        match self {
            Self::Workdir(provider) => pc::SandboxProvider::capabilities(provider),
            Self::Namespace { provider, .. } => pc::SandboxProvider::capabilities(provider),
            Self::Container { provider, .. } => provider.sandbox_capabilities(),
        }
    }

    pub(crate) fn workdir(base: impl Into<std::path::PathBuf>) -> Self {
        Self::Workdir(LocalProvider::new(base))
    }

    pub(crate) fn workdir_with_agent_stderr(
        base: impl Into<std::path::PathBuf>,
        inherit: bool,
    ) -> Self {
        Self::Workdir(LocalProvider::new(base).with_agent_stderr(inherit))
    }

    pub(crate) fn namespace_with_agent_stderr(
        base: impl Into<std::path::PathBuf>,
        inherit: bool,
        hand_factory: Arc<dyn HandExecutorFactory>,
        hand_bin: impl Into<String>,
        hand_idle_after: std::time::Duration,
    ) -> Self {
        Self::Namespace {
            provider: NamespaceProvider::new(base).with_agent_stderr(inherit),
            hand_factory,
            hand_bin: hand_bin.into(),
            hand_idle_after,
        }
    }

    #[cfg(test)]
    pub(crate) fn container(
        provider: Arc<dyn awaken_sandbox_container::ContainerEnvironmentProvider>,
        extra_mounts: Vec<pc::MountRequirement>,
        hand_factory: Arc<dyn HandExecutorFactory>,
        hand_bin: impl Into<String>,
    ) -> Self {
        Self::container_with_capacity_and_hand_idle(
            provider,
            None,
            extra_mounts,
            hand_factory,
            hand_bin,
            std::time::Duration::ZERO,
        )
    }

    #[cfg(test)]
    pub(crate) fn container_with_capacity_and_hand_idle(
        provider: Arc<dyn awaken_sandbox_container::ContainerEnvironmentProvider>,
        capacity: Option<Arc<dyn awaken_sandbox_container::ContainerEnvironmentCapacity>>,
        extra_mounts: Vec<pc::MountRequirement>,
        hand_factory: Arc<dyn HandExecutorFactory>,
        hand_bin: impl Into<String>,
        hand_idle_after: std::time::Duration,
    ) -> Self {
        Self::container_with_capacity_hand_idle_and_residency(
            provider,
            capacity,
            extra_mounts,
            hand_factory,
            hand_bin,
            hand_idle_after,
            ContainerHandResidency::AttachedExec,
        )
    }

    pub(crate) fn container_with_capacity_hand_idle_and_residency(
        provider: Arc<dyn awaken_sandbox_container::ContainerEnvironmentProvider>,
        capacity: Option<Arc<dyn awaken_sandbox_container::ContainerEnvironmentCapacity>>,
        extra_mounts: Vec<pc::MountRequirement>,
        hand_factory: Arc<dyn HandExecutorFactory>,
        hand_bin: impl Into<String>,
        hand_idle_after: std::time::Duration,
        hand_residency: ContainerHandResidency,
    ) -> Self {
        Self::Container {
            provider,
            capacity,
            extra_mounts,
            hand_factory,
            hand_bin: hand_bin.into(),
            hand_idle_after,
            hand_residency,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn at_root(&self, base: impl Into<std::path::PathBuf>) -> Self {
        let base = base.into();
        match self {
            Self::Workdir(provider) => {
                Self::workdir_with_agent_stderr(base, provider.inherits_agent_stderr())
            }
            Self::Namespace {
                provider,
                hand_factory,
                hand_bin,
                hand_idle_after,
            } => Self::namespace_with_agent_stderr(
                base,
                provider.inherits_agent_stderr(),
                hand_factory.clone(),
                hand_bin.clone(),
                *hand_idle_after,
            ),
            Self::Container {
                provider,
                capacity,
                extra_mounts,
                hand_factory,
                hand_bin,
                hand_idle_after,
                hand_residency,
            } => Self::Container {
                provider: provider.clone(),
                capacity: capacity.clone(),
                extra_mounts: extra_mounts.clone(),
                hand_factory: hand_factory.clone(),
                hand_bin: hand_bin.clone(),
                hand_idle_after: *hand_idle_after,
                hand_residency: *hand_residency,
            },
        }
    }

    pub(crate) fn install_memory_mounter(&self, mounter: Arc<dyn pc::MemoryMounter>) {
        match self {
            Self::Workdir(provider) => provider.install_memory_mounter(mounter),
            Self::Namespace { provider, .. } => provider.install_memory_mounter(mounter),
            Self::Container { provider, .. } => provider.install_memory_mounter(mounter),
        }
    }

    pub(crate) fn install_secret_broker(&self, broker: Arc<dyn pc::SecretBroker>) {
        match self {
            Self::Workdir(provider) => provider.install_secret_broker(broker),
            Self::Namespace { provider, .. } => provider.install_secret_broker(broker),
            Self::Container { provider, .. } => provider.install_secret_broker(broker),
        }
    }

    pub(crate) async fn create(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        let spec = self.effective_spec(spec)?;
        self.create_effective(&spec).await
    }

    /// Produce the exact provider-visible spec, including provider-owned
    /// isolation selection and container extra mounts, without performing I/O.
    /// Admission, capacity, create, adopt, and restore all consume this one
    /// projection so path validation cannot inspect a weaker layout.
    pub(crate) fn effective_spec(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<pc::SandboxSpec, pc::SandboxError> {
        match self {
            Self::Workdir(_) => Ok(spec.clone()),
            Self::Namespace { .. } => {
                let mut spec = spec.clone();
                spec.isolation = pc::IsolationClass::Namespace;
                Ok(spec)
            }
            Self::Container { extra_mounts, .. } => container_spec(spec, extra_mounts),
        }
    }

    /// Project the exact layout a provider will reopen from a durable handle.
    ///
    /// Local and namespace adapters restore output and process-directory
    /// coordinates from the typed handle rather than from the caller's current
    /// spec; container implementations receive both and may make the same
    /// choice. Keeping that historical evidence in this pure projection lets
    /// Runtime reject a stale conflicting layout before provider I/O.
    pub(crate) fn effective_adoption_layout(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
    ) -> Result<SessionEnvironmentAdoptionLayout, pc::SandboxError> {
        let mut spec = self.effective_spec(spec)?;
        match self {
            Self::Workdir(_) => {
                let payload = handle.local_payload()?;
                spec.outputs_path.clone_from(&payload.outputs_path);
                spec.env.clone_from(&payload.base_env);
            }
            Self::Namespace { .. } => {
                let payload = handle.namespace_payload(NamespaceProvider::provider_kind())?;
                spec.outputs_path.clone_from(&payload.outputs_path);
                spec.env.clone_from(&payload.base_env);
                spec.network.clone_from(&payload.network);
            }
            Self::Container { .. } => {
                let payload = handle.container_payload()?;
                spec.outputs_path.clone_from(&payload.outputs_path);
                spec.env.clone_from(&payload.base_env);
            }
        }
        Ok(SessionEnvironmentAdoptionLayout {
            spec,
            historical_owned_paths: handle.owned_paths().map(<[String]>::to_vec),
        })
    }

    pub(crate) async fn create_effective(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        self.create_effective_for_effect(
            spec,
            None,
            None,
            awaken_sandbox_container::ContainerRealizationIntent::Create,
        )
        .await
    }

    pub(crate) async fn create_effective_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        effect_fence: Option<&awaken_provisioning_contract::SandboxEffectFence>,
        source_handle: Option<&pc::SandboxHandle>,
        intent: awaken_sandbox_container::ContainerRealizationIntent,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        match self {
            Self::Workdir(provider) => match effect_fence {
                Some(effect_fence) => provider
                    .create_sandbox_for_effect(spec, effect_fence, source_handle)
                    .await
                    .map(SessionEnvironment::workdir),
                None => provider
                    .create_sandbox(spec)
                    .await
                    .map(SessionEnvironment::workdir),
            },
            Self::Namespace {
                provider,
                hand_factory,
                hand_bin,
                hand_idle_after,
            } => match effect_fence {
                Some(effect_fence) => {
                    provider
                        .create_sandbox_for_effect(spec, effect_fence, source_handle)
                        .await
                }
                None => provider.create_sandbox(spec).await,
            }
            .map(|sandbox| {
                SessionEnvironment::namespace(
                    sandbox,
                    hand_factory.clone(),
                    hand_bin,
                    *hand_idle_after,
                )
            }),
            Self::Container {
                provider,
                hand_factory,
                hand_bin,
                hand_idle_after,
                hand_residency,
                ..
            } => {
                let capabilities = provider.sandbox_capabilities();
                let environment = provider
                    .create_environment_for_effect(spec, effect_fence, intent)
                    .await?;
                SessionEnvironment::container(
                    environment,
                    hand_factory.clone(),
                    hand_bin,
                    *hand_idle_after,
                    *hand_residency,
                    capabilities,
                )
                .await
            }
        }
    }

    /// Pre-create never-used capacity for the same exact normalized container
    /// shape that [`Self::create`] will request. Non-container and direct-provider
    /// deployments have no capacity owner and return zero.
    pub(crate) async fn prewarm(
        &self,
        spec: &pc::SandboxSpec,
        target: usize,
    ) -> Result<usize, pc::SandboxError> {
        match self {
            Self::Container {
                capacity: Some(capacity),
                ..
            } => {
                let spec = self.effective_spec(spec)?;
                capacity.prewarm_to(&spec, target).await
            }
            _ => Ok(0),
        }
    }

    pub(crate) async fn discard_capacity(&self, spec: &pc::SandboxSpec) {
        if let Self::Container {
            capacity: Some(capacity),
            ..
        } = self
            && let Ok(spec) = self.effective_spec(spec)
        {
            capacity.discard_shape(&spec).await;
        }
    }

    pub(crate) fn ready_capacity(&self, spec: &pc::SandboxSpec) -> usize {
        if let Self::Container {
            capacity: Some(capacity),
            ..
        } = self
            && let Ok(spec) = self.effective_spec(spec)
        {
            return capacity.ready_capacity(&spec);
        }
        0
    }

    /// Drain only unused container capacity. Active Session environments are no
    /// longer members of the pool and retain their ordinary Session lifecycle.
    pub(crate) async fn shutdown_capacity(&self) {
        if let Self::Container {
            capacity: Some(capacity),
            ..
        } = self
        {
            capacity.shutdown_capacity().await;
        }
    }

    /// Test-only convenience projection. Durable Session adoption uses
    /// `adopt_effective_for_effect` after root-owned authorization.
    #[cfg(test)]
    pub(crate) async fn adopt(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        let layout = self.effective_adoption_layout(spec, handle)?;
        self.adopt_effective(&layout.spec, handle).await
    }

    /// Effect-free observation through the exact provider selected for this
    /// Session. This is the only Runtime projection of the neutral observation
    /// contract; backend errors remain indeterminate and are never translated
    /// into replacement authority here.
    pub(crate) async fn observe_effective(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        match self {
            Self::Workdir(provider) => pc::SandboxProvider::observe(provider, handle).await,
            Self::Namespace { provider, .. } => {
                pc::SandboxProvider::observe(provider, handle).await
            }
            Self::Container { provider, .. } => {
                provider
                    .observe_environment(
                        awaken_sandbox_container::ContainerEnvironmentAdoption::new(spec, handle),
                    )
                    .await
            }
        }
    }

    /// Observe one exact durable handle under the aggregate-owned physical
    /// effect fence. This is the sole Runtime projection of the neutral fenced
    /// observation port; adapters must revalidate the fence and provider-
    /// effective spec without producing create/adopt/delete effects.
    pub(crate) async fn observe_effective_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        effect_fence: &pc::SandboxEffectFence,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        match self {
            Self::Workdir(provider) => {
                pc::SandboxProvider::observe_for_effect(provider, spec, handle, effect_fence).await
            }
            Self::Namespace { provider, .. } => {
                pc::SandboxProvider::observe_for_effect(provider, spec, handle, effect_fence).await
            }
            Self::Container { provider, .. } => {
                provider
                    .observe_environment_for_effect(
                        awaken_sandbox_container::ContainerEnvironmentAdoption::new(spec, handle),
                        effect_fence,
                    )
                    .await
            }
        }
    }

    /// Validate the physical evidence carried by one closed observation.
    /// Filesystem absence has no live incarnation, while `Terminal` and
    /// `Disposing` must name the exact marker incarnation in the durable handle.
    /// Container observations always retain their exact backend incarnation.
    /// Keeping all three rows here prevents lifecycle callers from interpreting
    /// provider-specific evidence or weakening legacy/foreign handles.
    pub(crate) fn validate_closed_observation(
        &self,
        handle: &pc::SandboxHandle,
        observation: &pc::SandboxObservation,
    ) -> Result<(), pc::SandboxError> {
        match observation {
            pc::SandboxObservation::DefinitivelyUnavailable {
                physical_incarnation,
            } => match self {
                Self::Workdir(_) | Self::Namespace { .. } if physical_incarnation.is_none() => {
                    Ok(())
                }
                Self::Workdir(_) | Self::Namespace { .. } => Err(pc::SandboxError::new(
                    "filesystem sandbox absence carried a foreign physical incarnation",
                )),
                Self::Container { .. } => {
                    let expected = handle.container_physical_incarnation()?;
                    if physical_incarnation.as_deref() == Some(expected) {
                        Ok(())
                    } else {
                        Err(pc::SandboxError::new(
                            "container unavailable observation does not match the durable incarnation",
                        ))
                    }
                }
            },
            pc::SandboxObservation::Terminal {
                physical_incarnation,
            }
            | pc::SandboxObservation::Disposing {
                physical_incarnation,
            } => {
                let expected = match self {
                    Self::Workdir(_) | Self::Namespace { .. } => {
                        handle.filesystem_physical_incarnation()?.ok_or_else(|| {
                            pc::SandboxError::new(
                                "legacy filesystem handle has no physical incarnation",
                            )
                        })?
                    }
                    Self::Container { .. } => handle.container_physical_incarnation()?,
                };
                if physical_incarnation == expected {
                    Ok(())
                } else {
                    Err(pc::SandboxError::new(
                        "terminal sandbox observation does not match the durable incarnation",
                    ))
                }
            }
            pc::SandboxObservation::Ready
            | pc::SandboxObservation::Provisioning
            | pc::SandboxObservation::Incompatible { .. } => Err(pc::SandboxError::new(
                "sandbox observation does not carry closed physical evidence",
            )),
        }
    }

    /// Adopt after Runtime has validated this exact provider-and-handle
    /// projection. Callers that have not performed the pure preflight use
    /// [`Self::adopt`] instead.
    #[cfg(test)]
    pub(crate) async fn adopt_effective(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        self.adopt_effective_for_effect(spec, handle, None).await
    }

    /// Adopt the already-observed exact handle under the root-authorized
    /// provider fence. Local/namespace providers revalidate their immutable
    /// marker in `adopt_sandbox`; container adapters additionally fence the
    /// backend observation/lease effect against Worker replacement.
    pub(crate) async fn adopt_effective_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        effect_fence: Option<&awaken_provisioning_contract::SandboxEffectFence>,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        match self {
            Self::Workdir(provider) => match effect_fence {
                Some(effect_fence) => provider
                    .adopt_sandbox_for_effect(spec, handle, effect_fence)
                    .await
                    .map(SessionEnvironment::workdir),
                None => provider
                    .adopt_sandbox_with_spec(spec, handle)
                    .await
                    .map(SessionEnvironment::workdir),
            },
            Self::Namespace {
                provider,
                hand_factory,
                hand_bin,
                hand_idle_after,
            } => match effect_fence {
                Some(effect_fence) => {
                    provider
                        .adopt_sandbox_for_effect(spec, handle, effect_fence)
                        .await
                }
                None => {
                    provider
                        .adopt_sandbox_with_control_services(handle, &spec.control_services)
                        .await
                }
            }
            .map(|sandbox| {
                SessionEnvironment::namespace(
                    sandbox,
                    hand_factory.clone(),
                    hand_bin,
                    *hand_idle_after,
                )
            }),
            Self::Container {
                provider,
                hand_factory,
                hand_bin,
                hand_idle_after,
                hand_residency,
                ..
            } => {
                let capabilities = provider.sandbox_capabilities();
                let environment = provider
                    .adopt_environment_for_effect(
                        awaken_sandbox_container::ContainerEnvironmentAdoption::new(spec, handle),
                        effect_fence,
                    )
                    .await?;
                environment.renew_lease().await?;
                SessionEnvironment::container(
                    environment,
                    hand_factory.clone(),
                    hand_bin,
                    *hand_idle_after,
                    *hand_residency,
                    capabilities,
                )
                .await
            }
        }
    }

    /// Restore only the exact target frozen into the durable continuation
    /// operation. The returned handle is verified before the Session owner FSM
    /// may publish it; ordinary adoption then installs the one live owner.
    pub(crate) async fn restore(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
        store: &dyn pc::SandboxCheckpointStore,
    ) -> Result<pc::SandboxHandle, pc::SandboxError> {
        match self {
            Self::Workdir(provider) => {
                let restored = pc::SandboxProvider::restore(provider, spec, request, store).await?;
                let handle = restored.target().handle();
                request.verify_handle(spec, &handle)?;
                Ok(handle)
            }
            Self::Namespace { .. } => Err(pc::SandboxError::new(
                "namespace provider does not implement checkpoint restore",
            )),
            Self::Container {
                provider,
                extra_mounts,
                ..
            } => {
                let spec = container_spec(spec, extra_mounts)?;
                let restored = provider.restore_environment(&spec, request, store).await?;
                let handle = restored.target().handle();
                request.verify_handle(&spec, &handle)?;
                Ok(handle)
            }
        }
    }

    /// Reconstruct only the lifecycle object needed to finish an exact
    /// terminal realization. This never renews a container lease or starts a
    /// Hand; each provider re-observes `Terminal`/`Disposing` under the supplied
    /// fence and returns the same exact Sandbox owner used by
    /// `dispose_for_effect`. The Host separately projects whether live I/O is
    /// permitted; this port owns physical reconstruction only.
    pub(crate) async fn prepare_terminal_effective_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: Option<&pc::SandboxHandle>,
        expected_effect_fence: Option<&pc::SandboxEffectFence>,
        terminal_effect_fence: &pc::SandboxEffectFence,
    ) -> Result<Option<SessionEnvironment>, pc::SandboxError> {
        match self {
            Self::Workdir(provider) => provider
                .prepare_terminal_sandbox_for_effect(
                    spec,
                    handle,
                    expected_effect_fence,
                    terminal_effect_fence,
                )
                .map(|sandbox| sandbox.map(SessionEnvironment::workdir)),
            Self::Namespace {
                provider,
                hand_factory,
                hand_bin,
                hand_idle_after,
            } => provider
                .prepare_terminal_sandbox_for_effect(
                    spec,
                    handle,
                    expected_effect_fence,
                    terminal_effect_fence,
                )
                .map(|sandbox| {
                    sandbox.map(|sandbox| {
                        SessionEnvironment::namespace(
                            sandbox,
                            hand_factory.clone(),
                            hand_bin,
                            *hand_idle_after,
                        )
                    })
                }),
            Self::Container {
                provider,
                hand_factory,
                hand_bin,
                hand_idle_after,
                hand_residency,
                ..
            } => {
                let capabilities = provider.sandbox_capabilities();
                let environment = provider
                    .prepare_terminal_environment_for_effect(
                        spec,
                        handle,
                        expected_effect_fence,
                        terminal_effect_fence,
                    )
                    .await?;
                let Some(environment) = environment else {
                    return Ok(None);
                };
                SessionEnvironment::container(
                    environment,
                    hand_factory.clone(),
                    hand_bin,
                    *hand_idle_after,
                    *hand_residency,
                    capabilities,
                )
                .await
                .map(Some)
            }
        }
    }

    /// Dispose the exact restore target named by a durable request without
    /// performing restore or reconstructing the target from checkpoint data.
    pub(crate) async fn dispose_restored(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
    ) -> Result<(), pc::SandboxError> {
        match self {
            Self::Workdir(provider) => {
                pc::SandboxProvider::dispose_restored(provider, spec, request).await
            }
            Self::Namespace { .. } => Err(pc::SandboxError::new(
                "namespace provider does not implement exact restored-target disposal",
            )),
            Self::Container {
                provider,
                extra_mounts,
                ..
            } => {
                let spec = container_spec(spec, extra_mounts)?;
                provider.dispose_restored_environment(&spec, request).await
            }
        }
    }
}

fn container_spec(
    spec: &pc::SandboxSpec,
    extra_mounts: &[pc::MountRequirement],
) -> Result<pc::SandboxSpec, pc::SandboxError> {
    let mut spec = spec.clone();
    spec.isolation = pc::IsolationClass::Container;
    spec.mounts.extend(extra_mounts.iter().cloned());
    for mount in &mut spec.mounts {
        if !mount.mount_path.starts_with('/') {
            mount.mount_path = container_files::workspace_path(&mount.mount_path)?;
        }
    }
    Ok(spec)
}
