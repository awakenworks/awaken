//! Installation order of the shared Session environment services: whichever
//! order the composition selects the secret broker, the Memory mounter and
//! the Session provider(s), the provider that finally serves Sessions carries
//! the same broker, mounter and clock, and a replaced provider gets no new
//! execution right.

use awaken_session_realization::peers::EnvironmentPeer as _;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::SandboxEffect;
use awaken_session_environment_contract::{
    EnvironmentAdoption, EnvironmentAdoptionLayout, EnvironmentRealizationKind,
    EnvironmentRealizer, EnvironmentTerminalRealization, SandboxProviderRoles, SessionEnvironment,
};

use super::OkModel;

/// Delegates every environment role to a real Workdir provider and records
/// each installed shared service and each capacity request that reaches it.
struct RecordingInstallation {
    name: &'static str,
    inner: SandboxProviderRoles,
    clock: Arc<dyn awaken_time::WallClock>,
    mounters: Mutex<Vec<Arc<dyn pc::MemoryMounter>>>,
    brokers: Mutex<Vec<Arc<dyn pc::SecretBroker>>>,
    image_prewarms: Mutex<usize>,
}

impl RecordingInstallation {
    fn new(name: &'static str, clock: Arc<dyn awaken_time::WallClock>) -> Arc<Self> {
        let root = std::env::temp_dir().join(format!(
            "install-order-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        Arc::new(Self {
            name,
            inner: crate::uninstalled_environment::default_session_provider(root).0,
            clock,
            mounters: Mutex::default(),
            brokers: Mutex::default(),
            image_prewarms: Mutex::default(),
        })
    }

    fn provider(self: &Arc<Self>) -> SandboxProviderRoles {
        SandboxProviderRoles::new(self.clone())
    }

    fn installs(&self) -> (usize, usize) {
        (
            self.mounters.lock().unwrap().len(),
            self.brokers.lock().unwrap().len(),
        )
    }
}

#[async_trait]
impl pc::SandboxReadiness for RecordingInstallation {
    fn capabilities(&self) -> pc::SandboxCapabilities {
        self.inner.readiness().capabilities()
    }

    fn checkpoint_formats(&self) -> Vec<String> {
        self.inner.readiness().checkpoint_formats()
    }

    async fn probe_ready(&self) -> Result<(), pc::SandboxError> {
        self.inner.readiness().probe_ready().await
    }
}

#[async_trait]
impl EnvironmentRealizer for RecordingInstallation {
    fn supports_host_identity(&self) -> bool {
        self.inner.realizer().supports_host_identity()
    }

    fn effective_spec(&self, spec: &pc::SandboxSpec) -> Result<pc::SandboxSpec, pc::SandboxError> {
        self.inner.realizer().effective_spec(spec)
    }

    async fn prewarm_images(&self, _specs: &[pc::SandboxSpec]) -> Result<(), pc::SandboxError> {
        *self.image_prewarms.lock().unwrap() += 1;
        Ok(())
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
impl EnvironmentAdoption for RecordingInstallation {
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
impl EnvironmentTerminalRealization for RecordingInstallation {
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
        effect: pc::SandboxEffect<'_>,
    ) -> Result<pc::SandboxHandle, pc::SandboxError> {
        self.inner
            .terminal()
            .restore(spec, request, store, effect)
            .await
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
impl pc::SandboxCapacity for RecordingInstallation {
    async fn prewarm_to(
        &self,
        _spec: &pc::SandboxSpec,
        _target: usize,
    ) -> Result<usize, pc::SandboxError> {
        Ok(0)
    }

    async fn discard_shape(&self, _spec: &pc::SandboxSpec) {}

    fn ready_capacity(&self, _spec: &pc::SandboxSpec) -> usize {
        0
    }

    async fn shutdown_capacity(&self) {}
}

impl pc::SandboxInstallation for RecordingInstallation {
    fn install_memory_mounter(&self, mounter: Arc<dyn pc::MemoryMounter>) {
        self.mounters.lock().unwrap().push(mounter);
    }

    fn install_secret_broker(&self, broker: Arc<dyn pc::SecretBroker>) {
        self.brokers.lock().unwrap().push(broker);
    }
}

/// The one controlled broker instance each row installs.
struct ControlledBroker;

#[async_trait]
impl pc::SecretBroker for ControlledBroker {
    async fn materialize(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "controlled broker never materializes",
        ))
    }

    async fn materialize_process(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "controlled broker never materializes",
        ))
    }

