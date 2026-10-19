// @formal surface obligation=runtime.backend_owned_environment_is_materialized_only_by_the_claimed_worker
// @formal surface obligation=sandbox.admission_requires_requested_capabilities
// @formal surface obligation=session_realization.renewal_preserves_owner_incarnation_epoch_and_expiry_fence
// @formal surface obligation=session_realization.concurrent_phase_drivers_are_serialized_per_session
// @formal reason: Selects the admitted Session Environment provider, installs optional execution only
// @formal reason: after its real probe, and owns claim-fenced create, adoption, observation, restore, and
// @formal reason: container lease-renewal projection. Startup selection reuses SessionExecutionBinding; OS
// @formal reason: probe truth is not a refinement claim.
//! Selection and lifecycle construction for a Session's single environment.

use super::{HandExecutorFactory, SandboxEnvironment, SessionEnvironment, container_files};
use awaken_session_environment_contract::ContainerHandResidency;
use awaken_session_environment_contract::EnvironmentRealizationKind;
use std::sync::Arc;

use awaken_provisioning_contract as pc;
use awaken_sandbox_local::{LocalProvider, NamespaceProvider};

/// Creates/adopts the one Session environment while auxiliary housekeeping Runs
/// may continue using their deliberately-fresh LocalProvider.
pub enum SandboxEnvironmentProvider {
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
        provider: Arc<
            dyn awaken_provisioning_contract::SandboxProvider<
                    Environment = dyn awaken_provisioning_contract::SessionSandbox,
                >,
        >,
        capacity: Option<Arc<dyn awaken_provisioning_contract::SandboxCapacity>>,
        extra_mounts: Vec<pc::MountRequirement>,
        hand_factory: Arc<dyn HandExecutorFactory>,
        hand_bin: String,
        hand_idle_after: std::time::Duration,
        hand_residency: ContainerHandResidency,
        /// The Host's process clock; container Repository credentials are
        /// stamped and judged on it.
        clock: Arc<dyn awaken_time::WallClock>,
    },
}

/// The Hand a TrustedHost Workdir environment binds; configured once by the
/// process composition through
/// [`SandboxEnvironmentProvider::install_workdir_hand`].
#[derive(Clone)]
pub struct WorkdirHandConfig {
    pub(super) factory: Arc<dyn HandExecutorFactory>,
    pub(super) binary: String,
    pub(super) idle_after: std::time::Duration,
}

use awaken_provisioning_contract::SandboxEffect;
pub(crate) use awaken_session_environment_contract::EnvironmentAdoptionLayout as SessionEnvironmentAdoptionLayout;

impl SandboxEnvironmentProvider {
    /// Whether a TrustedHost Workdir Hand is installed, for the composition
    /// that configured it to verify its startup.
    #[must_use]
    pub fn has_workdir_hand(&self) -> bool {
        matches!(self, Self::Workdir { hand: Some(_), .. })
    }

    /// Whether scoped host execution is installed, for the composition that
    /// configured it to verify its startup.
    #[cfg(target_os = "linux")]
    #[must_use]
    pub fn has_scoped_execution(&self) -> bool {
        matches!(self, Self::Workdir { provider, .. } if provider.execution_deployment().is_some())
    }

    pub(crate) fn supports_host_identity(&self) -> bool {
        matches!(self, Self::Workdir { provider, .. } if provider.has_trusted_host_access())
    }

    pub(crate) fn capabilities(&self) -> pc::SandboxCapabilities {
        match self {
            Self::Workdir { provider, .. } => pc::SandboxReadiness::capabilities(provider),
            Self::Namespace { provider, .. } => pc::SandboxReadiness::capabilities(provider),
            Self::Container { provider, .. } => provider.capabilities(),
        }
    }

    pub fn workdir(base: impl Into<std::path::PathBuf>) -> Self {
        Self::Workdir {
            provider: LocalProvider::new(base),
            hand: None,
        }
    }

    pub fn workdir_with_agent_stderr(base: impl Into<std::path::PathBuf>, inherit: bool) -> Self {
        Self::Workdir {
            provider: LocalProvider::new(base).with_agent_stderr(inherit),
            hand: None,
        }
    }

