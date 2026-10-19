//! Warm Environment capacity on the `RealizationExecutor`: which requests reach the
//! installed provider's capacity owner and which are refused before it.

use awaken_session_realization::peers::EnvironmentPeer as _;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::SandboxEffect;
use awaken_session_environment_contract::{
    EnvironmentAdoption, EnvironmentAdoptionLayout, EnvironmentCapacity,
    EnvironmentProviderInstallation, EnvironmentRealizationKind, EnvironmentRealizer,
    EnvironmentTerminalRealization, SessionEnvironment, SessionEnvironmentProvider,
};

/// Delegates every environment role to a real workdir provider and records the
/// capacity requests that reach it; `fail` makes every prewarm fail.
struct RecordingCapacity {
    inner: SessionEnvironmentProvider,
    prewarmed: Mutex<Vec<(Option<pc::SandboxCapacityShapeId>, usize)>>,
    images: Mutex<Vec<usize>>,
    fail: bool,
}

#[async_trait]
impl EnvironmentRealizer for RecordingCapacity {
    fn capabilities(&self) -> pc::SandboxCapabilities {
        self.inner.realizer().capabilities()
    }

    fn supports_host_identity(&self) -> bool {
        self.inner.realizer().supports_host_identity()
    }

    fn effective_spec(&self, spec: &pc::SandboxSpec) -> Result<pc::SandboxSpec, pc::SandboxError> {
        self.inner.realizer().effective_spec(spec)
    }

    async fn create_effective_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        effect: SandboxEffect<'_>,
        source_handle: Option<&pc::SandboxHandle>,
        kind: EnvironmentRealizationKind,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        self.inner
            .realizer()
            .create_effective_for_effect(spec, effect, source_handle, kind)
            .await
    }
}

#[async_trait]
impl EnvironmentAdoption for RecordingCapacity {
    fn effective_adoption_layout(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
    ) -> Result<EnvironmentAdoptionLayout, pc::SandboxError> {
        self.inner
            .adoption()
            .effective_adoption_layout(spec, handle)
    }

    async fn observe_effective(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        retained: Option<&SessionEnvironment>,
        basis: pc::SandboxObservationBasis<'_>,
        purpose: pc::SandboxObservationPurpose,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        self.inner
            .adoption()
            .observe_effective(spec, handle, retained, basis, purpose)
            .await
    }

    fn validate_closed_observation(
        &self,
        handle: &pc::SandboxHandle,
        observation: &pc::SandboxObservation,
    ) -> Result<(), pc::SandboxError> {
        self.inner
            .adoption()
            .validate_closed_observation(handle, observation)
    }

    async fn adopt_effective_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        effect: SandboxEffect<'_>,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        self.inner
            .adoption()
            .adopt_effective_for_effect(spec, handle, effect)
            .await
    }

    async fn rebind_retained(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        retained: &SessionEnvironment,
        effect: SandboxEffect<'_>,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        self.inner
            .adoption()
            .rebind_retained(spec, handle, retained, effect)
            .await
    }
}

#[async_trait]
impl EnvironmentTerminalRealization for RecordingCapacity {
    fn terminal_data_is_readable(&self) -> bool {
        self.inner.terminal().terminal_data_is_readable()
    }