    async fn write_back(
        &self,
        _target: pc::SecretWritebackTarget<'_>,
        _bytes: Vec<u8>,
    ) -> Result<(), pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "controlled broker never writes back",
        ))
    }
}

/// The controlled shared services of one row.
struct Controlled {
    broker: Arc<dyn pc::SecretBroker>,
    mounter: Arc<dyn pc::MemoryMounter>,
    clock: Arc<dyn awaken_time::WallClock>,
}

impl Controlled {
    async fn new() -> (crate::SessionExecutorBuilder, Self) {
        let builder = crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await;
        let builder = crate::host::with_test_memory_mounter(builder);
        let mounter = builder
            .env
            .memory_mounter
            .clone()
            .expect("the fixture selects a Memory mounter");
        let clock = builder.clock();
        (
            builder,
            Self {
                broker: Arc::new(ControlledBroker),
                mounter,
                clock,
            },
        )
    }
}

fn last_is<T: ?Sized>(list: &Mutex<Vec<Arc<T>>>, expected: &Arc<T>) -> bool {
    list.lock()
        .unwrap()
        .last()
        .is_some_and(|installed| Arc::ptr_eq(installed, expected))
}

/// Oracle 0 and 1 over one selected provider: it is the expected adapter, and
/// it carries the controlled broker, mounter and clock.
fn installation_oracle(
    selected: &SandboxProviderRoles,
    expected: &Arc<RecordingInstallation>,
    controlled: &Controlled,
) -> Result<(), String> {
    let origin = selected
        .origin()
        .downcast_ref::<RecordingInstallation>()
        .ok_or("selected provider is not a recording adapter")?;
    if !std::ptr::eq(origin, expected.as_ref()) {
        return Err(format!(
            "selected provider is {}, expected {}",
            origin.name, expected.name
        ));
    }
    let installation: Arc<dyn pc::SandboxInstallation> = expected.clone();
    if !Arc::ptr_eq(selected.installation(), &installation) {
        return Err(format!(
            "{} has a foreign installation owner",
            expected.name
        ));
    }
    if !last_is(&expected.brokers, &controlled.broker) {
        return Err(format!("{} lost the secret broker", expected.name));
    }
    if !last_is(&expected.mounters, &controlled.mounter) {
        return Err(format!("{} lost the Memory mounter", expected.name));
    }
    if !Arc::ptr_eq(&expected.clock, &controlled.clock) {
        return Err(format!("{} runs on a foreign clock", expected.name));
    }
    Ok(())
}

/// Build the Host and prove the selected provider is the only one that
/// receives execution, on the Host's own clock.
async fn assert_built_host_serves(
    builder: crate::SessionExecutorBuilder,
    selected: &Arc<RecordingInstallation>,
    capacity_owner: &Arc<RecordingInstallation>,
    replaced: Option<&Arc<RecordingInstallation>>,
    controlled: &Controlled,
    row: &str,
) {
    let host = builder.build().expect("validated Host dependencies");
    assert!(
        Arc::ptr_eq(&host.clock, &controlled.clock),
        "{row}: Host clock"
    );
    let providers = host.realization_executor.environments().providers();
    installation_oracle(&providers.session_provider, selected, controlled)
        .unwrap_or_else(|error| panic!("{row}: built Host: {error}"));
    let expected_capacity: Arc<dyn pc::SandboxCapacity> = capacity_owner.clone();
    assert!(
        providers
            .session_capacity
            .as_ref()
            .is_some_and(|capacity| Arc::ptr_eq(capacity, &expected_capacity)),
        "{row}: capacity belongs to the same selected construction"
    );
    providers
        .prewarm_environment_images(&[])
        .await
        .expect("image preparation reaches the selected provider");
    assert_eq!(
        *selected.image_prewarms.lock().unwrap(),
        1,
        "{row}: selected executes"
    );
    if let Some(replaced) = replaced {
        assert_eq!(
            *replaced.image_prewarms.lock().unwrap(),
            0,
            "{row}: the replaced provider gets no new execution"
        );
    }
}

