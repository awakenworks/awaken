// @formal surface obligation=runtime.backend_owned_environment_is_materialized_only_by_the_claimed_worker
// @formal surface obligation=sandbox.admission_requires_requested_capabilities
// @formal surface obligation=session_realization.renewal_preserves_owner_incarnation_epoch_and_expiry_fence
// @formal surface obligation=session_realization.concurrent_phase_drivers_are_serialized_per_session
// @formal reason: Selects the admitted Session Environment provider, installs optional execution only
// @formal reason: after its real probe, and owns claim-fenced create, adoption, observation, restore, and
// @formal reason: container lease-renewal projection. Startup selection reuses SessionExecutionBinding; OS
// @formal reason: probe truth is not a refinement claim.
//! Selection and lifecycle construction for a Session's single environment.

use super::{HandExecutorFactory, SessionEnvironment, container_files};
use crate::deployment_config::ContainerHandResidency;
use awaken_sandbox_local::FilesystemHandleCodec as _;
use std::sync::Arc;

use awaken_provisioning_contract as pc;
use awaken_sandbox_container::ContainerHandleCodec as _;
use awaken_sandbox_local::{LocalProvider, NamespaceProvider};

/// Creates/adopts the one Session environment while auxiliary housekeeping Runs
/// may continue using their deliberately-fresh LocalProvider.
pub(crate) enum SessionEnvironmentProvider {
    Workdir {
        provider: LocalProvider,
        hand: Option<WorkdirHandConfig>,
    },
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

#[derive(Clone)]
pub(crate) struct WorkdirHandConfig {
    pub(super) factory: Arc<dyn HandExecutorFactory>,
    pub(super) binary: String,
    pub(super) idle_after: std::time::Duration,
}

pub(crate) struct SessionEnvironmentAdoptionLayout {
    pub(crate) spec: pc::SandboxSpec,
    pub(crate) historical_owned_paths: Option<Vec<String>>,
}

/// Who owns one Session Environment provider effect. The Session aggregate
/// decides this once (an unowned Environment has no realization lease); every
/// provider projection then takes the matching named operation. A fence always
/// travels with its current authority, so neither an omitted fence nor a fence
/// without authority can select a weaker path.
#[derive(Clone, Copy)]
pub(crate) enum ProviderEffect<'a> {
    /// No aggregate effect owns the Environment.
    Unattached,
    /// The aggregate-owned effect fence and the authority that admits it.
    Fenced {
        fence: &'a pc::SandboxEffectFence,
        authority: &'a dyn pc::CurrentExecutionAuthority,
    },
}

impl<'a> ProviderEffect<'a> {
    /// Project an aggregate-derived optional fence and its captured authority.
    /// A fence without authority is refused rather than degraded.
    pub(crate) fn from_parts(
        fence: Option<&'a pc::SandboxEffectFence>,
        authority: Option<&'a dyn pc::CurrentExecutionAuthority>,
    ) -> Result<Self, pc::SandboxError> {
        match (fence, authority) {
            (Some(fence), Some(authority)) => Ok(Self::Fenced { fence, authority }),
            (None, _) => Ok(Self::Unattached),
            (Some(_), None) => Err(pc::SandboxError::fence_rejected(
                "a fenced Session Environment effect has no current authority",
            )),
        }
    }