    async fn prepare_terminal_effective_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: Option<&pc::SandboxHandle>,
        expected_effect_fence: Option<&pc::SandboxEffectFence>,
        terminal_effect_fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<Option<SessionEnvironment>, pc::SandboxError> {
        self.inner
            .terminal()
            .prepare_terminal_effective_for_effect(
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
        self.inner.terminal().restore(spec, request, store).await
    }

    async fn dispose_restored(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
        authorization: &pc::SandboxDisposalAuthorization,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError> {
        self.inner
            .terminal()
            .dispose_restored(spec, request, authorization, authority)
            .await
    }
}

#[async_trait]
impl EnvironmentCapacity for RecordingCapacity {
    async fn prewarm_images(&self, specs: &[pc::SandboxSpec]) -> Result<(), pc::SandboxError> {
        self.images.lock().unwrap().push(specs.len());
        Ok(())
    }

    async fn prewarm(
        &self,
        spec: &pc::SandboxSpec,
        target: usize,
    ) -> Result<usize, pc::SandboxError> {
        if self.fail {
            return Err(pc::SandboxError::not_ready("capacity owner unavailable"));
        }
        self.prewarmed
            .lock()
            .unwrap()
            .push((pc::SandboxCapacityShapeId::from_spec(spec), target));
        Ok(target)
    }

    async fn discard_capacity(&self, _spec: &pc::SandboxSpec) {}

    fn ready_capacity(&self, _spec: &pc::SandboxSpec) -> usize {
        7
    }

    async fn shutdown_capacity(&self) {}
}

impl EnvironmentProviderInstallation for RecordingCapacity {
    fn install_memory_mounter(&self, mounter: Arc<dyn pc::MemoryMounter>) {
        self.inner.installation().install_memory_mounter(mounter);
    }

    fn install_secret_broker(&self, broker: Arc<dyn pc::SecretBroker>) {
        self.inner.installation().install_secret_broker(broker);
    }
}

fn executor(
    runs_local_dispatch_pool: bool,
    warm_pool: crate::WarmPool,
    fail: bool,
) -> (
    crate::realization_executor::RealizationExecutor,
    Arc<RecordingCapacity>,
) {
    let root = std::env::temp_dir().join(format!("capacity-{}", uuid::Uuid::new_v4().simple()));
    let recording = Arc::new(RecordingCapacity {
        inner: crate::uninstalled_environment::default_session_provider(root.clone()),
        prewarmed: Mutex::default(),
        images: Mutex::default(),
        fail,
    });
    let sessions = Arc::new(crate::installed_session::InstalledSessions::default()).realizations();
    let env = crate::environment_providers::EnvironmentProviders::new(
        root,
        SessionEnvironmentProvider::new(recording.clone()),
        crate::auxiliary_sandbox::default_auxiliary_sandboxes(),
        crate::TrustedHostPolicy::Refused,
        warm_pool,
    );
    (
        crate::realization_executor::RealizationExecutor::new(
            awaken_time::system_clock(),
            sessions.clone(),
            crate::environment_holding::EnvironmentHolding::new(
                env,
                runs_local_dispatch_pool,
                sessions.clone(),
                None,
                None,
                crate::session_publication::SessionPublicationSelection::new(
                    sessions.clone(),
                    "capacity-workspace".into(),
                    None,
                ),
            ),
            crate::resource_staging::ResourceStaging::new(
                sessions.clone(),
                crate::resource_staging::ResourceSources {
                    bundle_source: None,
                    file_content_source: Arc::new(
                        awaken_resource_contract::UnavailableFileContentSource,
                    ),
                    memory: crate::memory_stores::open(None, awaken_time::system_clock()),
                    resource_validator: None,
                    repository_binding_verifier: None,
                    fetches_resources_upstream: false,
                    credential_materializer: None,
                    memory_reference_encoder: None,
                },
            ),
            crate::terminal_effects::TerminalEffects::new(
                sessions.clone(),
                Arc::new(crate::artifact_harvest::ArtifactHarvester::new(
                    sessions.clone(),
                    "capacity-workspace".into(),
                    Arc::new(awaken_resource_contract::UnavailableArtifactPublisher),
                )),
            ),
            crate::execution_authority::ExecutionAuthority::new(sessions, None),
        ),
        recording,
    )
}

#[tokio::test]
async fn warm_capacity_reaches_the_provider_only_on_a_local_pool_with_a_target() {
    // Ownership move: the existing provider component owns warm capacity;
    // canonical Host construction supplies its final local-pool bit once.
    // W1-W6 must still reach that exact recording provider through the narrowed
    // capacity entry point, without a second warmup implementation or flag.
    // Decision table. C1 local dispatch pool; C2 warm pool on; C3 target > 0;
    // C4 Environment self-hosted. E1 the provider prewarms the exact shape the
    // Worker receipts publish; E2 nothing reaches the provider and 0 is ready.
    // | Row | C1 | C2 | C3 | C4  | effect                                 |
    // | W1  | no | on | >0 | no  | E2; limits (0, 0)                      |
    // | W2  | yes| off| >0 | no  | E2; limits (0, 0)                      |
    // | W3  | yes| on | 0  | no  | E2; limits are the pool's              |
    // | W4  | yes| on | >0 | yes | E2 for the Environment                 |
    // | W5  | yes| on | >0 | no  | E1 for the default and the Environment |
    // | W6  | yes| on | >0 | no, provider fails | the failure surfaces, never 0 |
    let on = crate::WarmPool::new(2, 4);
    let environment = crate::host::eager_environment();
    let mut self_hosted = environment.clone();
    self_hosted.self_hosted = true;

    for (row, local, pool) in [("W1", false, on), ("W2", true, crate::WarmPool::Disabled)] {
        let (executor, recording) = executor(local, pool, false);
        assert_eq!(
            executor
                .environments()
                .providers()
                .environment_warmup_limits(),
            (0, 0),
            "{row}"
        );
        assert_eq!(
            executor
                .environments()
                .providers()
                .prewarm_default_environment_capacity(2)
                .await
                .unwrap(),
            0,
            "{row}"
        );
        assert_eq!(
            executor
                .environments()
                .providers()
                .prewarm_environment_snapshot(&environment, 2)
                .await
                .unwrap(),
            0,
            "{row}"
        );
        assert!(recording.prewarmed.lock().unwrap().is_empty(), "{row}");
    }

    let (executor, recording) = executor(true, on, false);
    assert_eq!(
        executor
            .environments()
            .providers()
            .environment_warmup_limits(),
        (2, 4),
        "W3"
    );
    assert_eq!(
        executor
            .environments()
            .providers()
            .prewarm_default_environment_capacity(0)
            .await
            .unwrap(),
        0,
        "W3"
    );
    assert_eq!(
        executor
            .environments()
            .providers()
            .prewarm_environment_snapshot(&environment, 0)
            .await
            .unwrap(),
        0,
        "W3"
    );
    assert_eq!(
        executor
            .environments()
            .providers()
            .prewarm_environment_snapshot(&self_hosted, 2)
            .await
            .unwrap(),
        0,
        "W4"
    );
    assert!(recording.prewarmed.lock().unwrap().is_empty(), "W3/W4");

    assert_eq!(
        executor
            .environments()
            .providers()
            .prewarm_default_environment_capacity(2)
            .await
            .unwrap(),
        2,
        "W5"
    );
    assert_eq!(
        executor
            .environments()
            .providers()
            .prewarm_environment_snapshot(&environment, 3)
            .await
            .unwrap(),
        3,
        "W5"
    );
    assert_eq!(
        *recording.prewarmed.lock().unwrap(),
        vec![
            (
                Some(
                    executor
                        .environments()
                        .providers()
                        .default_environment_capacity_shape()
                ),
                2
            ),
            (
                Some(
                    executor
                        .environments()
                        .providers()
                        .environment_snapshot_capacity_shape(&environment)
                ),
                3
            ),
        ],
        "W5 exact published shapes"
    );
    assert_ne!(
        executor
            .environments()
            .providers()
            .default_environment_capacity_shape(),
        executor
            .environments()
            .providers()
            .environment_snapshot_capacity_shape(&environment),
        "W5 distinct shapes"
    );
    assert_eq!(
        executor
            .environments()
            .providers()
            .ready_environment_snapshot_capacity(&environment),
        7,
        "ready is observed"
    );

    let (failing, _) = executor_for_failure();
    assert!(
        failing
            .environments()
            .providers()
            .prewarm_default_environment_capacity(2)
            .await
            .is_err(),
        "W6"
    );
    assert!(
        failing
            .environments()
            .providers()
            .prewarm_environment_snapshot(&environment, 2)
            .await
            .is_err(),
        "W6"
    );
}

fn executor_for_failure() -> (
    crate::realization_executor::RealizationExecutor,
    Arc<RecordingCapacity>,
) {
    executor(true, crate::WarmPool::new(2, 4), true)
}

#[tokio::test]
async fn image_preparation_covers_the_default_shape_and_every_non_self_hosted_environment() {
    // I1 the default Session shape is always prepared; I2 a self-hosted
    // Environment is never pulled here; I3 every other Environment is.
    let (executor, recording) = executor(false, crate::WarmPool::Disabled, false);
    let environment = crate::host::eager_environment();
    let mut self_hosted = environment.clone();
    self_hosted.self_hosted = true;
    executor
        .environments()
        .providers()
        .prewarm_environment_images(&[environment, self_hosted])
        .await
        .unwrap();
    assert_eq!(*recording.images.lock().unwrap(), vec![2], "I1+I3, not I2");
}