#[tokio::test]
async fn every_installation_order_lands_the_same_broker_mounter_and_clock_on_the_final_provider() {
    // SI3 cause/effect: R1-R6 select services before/after the provider, replace
    // its selection or relocate its adapter. Every row consumes the canonical
    // SandboxInstallation role of the SAME selected Arc; an installation-only
    // object neither requires an associated Environment nor grants execution.
    // The original broker/mounter/clock and capacity assertions remain below.
    // Decision table (T01-C01). Every row first selects the controlled Memory
    // mounter through the fixture, except where the row orders it explicitly.
    // C2 provider replacement selects its SandboxCapacity in the same call;
    // E2 the built Host keeps that exact pool, never the replaced provider's.
    // | Row | call order                                  | selected | expected                         |
    // | R1  | Broker -> Provider A                        | A        | A uses the original Broker       |
    // | R2  | Provider A -> Broker                        | A        | same as R1                       |
    // | R3  | fallback A -> Broker -> Provider B          | B        | B uses the Broker, A no new run  |
    // | R4  | Provider A -> mounter -> Provider B -> Broker | B      | B's Broker and mounter original  |
    // | R5  | Broker -> Provider A -> durable relocation  | A'       | relocated A' keeps both          |

    // R1
    let (builder, controlled) = Controlled::new().await;
    let a = RecordingInstallation::new("A", builder.clock());
    let builder = builder
        .with_session_secret_broker(controlled.broker.clone())
        .with_session_environment_provider(a.provider(), Some(a.clone()));
    installation_oracle(
        builder.installed_session_environment_provider(),
        &a,
        &controlled,
    )
    .unwrap_or_else(|error| panic!("R1: {error}"));
    assert_eq!(a.installs(), (1, 1), "R1: one install of each service");
    assert_built_host_serves(builder, &a, &a, None, &controlled, "R1").await;

    // R2
    let (builder, controlled) = Controlled::new().await;
    let a = RecordingInstallation::new("A", builder.clock());
    let builder = builder
        .with_session_environment_provider(a.provider(), Some(a.clone()))
        .with_session_secret_broker(controlled.broker.clone());
    installation_oracle(
        builder.installed_session_environment_provider(),
        &a,
        &controlled,
    )
    .unwrap_or_else(|error| panic!("R2: {error}"));
    assert_eq!(a.installs(), (1, 1), "R2: same as R1");
    assert_built_host_serves(builder, &a, &a, None, &controlled, "R2").await;

    // R3
    let (builder, controlled) = Controlled::new().await;
    let a = RecordingInstallation::new("A", builder.clock());
    let b = RecordingInstallation::new("B", builder.clock());
    let builder = builder
        .with_fallback_session_environment_provider(a.provider())
        .with_session_secret_broker(controlled.broker.clone());
    let a_before_b = a.installs();
    let builder = builder.with_session_environment_provider(b.provider(), Some(b.clone()));
    installation_oracle(
        builder.installed_session_environment_provider(),
        &b,
        &controlled,
    )
    .unwrap_or_else(|error| panic!("R3: {error}"));
    assert_eq!(a.installs(), a_before_b, "R3: A receives nothing after B");
    assert_eq!(b.installs(), (1, 1), "R3: B receives each service once");
    assert_built_host_serves(builder, &b, &b, Some(&a), &controlled, "R3").await;

    // R4: the mounter is ordered explicitly here, after A.
    let builder = crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub").await;
    let a = RecordingInstallation::new("A", builder.clock());
    let b = RecordingInstallation::new("B", builder.clock());
    let builder = crate::host::with_test_memory_mounter(
        builder.with_session_environment_provider(a.provider(), Some(a.clone())),
    );
    let controlled = Controlled {
        broker: Arc::new(ControlledBroker),
        mounter: builder
            .env
            .memory_mounter
            .clone()
            .expect("mounter selected"),
        clock: builder.clock(),
    };
    let builder = builder
        .with_session_environment_provider(b.provider(), Some(b.clone()))
        .with_session_secret_broker(controlled.broker.clone());
    installation_oracle(
        builder.installed_session_environment_provider(),
        &b,
        &controlled,
    )
    .unwrap_or_else(|error| panic!("R4: {error}"));
    assert_eq!(a.installs(), (1, 0), "R4: A never sees the later Broker");
    assert_eq!(b.installs(), (1, 1), "R4");
    assert_built_host_serves(builder, &b, &b, Some(&a), &controlled, "R4").await;

    // R5: test-support durable relocation reconfigures the provider adapter,
    // not its physical pool. The original selected capacity remains paired
    // with that same backend; container at_root preserves its provider Arc.
    let (builder, controlled) = Controlled::new().await;
    let a = RecordingInstallation::new("A", builder.clock());
    let relocated = RecordingInstallation::new("A-relocated", builder.clock());
    let dir = tempfile::tempdir().expect("durable store dir");
    let repository =
        awaken_session_executor_testkit::memory_extraction_repository(Some(dir.path())).await;
    let replacement = relocated.provider();
    let builder = builder
        .with_session_secret_broker(controlled.broker.clone())
        .with_session_environment_provider(a.provider(), Some(a.clone()))
        .with_durable_store_dir(dir.path(), repository, move |_, _| replacement.clone());
    installation_oracle(
        builder.installed_session_environment_provider(),
        &relocated,
        &controlled,
    )
    .unwrap_or_else(|error| panic!("R5: {error}"));
    assert_eq!(relocated.installs(), (1, 1), "R5");
    assert_built_host_serves(builder, &relocated, &a, Some(&a), &controlled, "R5").await;

    // R6: an explicit fallback selection has no capacity owner. Replacing a
    // previously selected pair must clear its pool while keeping the same
    // broker/mounter installation rules; no stale positive capacity survives.
    let (builder, controlled) = Controlled::new().await;
    let a = RecordingInstallation::new("A", builder.clock());
    let b = RecordingInstallation::new("fallback-B", builder.clock());
    let builder = builder
        .with_session_secret_broker(controlled.broker.clone())
        .with_session_environment_provider(a.provider(), Some(a.clone()))
        .with_fallback_session_environment_provider(b.provider());
    installation_oracle(
        builder.installed_session_environment_provider(),
        &b,
        &controlled,
    )
    .unwrap_or_else(|error| panic!("R6: {error}"));
    assert!(builder.env.session_capacity.is_none(), "R6 no former pool");
}