    /// The observation basis this effect implies for `spec`.
    pub(crate) const fn observation_basis(
        self,
        spec: &'a pc::SandboxSpec,
    ) -> pc::SandboxObservationBasis<'a> {
        match self {
            Self::Unattached => pc::SandboxObservationBasis::Unattached,
            Self::Fenced { fence, .. } => pc::SandboxObservationBasis::Effect {
                spec,
                effect_fence: fence,
            },
        }
    }

    fn admit(self) -> Result<(), pc::SandboxError> {
        match self {
            Self::Unattached => Ok(()),
            Self::Fenced { fence, authority } => {
                pc::with_current_sandbox_effect(fence, authority, |_| Ok(()))
            }
        }
    }
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
    ) -> Result<Option<Self>, String> {
        let base = base.into();
        Ok(match tier {
            crate::SandboxTier::Local => {
                Some(Self::workdir_with_agent_stderr(base, inherit_agent_stderr))
            }
            crate::SandboxTier::Namespace => {
                let hand_factory = hand_factory
                    .ok_or("namespace Session environments require a hand executor factory")?;
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
        })
    }

    pub(crate) fn supports_host_identity(&self) -> bool {
        matches!(self, Self::Workdir { provider, .. } if provider.has_trusted_host_access())
    }

    pub(crate) fn capabilities(&self) -> pc::SandboxCapabilities {
        match self {
            Self::Workdir { provider, .. } => pc::SandboxProvider::capabilities(provider),
            Self::Namespace { provider, .. } => pc::SandboxProvider::capabilities(provider),
            Self::Container { provider, .. } => provider.sandbox_capabilities(),
        }
    }

    pub(crate) fn workdir(base: impl Into<std::path::PathBuf>) -> Self {
        Self::Workdir {
            provider: LocalProvider::new(base),
            hand: None,
        }
    }

    pub(crate) fn workdir_with_agent_stderr(
        base: impl Into<std::path::PathBuf>,
        inherit: bool,
    ) -> Self {
        Self::Workdir {
            provider: LocalProvider::new(base).with_agent_stderr(inherit),
            hand: None,
        }
    }

    pub(crate) fn trusted_host_workdir_with_agent_stderr(
        base: impl Into<std::path::PathBuf>,
        inherit: bool,
    ) -> Self {
        Self::Workdir {
            provider: LocalProvider::trusted_host(base).with_agent_stderr(inherit),
            hand: None,
        }
    }

    pub(crate) fn install_workdir_hand(
        &mut self,
        factory: Arc<dyn HandExecutorFactory>,
        binary: String,
        idle_after: std::time::Duration,
    ) {
        if let Self::Workdir { hand, .. } = self {
            *hand = Some(WorkdirHandConfig {
                factory,
                binary,
                idle_after,
            });
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) async fn install_scoped_execution(
        &mut self,
        execution: awaken_sandbox_local::LocalExecutionDeployment,
    ) -> Result<(), pc::SandboxError> {
        let Self::Workdir { provider, .. } = self else {
            return Err(pc::SandboxError::unsupported(
                "scoped host execution requires Workdir provider",
            ));
        };
        if !provider.has_trusted_host_access() {
            return Err(pc::SandboxError::unauthorized(
                "scoped host execution requires TrustedHost authorization",
            ));
        }
        if let Some(current) = provider.execution_deployment() {
            return if current == &execution {
                pc::SandboxProvider::probe_ready(provider).await
            } else {
                Err(pc::SandboxError::conflict(
                    "scoped execution deployment changed after installation",
                ))
            };
        }
        execution.probe_ready().await?;
        provider.install_execution_deployment(execution)
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
            Self::Workdir { provider, hand } => {
                let mut relocated = if provider.has_trusted_host_access() {
                    Self::trusted_host_workdir_with_agent_stderr(
                        base,
                        provider.inherits_agent_stderr(),
                    )
                } else {
                    Self::workdir_with_agent_stderr(base, provider.inherits_agent_stderr())
                };
                if let Self::Workdir { hand: target, .. } = &mut relocated {
                    *target = hand.clone();
                }
                #[cfg(target_os = "linux")]
                if let (
                    Some(execution),
                    Self::Workdir {
                        provider: target, ..
                    },
                ) = (provider.execution_deployment(), &mut relocated)
                {
                    target
                        .install_execution_deployment(execution.clone())
                        .expect("root relocation preserves an already-admitted deployment");
                }
                relocated
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
            Self::Workdir { provider, .. } => provider.install_memory_mounter(mounter),
            Self::Namespace { provider, .. } => provider.install_memory_mounter(mounter),
            Self::Container { provider, .. } => provider.install_memory_mounter(mounter),
        }
    }

    pub(crate) fn install_secret_broker(&self, broker: Arc<dyn pc::SecretBroker>) {
        match self {
            Self::Workdir { provider, .. } => provider.install_secret_broker(broker),
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
            Self::Workdir { .. } => Ok(spec.clone()),
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
            Self::Workdir { .. } => {
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
            ProviderEffect::Unattached,
            None,
            awaken_sandbox_container::ContainerRealizationIntent::Create,
        )
        .await
    }

    pub(crate) async fn create_effective_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        effect: ProviderEffect<'_>,
        source_handle: Option<&pc::SandboxHandle>,
        intent: awaken_sandbox_container::ContainerRealizationIntent,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        effect.admit()?;
        let environment = match self {
            Self::Workdir { provider, hand } => match effect {
                ProviderEffect::Fenced { fence, authority } => provider
                    .create_sandbox_for_effect(spec, fence, source_handle, authority)
                    .await
                    .map(|sandbox| {
                        SessionEnvironment::workdir(sandbox).with_workdir_hand(hand.as_ref())
                    }),
                ProviderEffect::Unattached => provider.create_sandbox(spec).await.map(|sandbox| {
                    SessionEnvironment::workdir(sandbox).with_workdir_hand(hand.as_ref())
                }),
            },
            Self::Namespace {
                provider,
                hand_factory,
                hand_bin,
                hand_idle_after,
            } => match effect {
                ProviderEffect::Fenced { fence, authority } => {
                    provider
                        .create_sandbox_for_effect(spec, fence, source_handle, authority)
                        .await
                }
                ProviderEffect::Unattached => provider.create_sandbox(spec).await,
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
                let environment = match effect {
                    ProviderEffect::Fenced { fence, authority } => {
                        provider
                            .create_environment_for_effect(spec, fence, authority, intent)
                            .await?
                    }
                    ProviderEffect::Unattached => {
                        if matches!(
                            intent,
                            awaken_sandbox_container::ContainerRealizationIntent::Rebuild { .. }
                        ) {
                            return Err(pc::SandboxError::fence_rejected(
                                "a container rebuild requires an aggregate effect fence",
                            ));
                        }
                        provider.create_unattached_environment(spec).await?
                    }
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
            }
        }?;
        effect.admit()?;
        Ok(environment)
    }

    pub(crate) async fn prewarm_images(
        &self,
        specs: &[pc::SandboxSpec],
    ) -> Result<(), pc::SandboxError> {
        if let Self::Container { provider, .. } = self {
            let specs = specs
                .iter()
                .map(|spec| self.effective_spec(spec))
                .collect::<Result<Vec<_>, _>>()?;
            provider.prewarm_images(&specs).await?;
        }
        Ok(())
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

    /// The Host has validated the retained lifecycle owner; recheck exact
    /// handle equality before borrowing its unpersisted create-attempt proof.
    /// A matching serialized handle alone cannot mint this proof after restart.
    fn container_observation_adoption<'a>(
        spec: &'a pc::SandboxSpec,
        handle: &'a pc::SandboxHandle,
        retained: Option<&'a super::SessionEnvironment>,
    ) -> Result<awaken_sandbox_container::ContainerEnvironmentAdoption<'a>, pc::SandboxError> {
        let mut adoption =
            awaken_sandbox_container::ContainerEnvironmentAdoption::new(spec, handle);
        if let Some(environment) = retained {
            let super::SessionEnvironment::Container { sandbox, .. } = environment else {
                return Err(pc::SandboxError::conflict(
                    "retained Environment belongs to another provider",
                ));
            };
            if &environment.handle() != handle {
                return Err(pc::SandboxError::conflict(
                    "retained Environment differs from observed handle",
                ));
            }
            adoption.retained_attempt = sandbox.retained_create_attempt();
        }
        Ok(adoption)
    }

    /// Observe one exact durable handle through the exact provider selected
    /// for this Session, without create/adopt/delete effects. This is the sole
    /// Runtime projection of the neutral observation port: `basis` selects an
    /// unattached inspection or a revalidation under the aggregate-owned effect
    /// fence. Backend errors remain indeterminate and are never translated into
    /// replacement authority here. `purpose` is container observation semantics
    /// only and never authorizes a mutation.
    pub(crate) async fn observe_effective(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        retained: Option<&super::SessionEnvironment>,
        basis: pc::SandboxObservationBasis<'_>,
        purpose: awaken_sandbox_container::ContainerObservationPurpose,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        match self {
            Self::Workdir { provider, .. } => {
                pc::SandboxProvider::observe(provider, handle, basis).await
            }
            Self::Namespace { provider, .. } => {
                pc::SandboxProvider::observe(provider, handle, basis).await
            }
            Self::Container { provider, .. } => {
                let mut adoption = Self::container_observation_adoption(spec, handle, retained)?;
                adoption.observation_purpose = purpose;
                provider.observe_environment(adoption, basis).await
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
                Self::Workdir { .. } | Self::Namespace { .. } if physical_incarnation.is_none() => {
                    Ok(())
                }
                Self::Workdir { .. } | Self::Namespace { .. } => Err(pc::SandboxError::conflict(
                    "filesystem sandbox absence carried a foreign physical incarnation",
                )),
                Self::Container { .. } => {
                    let expected = handle.container_physical_incarnation()?;
                    if physical_incarnation.as_deref() == Some(expected.as_str()) {
                        Ok(())
                    } else {
                        Err(pc::SandboxError::conflict(
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
                    Self::Workdir { .. } | Self::Namespace { .. } => {
                        handle.filesystem_physical_incarnation()?.ok_or_else(|| {
                            pc::SandboxError::unsupported(
                                "legacy filesystem handle has no physical incarnation",
                            )
                        })?
                    }
                    Self::Container { .. } => handle.container_physical_incarnation()?,
                };
                if *physical_incarnation == expected {
                    Ok(())
                } else {
                    Err(pc::SandboxError::conflict(
                        "terminal sandbox observation does not match the durable incarnation",
                    ))
                }
            }
            pc::SandboxObservation::Ready
            | pc::SandboxObservation::Provisioning
            | pc::SandboxObservation::Incompatible { .. } => Err(pc::SandboxError::invalid(
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
        self.adopt_effective_for_effect(spec, handle, ProviderEffect::Unattached)
            .await
    }

    /// Rebuild only the revoked process bindings after the lifecycle owner has
    /// authorized and observed this exact retained Sandbox. Its physical owner
    /// carries non-durable create-attempt and cleanup participants that a cold
    /// provider adoption cannot reconstruct. Scoped Local execution is instead
    /// bound once to its original Session generation: reuse the canonical fenced
    /// adoption to obtain a fresh execution object over the SAME root and shared
    /// Memory participants. Never reset the old object's authority or admission.
    /// No branch allocates a new physical Sandbox or launches an Agent.
    pub(crate) async fn rebind_retained(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        retained: &SessionEnvironment,
        effect: ProviderEffect<'_>,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        if &retained.handle() != handle {
            return Err(pc::SandboxError::conflict(
                "retained Environment differs from rebound handle",
            ));
        }
        #[cfg(target_os = "linux")]
        if matches!(retained, SessionEnvironment::Workdir { sandbox, .. } if sandbox.has_scoped_execution())
        {
            if !matches!(self, Self::Workdir { provider, .. } if provider.execution_deployment().is_some())
                || matches!(effect, ProviderEffect::Unattached)
            {
                return Err(pc::SandboxError::fence_rejected(
                    "scoped rebind requires its deployment and current adoption authority",
                ));
            }
            retained.stop_bound_processes().await?;
            return self.adopt_effective_for_effect(spec, handle, effect).await;
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (spec, effect);
        match (self, retained) {
            (Self::Workdir { hand, .. }, SessionEnvironment::Workdir { sandbox, .. }) => {
                Ok(SessionEnvironment::Workdir {
                    sandbox: sandbox.clone(),
                    hand: None,
                }
                .with_workdir_hand(hand.as_ref()))
            }
            (
                Self::Namespace {
                    hand_factory,
                    hand_bin,
                    hand_idle_after,
                    ..
                },
                SessionEnvironment::Namespace { sandbox, .. },
            ) => Ok(SessionEnvironment::namespace_shared(
                sandbox.clone(),
                hand_factory.clone(),
                hand_bin,
                *hand_idle_after,
            )),
            (
                Self::Container {
                    hand_factory,
                    hand_bin,
                    hand_idle_after,
                    hand_residency,
                    ..
                },
                SessionEnvironment::Container {
                    sandbox,
                    capabilities,
                    ..
                },
            ) => {
                SessionEnvironment::container(
                    sandbox.clone(),
                    hand_factory.clone(),
                    hand_bin,
                    *hand_idle_after,
                    *hand_residency,
                    capabilities.clone(),
                )
                .await
            }
            _ => Err(pc::SandboxError::conflict(
                "retained Environment belongs to another provider",
            )),
        }
    }

    /// Adopt the already-observed exact handle under the root-authorized
    /// provider fence. Local/namespace providers revalidate their immutable
    /// marker in `adopt_sandbox`; container adapters additionally fence the
    /// backend observation/lease effect against Worker replacement.
    pub(crate) async fn adopt_effective_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        effect: ProviderEffect<'_>,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        effect.admit()?;
        let environment = match self {
            Self::Workdir { provider, hand } => match effect {
                ProviderEffect::Fenced { fence, authority } => provider
                    .adopt_sandbox_for_effect(spec, handle, fence, authority)
                    .await
                    .map(|sandbox| {
                        SessionEnvironment::workdir(sandbox).with_workdir_hand(hand.as_ref())
                    }),
                ProviderEffect::Unattached => provider
                    .adopt_sandbox_with_spec(spec, handle)
                    .await
                    .map(|sandbox| {
                        SessionEnvironment::workdir(sandbox).with_workdir_hand(hand.as_ref())
                    }),
            },
            Self::Namespace {
                provider,
                hand_factory,
                hand_bin,
                hand_idle_after,
            } => match effect {
                ProviderEffect::Fenced { fence, authority } => {
                    provider
                        .adopt_sandbox_for_effect(spec, handle, fence, authority)
                        .await
                }
                ProviderEffect::Unattached => {
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
                let adoption =
                    awaken_sandbox_container::ContainerEnvironmentAdoption::new(spec, handle);
                let environment = match effect {
                    ProviderEffect::Fenced { fence, authority } => {
                        provider
                            .adopt_environment_for_effect(adoption, fence, authority)
                            .await?
                    }
                    ProviderEffect::Unattached => {
                        provider.adopt_unattached_environment(adoption).await?
                    }
                };
                effect.admit()?;
                environment.renew_lease().await?;
                effect.admit()?;
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
        }?;
        effect.admit()?;
        Ok(environment)
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
            Self::Workdir { provider, .. } => {
                let restored =
                    pc::RestorableSandboxProvider::restore(provider, spec, request, store).await?;
                let handle = restored.target().handle();
                request.verify_handle(spec, &handle)?;
                Ok(handle)
            }
            Self::Namespace { .. } => Err(pc::SandboxError::unsupported(
                "namespace provider does not implement checkpoint restore",
            )),
            Self::Container {
                provider,
                extra_mounts,
                ..
            } => {
                let spec = container_spec(spec, extra_mounts)?;
                let restorable = provider.restorable().ok_or_else(|| {
                    pc::SandboxError::unsupported(
                        "container provider does not implement checkpoint restore",
                    )
                })?;
                let restored = restorable
                    .restore_environment(&spec, request, store)
                    .await?;
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
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<Option<SessionEnvironment>, pc::SandboxError> {
        match self {
            Self::Workdir { provider, hand } => provider
                .prepare_terminal_sandbox_for_effect(
                    spec,
                    handle,
                    expected_effect_fence,
                    terminal_effect_fence,
                    authority,
                )
                .await
                .map(|sandbox| {
                    sandbox.map(|sandbox| {
                        SessionEnvironment::workdir(sandbox).with_workdir_hand(hand.as_ref())
                    })
                }),
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
                    authority,
                )
                .await
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
                        authority,
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
        authorization: &pc::SandboxDisposalAuthorization,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError> {
        match self {
            Self::Workdir { provider, .. } => {
                pc::RestoreTargetSandboxProvider::dispose_restored(
                    provider,
                    spec,
                    request,
                    authorization,
                    authority,
                )
                .await
            }
            Self::Namespace { .. } => Err(pc::SandboxError::unsupported(
                "namespace provider does not implement exact restored-target disposal",
            )),
            Self::Container {
                provider,
                extra_mounts,
                ..
            } => {
                let spec = container_spec(spec, extra_mounts)?;
                provider
                    .restore_targets()
                    .ok_or_else(|| {
                        pc::SandboxError::unsupported(
                            "container provider does not implement exact restored-target disposal",
                        )
                    })?
                    .dispose_restored_environment(&spec, request, authorization, authority)
                    .await
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

#[cfg(test)]
mod provider_effect_tests {
    use super::*;

    #[tokio::test]
    async fn provider_effect_is_named_and_never_degrades() {
        // Decision table for the Host's single ownership projection.
        // | Rule | fence | authority | Effect                                  |
        // | P1   | none  | any       | Unattached; unattached observation      |
        // | P2   | some  | some      | Fenced; effect observation with spec    |
        // | P3   | some  | none      | FenceRejected, never Unattached         |
        let fence = pc::SandboxEffectFence::new("op", "owner", "boot", 1, u64::MAX).unwrap();
        let authority = pc::test_support::EffectAuthority::new(&fence);
        let spec = crate::session_environment::environment_test_spec();
        for authority in [None, Some(&authority as &dyn pc::CurrentExecutionAuthority)] {
            let effect = ProviderEffect::from_parts(None, authority).expect("P1");
            assert!(matches!(effect, ProviderEffect::Unattached), "P1");
            assert!(
                matches!(
                    effect.observation_basis(&spec),
                    pc::SandboxObservationBasis::Unattached
                ),
                "P1 basis"
            );
        }
        let effect = ProviderEffect::from_parts(Some(&fence), Some(&authority)).expect("P2");
        assert!(
            matches!(
                effect.observation_basis(&spec),
                pc::SandboxObservationBasis::Effect { spec: observed, effect_fence }
                    if observed == &spec && effect_fence == &fence
            ),
            "P2 basis"
        );
        assert!(
            matches!(
                ProviderEffect::from_parts(Some(&fence), None),
                Err(pc::SandboxError::FenceRejected(_))
            ),
            "P3"
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod execution_installation_tests {
    use super::*;
    use awaken_sandbox_local::LocalExecutionDeployment as Deployment;

    #[tokio::test]
    async fn failed_probe_cannot_install_and_reentry_cannot_replace_configuration() {
        // I1 unauthorized Workdir -> reject before probe. I2 authorized but
        // failed probe -> no installed config or filesystem effect. I3 different
        // reentry -> reject before replacement; I4 same reentry re-probes, so
        // retained configuration is not a cached readiness/authority receipt.
        // I5 a different provider kind rejects before probe. Negative control:
        // removing the first-install probe must make I2 fail, linking the Rust
        // admission gate to OnlyProbedDeploymentSelected in the existing model.
        let root = tempfile::tempdir().unwrap();
        let deployment =
            Deployment::new("/missing-launcher".into(), "/missing-helper".into()).unwrap();
        let mut namespace = SessionEnvironmentProvider::namespace_with_agent_stderr(
            root.path(),
            false,
            Arc::new(crate::session_environment::UnusedHandExecutorFactory),
            "/missing-helper",
            std::time::Duration::from_secs(1),
        );
        assert!(
            namespace
                .install_scoped_execution(deployment.clone())
                .await
                .unwrap_err()
                .to_string()
                .contains("requires Workdir provider")
        );
        let mut untrusted = SessionEnvironmentProvider::workdir(root.path());
        assert!(
            untrusted
                .install_scoped_execution(deployment.clone())
                .await
                .unwrap_err()
                .to_string()
                .contains("TrustedHost authorization")
        );
        let mut owner =
            SessionEnvironmentProvider::trusted_host_workdir_with_agent_stderr(root.path(), false);
        assert!(
            owner
                .install_scoped_execution(deployment.clone())
                .await
                .is_err()
        );
        let SessionEnvironmentProvider::Workdir { provider, .. } = &mut owner else {
            unreachable!()
        };
        assert!(provider.execution_deployment().is_none());
        provider
            .install_execution_deployment(deployment.clone())
            .unwrap();
        let different =
            Deployment::new("/another-launcher".into(), "/another-helper".into()).unwrap();
        assert!(
            owner
                .install_scoped_execution(different)
                .await
                .unwrap_err()
                .to_string()
                .contains("changed after installation")
        );
        assert!(
            owner
                .install_scoped_execution(deployment.clone())
                .await
                .is_err()
        );
        let SessionEnvironmentProvider::Workdir { provider, .. } = &owner else {
            unreachable!()
        };
        assert_eq!(provider.execution_deployment(), Some(&deployment));
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
}