    pub fn trusted_host_workdir_with_agent_stderr(
        base: impl Into<std::path::PathBuf>,
        inherit: bool,
    ) -> Self {
        Self::Workdir {
            provider: LocalProvider::trusted_host(base).with_agent_stderr(inherit),
            hand: None,
        }
    }

    pub fn install_workdir_hand(
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
    pub async fn install_scoped_execution(
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
                pc::SandboxReadiness::probe_ready(provider).await
            } else {
                Err(pc::SandboxError::conflict(
                    "scoped execution deployment changed after installation",
                ))
            };
        }
        execution.probe_ready().await?;
        provider.install_execution_deployment(execution)
    }

    pub fn namespace_with_agent_stderr(
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

    // Every argument is one independently configured part of the one container
    // Session provider the startup composition installs.
    #[allow(clippy::too_many_arguments)]
    pub fn container_with_capacity_hand_idle_and_residency(
        provider: Arc<
            dyn awaken_provisioning_contract::SandboxProvider<
                    Environment = dyn awaken_provisioning_contract::SessionSandbox,
                >,
        >,
        capacity: Option<Arc<dyn awaken_provisioning_contract::SandboxCapacity>>,
        extra_mounts: Vec<pc::MountRequirement>,
        hand_factory: Arc<dyn HandExecutorFactory>,
        hand_bin: impl Into<String>,
        hand_idle_after: std::time::Duration,
        hand_residency: ContainerHandResidency,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self::Container {
            provider,
            capacity,
            extra_mounts,
            hand_factory,
            hand_bin: hand_bin.into(),
            hand_idle_after,
            hand_residency,
            clock,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn at_root(&self, base: impl Into<std::path::PathBuf>) -> Self {
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
                clock,
            } => Self::Container {
                provider: provider.clone(),
                capacity: capacity.clone(),
                extra_mounts: extra_mounts.clone(),
                hand_factory: hand_factory.clone(),
                hand_bin: hand_bin.clone(),
                hand_idle_after: *hand_idle_after,
                hand_residency: *hand_residency,
                clock: clock.clone(),
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
        let spec = self.effective_spec(spec)?;
        let layout = match self {
            Self::Workdir { provider, .. } => {
                pc::SandboxProvider::adoption_layout(provider, handle)?
            }
            Self::Namespace { provider, .. } => {
                pc::SandboxProvider::adoption_layout(provider, handle)?
            }
            Self::Container { provider, .. } => provider.adoption_layout(handle)?,
        };
        let spec = layout.apply_to(spec);
        Ok(SessionEnvironmentAdoptionLayout {
            spec,
            historical_owned_paths: handle.owned_paths().map(<[String]>::to_vec),
        })
    }

    pub(crate) async fn create_effective_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        effect: SandboxEffect<'_>,
        source_handle: Option<&pc::SandboxHandle>,
        kind: EnvironmentRealizationKind,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        effect.admit()?;
        let environment = match self {
            Self::Workdir { provider, hand } => match effect {
                SandboxEffect::Fenced { fence, authority } => provider
                    .create_sandbox_for_effect(spec, fence, source_handle, authority)
                    .await
                    .map(|sandbox| {
                        SandboxEnvironment::workdir(sandbox).with_workdir_hand(hand.as_ref())
                    }),
                SandboxEffect::Unattached => provider.create_sandbox(spec).await.map(|sandbox| {
                    SandboxEnvironment::workdir(sandbox).with_workdir_hand(hand.as_ref())
                }),
            },
            Self::Namespace {
                provider,
                hand_factory,
                hand_bin,
                hand_idle_after,
            } => match effect {
                SandboxEffect::Fenced { fence, authority } => {
                    provider
                        .create_sandbox_for_effect(spec, fence, source_handle, authority)
                        .await
                }
                SandboxEffect::Unattached => provider.create_sandbox(spec).await,
            }
            .map(|sandbox| {
                SandboxEnvironment::namespace(
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
                clock,
                ..
            } => {
                let capabilities = provider.capabilities();
                // The container provider decodes its own source handle into the
                // exact physical incarnation a Rebuild replaces; Workdir and
                // Namespace rebuilds are owned by their realization-marker path.
                let intent = match kind {
                    EnvironmentRealizationKind::Create => {
                        awaken_provisioning_contract::SandboxRealizationIntent::Create
                    }
                    EnvironmentRealizationKind::Rebuild => {
                        let source = source_handle.ok_or_else(|| {
                            pc::SandboxError::invalid(
                                "Container Environment rebuild lacks its source binding",
                            )
                        })?;
                        awaken_provisioning_contract::SandboxRealizationIntent::Rebuild {
                            source: Box::new(source.clone()),
                        }
                    }
                };
                let environment = match effect {
                    SandboxEffect::Fenced { fence, authority } => {
                        provider
                            .claim_for_effect(
                                spec,
                                awaken_provisioning_contract::SandboxEffect::Fenced {
                                    fence,
                                    authority,
                                },
                                intent,
                            )
                            .await?
                    }
                    SandboxEffect::Unattached => {
                        if matches!(
                            intent,
                            awaken_provisioning_contract::SandboxRealizationIntent::Rebuild { .. }
                        ) {
                            return Err(pc::SandboxError::fence_rejected(
                                "a container rebuild requires an aggregate effect fence",
                            ));
                        }
                        provider
                            .claim_for_effect(
                                spec,
                                awaken_provisioning_contract::SandboxEffect::Unattached,
                                awaken_provisioning_contract::SandboxRealizationIntent::Create,
                            )
                            .await?
                    }
                };
                SandboxEnvironment::container(
                    environment,
                    hand_factory.clone(),
                    hand_bin,
                    *hand_idle_after,
                    *hand_residency,
                    capabilities,
                    clock.clone(),
                )
                .await
            }
        }?;
        effect.admit()?;
        Ok(environment.into_session_environment())
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

    /// The Host has validated the retained lifecycle owner; recheck exact
    /// handle equality before borrowing its unpersisted create-attempt proof.
    /// A matching serialized handle alone cannot mint this proof after restart.
    fn container_observation_adoption<'a>(
        spec: &'a pc::SandboxSpec,
        handle: &'a pc::SandboxHandle,
        retained: Option<&'a super::SessionEnvironment>,
        allow_monotonic_retained: bool,
    ) -> Result<awaken_provisioning_contract::SandboxAdoption<'a>, pc::SandboxError> {
        let mut adoption = awaken_provisioning_contract::SandboxAdoption::new(spec, handle);
        if let Some(environment) = retained {
            let environment = own_environment(environment)?;
            let super::SandboxEnvironment::Container { sandbox, .. } = environment else {
                return Err(pc::SandboxError::conflict(
                    "retained Environment belongs to another provider",
                ));
            };
            let retained_handle = environment.handle();
            if &retained_handle != handle
                && !(allow_monotonic_retained
                    && retained_handle.owned_paths_are_monotonic_to(handle))
            {
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
        purpose: pc::SandboxObservationPurpose,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        match self {
            Self::Workdir { provider, .. } => {
                pc::SandboxProvider::observe(
                    provider,
                    pc::SandboxAdoption::new(spec, handle),
                    basis,
                )
                .await
            }
            Self::Namespace { provider, .. } => {
                pc::SandboxProvider::observe(
                    provider,
                    pc::SandboxAdoption::new(spec, handle),
                    basis,
                )
                .await
            }
            Self::Container { provider, .. } => {
                let mut adoption = Self::container_observation_adoption(
                    spec,
                    handle,
                    retained,
                    purpose == pc::SandboxObservationPurpose::Recovery,
                )?;
                adoption.observation_purpose = purpose;
                provider.observe(adoption, basis).await
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
        match self {
            Self::Workdir { provider, .. } => {
                pc::SandboxProvider::validate_closed_observation(provider, handle, observation)
            }
            Self::Namespace { provider, .. } => {
                pc::SandboxProvider::validate_closed_observation(provider, handle, observation)
            }
            Self::Container { provider, .. } => {
                provider.validate_closed_observation(handle, observation)
            }
        }
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
        effect: SandboxEffect<'_>,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        let retained_role = retained;
        let retained = own_environment(retained)?;
        let retained_handle = retained.handle();
        if &retained_handle != handle {
            if matches!(self, Self::Container { .. })
                && retained_handle.owned_paths_are_monotonic_to(handle)
            {
                retained.stop_bound_processes().await?;
                return self
                    .adopt_effective_with_retained(spec, handle, Some(retained_role), effect)
                    .await;
            }
            return Err(pc::SandboxError::conflict(
                "retained Environment differs from rebound handle",
            ));
        }
        #[cfg(target_os = "linux")]
        if matches!(retained, SandboxEnvironment::Workdir { sandbox, .. } if sandbox.has_scoped_execution())
        {
            if !matches!(self, Self::Workdir { provider, .. } if provider.execution_deployment().is_some())
                || matches!(effect, SandboxEffect::Unattached)
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
        let rebound = match (self, retained) {
            (Self::Workdir { hand, .. }, SandboxEnvironment::Workdir { sandbox, .. }) => {
                Ok(SandboxEnvironment::Workdir {
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
                SandboxEnvironment::Namespace { sandbox, .. },
            ) => Ok(SandboxEnvironment::namespace_shared(
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
                    clock,
                    ..
                },
                SandboxEnvironment::Container {
                    sandbox,
                    capabilities,
                    ..
                },
            ) => {
                SandboxEnvironment::container(
                    sandbox.clone(),
                    hand_factory.clone(),
                    hand_bin,
                    *hand_idle_after,
                    *hand_residency,
                    capabilities.clone(),
                    clock.clone(),
                )
                .await
            }
            _ => Err(pc::SandboxError::conflict(
                "retained Environment belongs to another provider",
            )),
        }?;
        Ok(rebound.into_session_environment())
    }

    /// Adopt the already-observed exact handle under the root-authorized
    /// provider fence. Local/namespace providers revalidate their immutable
    /// marker in `adopt_sandbox`; container adapters additionally fence the
    /// backend observation/lease effect against Worker replacement.
    pub(crate) async fn adopt_effective_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        effect: SandboxEffect<'_>,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        self.adopt_effective_with_retained(spec, handle, None, effect)
            .await
    }

    async fn adopt_effective_with_retained(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        retained: Option<&SessionEnvironment>,
        effect: SandboxEffect<'_>,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        effect.admit()?;
        let environment = match self {
            Self::Workdir { provider, hand } => match effect {
                SandboxEffect::Fenced { fence, authority } => provider
                    .adopt_sandbox_for_effect(spec, handle, fence, authority)
                    .await
                    .map(|sandbox| {
                        SandboxEnvironment::workdir(sandbox).with_workdir_hand(hand.as_ref())
                    }),
                SandboxEffect::Unattached => provider
                    .adopt_sandbox_with_spec(spec, handle)
                    .await
                    .map(|sandbox| {
                        SandboxEnvironment::workdir(sandbox).with_workdir_hand(hand.as_ref())
                    }),
            },
            Self::Namespace {
                provider,
                hand_factory,
                hand_bin,
                hand_idle_after,
            } => match effect {
                SandboxEffect::Fenced { fence, authority } => {
                    provider
                        .adopt_sandbox_for_effect(spec, handle, fence, authority)
                        .await
                }
                SandboxEffect::Unattached => {
                    provider
                        .adopt_sandbox_with_control_services(handle, &spec.control_services)
                        .await
                }
            }
            .map(|sandbox| {
                SandboxEnvironment::namespace(
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
                clock,
                ..
            } => {
                let capabilities = provider.capabilities();
                let adoption = Self::container_observation_adoption(spec, handle, retained, true)?;
                let environment = match effect {
                    SandboxEffect::Fenced { fence, authority } => {
                        provider
                            .adopt_for_effect(
                                adoption,
                                awaken_provisioning_contract::SandboxEffect::Fenced {
                                    fence,
                                    authority,
                                },
                            )
                            .await?
                    }
                    SandboxEffect::Unattached => {
                        provider
                            .adopt_for_effect(
                                adoption,
                                awaken_provisioning_contract::SandboxEffect::Unattached,
                            )
                            .await?
                    }
                };
                effect.admit()?;
                environment.renew_lease().await?;
                effect.admit()?;
                SandboxEnvironment::container(
                    environment,
                    hand_factory.clone(),
                    hand_bin,
                    *hand_idle_after,
                    *hand_residency,
                    capabilities,
                    clock.clone(),
                )
                .await
            }
        }?;
        effect.admit()?;
        Ok(environment.into_session_environment())
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
                    pc::RestorableProvider::<std::sync::Arc<dyn pc::Sandbox>>::restore_environment(
                        provider, spec, request, store,
                    )
                    .await?;
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
                let restorable = provider.restoration().restorable().map_err(|_| {
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
        let prepared = match self {
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
                        SandboxEnvironment::workdir(sandbox).with_workdir_hand(hand.as_ref())
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
                        SandboxEnvironment::namespace(
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
                clock,
                ..
            } => {
                let capabilities = provider.capabilities();
                let environment = provider
                    .prepare_terminal_for_effect(
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
                SandboxEnvironment::container(
                    environment,
                    hand_factory.clone(),
                    hand_bin,
                    *hand_idle_after,
                    *hand_residency,
                    capabilities,
                    clock.clone(),
                )
                .await
                .map(Some)
            }
        }?;
        Ok(prepared.map(SandboxEnvironment::into_session_environment))
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
                pc::RestoreTargetProvider::<std::sync::Arc<dyn pc::Sandbox>>::dispose_restored(
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
                    .restoration()
                    .targets()
                    .map_err(|_| {
                        pc::SandboxError::unsupported(
                            "container provider does not implement exact restored-target disposal",
                        )
                    })?
                    .dispose_restored(&spec, request, authorization, authority)
                    .await
            }
        }
    }
}

impl SandboxEnvironmentProvider {
    /// Hand this provider to the Host as one handle per consuming role.
    #[must_use]
    pub fn into_session_provider(
        self,
    ) -> awaken_session_environment_contract::SessionEnvironmentProvider {
        awaken_session_environment_contract::SessionEnvironmentProvider::new(Arc::new(self))
    }
}

#[async_trait::async_trait]
impl awaken_session_environment_contract::EnvironmentRealizer for SandboxEnvironmentProvider {
    fn capabilities(&self) -> pc::SandboxCapabilities {
        SandboxEnvironmentProvider::capabilities(self)
    }

    fn supports_host_identity(&self) -> bool {
        SandboxEnvironmentProvider::supports_host_identity(self)
    }

    fn effective_spec(&self, spec: &pc::SandboxSpec) -> Result<pc::SandboxSpec, pc::SandboxError> {
        SandboxEnvironmentProvider::effective_spec(self, spec)
    }

    async fn create_effective_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        effect: SandboxEffect<'_>,
        source_handle: Option<&pc::SandboxHandle>,
        kind: EnvironmentRealizationKind,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        SandboxEnvironmentProvider::create_effective_for_effect(
            self,
            spec,
            effect,
            source_handle,
            kind,
        )
        .await
    }
}

#[async_trait::async_trait]
impl awaken_session_environment_contract::EnvironmentAdoption for SandboxEnvironmentProvider {
    fn effective_adoption_layout(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
    ) -> Result<SessionEnvironmentAdoptionLayout, pc::SandboxError> {
        SandboxEnvironmentProvider::effective_adoption_layout(self, spec, handle)
    }

    async fn observe_effective(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        retained: Option<&SessionEnvironment>,
        basis: pc::SandboxObservationBasis<'_>,
        purpose: pc::SandboxObservationPurpose,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        SandboxEnvironmentProvider::observe_effective(self, spec, handle, retained, basis, purpose)
            .await
    }

    fn validate_closed_observation(
        &self,
        handle: &pc::SandboxHandle,
        observation: &pc::SandboxObservation,
    ) -> Result<(), pc::SandboxError> {
        SandboxEnvironmentProvider::validate_closed_observation(self, handle, observation)
    }

    async fn adopt_effective_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        effect: SandboxEffect<'_>,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        SandboxEnvironmentProvider::adopt_effective_for_effect(self, spec, handle, effect).await
    }

    async fn rebind_retained(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        retained: &SessionEnvironment,
        effect: SandboxEffect<'_>,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        SandboxEnvironmentProvider::rebind_retained(self, spec, handle, retained, effect).await
    }
}

#[async_trait::async_trait]
impl awaken_session_environment_contract::EnvironmentTerminalRealization
    for SandboxEnvironmentProvider
{
    fn terminal_data_is_readable(&self) -> bool {
        !matches!(self, Self::Container { .. })
    }

    async fn prepare_terminal_effective_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: Option<&pc::SandboxHandle>,
        expected_effect_fence: Option<&pc::SandboxEffectFence>,
        terminal_effect_fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<Option<SessionEnvironment>, pc::SandboxError> {
        SandboxEnvironmentProvider::prepare_terminal_effective_for_effect(
            self,
            spec,
            handle,
            expected_effect_fence,
            terminal_effect_fence,
            authority,
        )
        .await
    }

    async fn restore(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
        store: &dyn pc::SandboxCheckpointStore,
    ) -> Result<pc::SandboxHandle, pc::SandboxError> {
        SandboxEnvironmentProvider::restore(self, spec, request, store).await
    }

    async fn dispose_restored(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
        authorization: &pc::SandboxDisposalAuthorization,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError> {
        SandboxEnvironmentProvider::dispose_restored(self, spec, request, authorization, authority)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_environment_contract::EnvironmentCapacity for SandboxEnvironmentProvider {
    async fn prewarm_images(&self, specs: &[pc::SandboxSpec]) -> Result<(), pc::SandboxError> {
        SandboxEnvironmentProvider::prewarm_images(self, specs).await
    }

    async fn prewarm(
        &self,
        spec: &pc::SandboxSpec,
        target: usize,
    ) -> Result<usize, pc::SandboxError> {
        SandboxEnvironmentProvider::prewarm(self, spec, target).await
    }

    async fn discard_capacity(&self, spec: &pc::SandboxSpec) {
        SandboxEnvironmentProvider::discard_capacity(self, spec).await;
    }

    fn ready_capacity(&self, spec: &pc::SandboxSpec) -> usize {
        SandboxEnvironmentProvider::ready_capacity(self, spec)
    }

    async fn shutdown_capacity(&self) {
        SandboxEnvironmentProvider::shutdown_capacity(self).await;
    }
}

impl awaken_session_environment_contract::EnvironmentProviderInstallation
    for SandboxEnvironmentProvider
{
    fn install_memory_mounter(&self, mounter: Arc<dyn pc::MemoryMounter>) {
        SandboxEnvironmentProvider::install_memory_mounter(self, mounter);
    }

    fn install_secret_broker(&self, broker: Arc<dyn pc::SecretBroker>) {
        SandboxEnvironmentProvider::install_secret_broker(self, broker);
    }
}

/// Recover this provider family's own adapter from the handles the Host holds.
/// An environment realized by any other provider is refused.
fn own_environment(
    environment: &SessionEnvironment,
) -> Result<&SandboxEnvironment, pc::SandboxError> {
    environment
        .origin()
        .downcast_ref::<SandboxEnvironment>()
        .ok_or_else(|| {
            pc::SandboxError::conflict("retained Environment belongs to another provider")
        })
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
        let spec = crate::test_support::spec();
        for authority in [None, Some(&authority as &dyn pc::CurrentExecutionAuthority)] {
            let effect = SandboxEffect::from_parts(None, authority).expect("P1");
            assert!(matches!(effect, SandboxEffect::Unattached), "P1");
            assert!(
                matches!(
                    effect.observation_basis(&spec),
                    pc::SandboxObservationBasis::Unattached
                ),
                "P1 basis"
            );
        }
        let effect = SandboxEffect::from_parts(Some(&fence), Some(&authority)).expect("P2");
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
                SandboxEffect::from_parts(Some(&fence), None),
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
        let mut namespace = SandboxEnvironmentProvider::namespace_with_agent_stderr(
            root.path(),
            false,
            Arc::new(crate::test_support::UnusedHandExecutorFactory),
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
        let mut untrusted = SandboxEnvironmentProvider::workdir(root.path());
        assert!(
            untrusted
                .install_scoped_execution(deployment.clone())
                .await
                .unwrap_err()
                .to_string()
                .contains("TrustedHost authorization")
        );
        let mut owner =
            SandboxEnvironmentProvider::trusted_host_workdir_with_agent_stderr(root.path(), false);
        assert!(
            owner
                .install_scoped_execution(deployment.clone())
                .await
                .is_err()
        );
        let SandboxEnvironmentProvider::Workdir { provider, .. } = &mut owner else {
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
        let SandboxEnvironmentProvider::Workdir { provider, .. } = &owner else {
            unreachable!()
        };
        assert_eq!(provider.execution_deployment(), Some(&deployment));
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
}