#[tokio::test]
async fn installation_oracle_detects_a_removed_final_binding_or_a_kept_old_provider() {
    // Negative controls: the same oracle check must reject each mutant replay.

    // N1 final binding step removed: R3 without selecting B.
    let (builder, controlled) = Controlled::new().await;
    let a = RecordingInstallation::new("A", builder.clock());
    let b = RecordingInstallation::new("B", builder.clock());
    let builder = builder
        .with_fallback_session_environment_provider(a.provider())
        .with_session_secret_broker(controlled.broker.clone());
    let error = installation_oracle(
        builder.installed_session_environment_provider(),
        &b,
        &controlled,
    )
    .expect_err("N1: removing the final binding must fail the oracle");
    assert_eq!(error, "selected provider is A, expected B", "N1");

    // N2 old provider kept: B only lands on the TrustedHost slot, A still serves.
    let (builder, controlled) = Controlled::new().await;
    let a = RecordingInstallation::new("A", builder.clock());
    let b = RecordingInstallation::new("B", builder.clock());
    let builder = builder
        .with_fallback_session_environment_provider(a.provider())
        .with_session_secret_broker(controlled.broker.clone())
        .with_backend_owned_environment_provider(b.provider());
    let error = installation_oracle(
        builder.installed_session_environment_provider(),
        &b,
        &controlled,
    )
    .expect_err("N2: keeping the old provider must fail the oracle");
    assert_eq!(error, "selected provider is A, expected B", "N2");

    // N3 the pre-fix behaviour: the Broker installed only on the provider
    // current at that moment, never stored for a later selection.
    let (builder, controlled) = Controlled::new().await;
    let a = RecordingInstallation::new("A", builder.clock());
    let b = RecordingInstallation::new("B", builder.clock());
    let builder = builder.with_fallback_session_environment_provider(a.provider());
    builder
        .installed_session_environment_provider()
        .installation()
        .install_secret_broker(controlled.broker.clone());
    let builder = builder.with_session_environment_provider(b.provider(), Some(b.clone()));
    let error = installation_oracle(
        builder.installed_session_environment_provider(),
        &b,
        &controlled,
    )
    .expect_err("N3: a broker that is not re-installed must fail the oracle");
    assert_eq!(error, "B lost the secret broker", "N3");

    // N4 a provider constructed on a foreign clock.
    let (builder, controlled) = Controlled::new().await;
    let foreign = RecordingInstallation::new("F", awaken_time::system_clock());
    let builder = builder
        .with_session_secret_broker(controlled.broker.clone())
        .with_session_environment_provider(foreign.provider(), Some(foreign.clone()));
    let error = installation_oracle(
        builder.installed_session_environment_provider(),
        &foreign,
        &controlled,
    )
    .expect_err("N4: a foreign clock must fail the oracle");
    assert_eq!(error, "F runs on a foreign clock", "N4");
}
