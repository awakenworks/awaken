//! Decision table for how the Session reaches the one selected Sandbox
//! provider through its role handles.
//!
//! | Row | Cause                                        | Effect                                                                 |
//! |-----|----------------------------------------------|------------------------------------------------------------------------|
//! | U1  | unattached create of spec `S`                | the adapter projects `S` once, then creates the projection Unattached as a Create with no source |
//! | U2  | the projection of `S` is refused             | the refusal is returned; no create is attempted (fail closed)          |
//! | U3  | unattached adopt of handle `H` under `S`     | the adapter's own layout for `(S, H)` is computed once, then `H` is adopted Unattached under that layout's spec |
//! | U4  | the adoption layout is refused               | the refusal is returned; no adoption is attempted (fail closed)        |
//! | U5  | readiness is read through the role handles   | the same adapter instance answers capabilities, formats and its probe  |
//!
//! Every row's effect is the adapter's own typed error passed through
//! unchanged: no row may turn a refusal into success or a different class.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::SandboxEffect;

use super::*;

/// Records every provider call it receives and refuses the effect itself, so
/// a row observes exactly which calls the role handles composed.
#[derive(Default)]
struct RecordingAdapter {
    calls: Mutex<Vec<String>>,
    refuse_projection: bool,
}

impl RecordingAdapter {
    fn refusing_projection() -> Self {
        Self {
            refuse_projection: true,
            ..Self::default()
        }
    }

    fn record(&self, call: String) {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(call);
    }

    fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

fn spec(scope: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        process_tree_supervision: false,
        scope: scope.into(),
        isolation: pc::IsolationClass::Container,
        mounts: Vec::new(),
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: Default::default(),
        filesystem_continuity: pc::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        control_services: Default::default(),
        environment: None,
        command: Vec::new(),
        deny_tool_egress: false,
    }
}

fn effect_name(effect: &SandboxEffect<'_>) -> &'static str {
    match effect {
        SandboxEffect::Unattached => "unattached",
        SandboxEffect::Fenced { .. } => "fenced",
    }
}

#[async_trait]
impl pc::SandboxReadiness for RecordingAdapter {
    fn capabilities(&self) -> pc::SandboxCapabilities {
        self.record("capabilities".into());
        pc::SandboxCapabilities::default()
    }

    fn checkpoint_formats(&self) -> Vec<String> {
        self.record("checkpoint_formats".into());
        vec!["recorded-format".into()]
    }

    async fn probe_ready(&self) -> Result<(), pc::SandboxError> {
        self.record("probe_ready".into());
        Err(pc::SandboxError::not_ready("recorded probe"))
    }
}

#[async_trait]
impl EnvironmentRealizer for RecordingAdapter {
    fn supports_host_identity(&self) -> bool {
        false
    }

    fn effective_spec(&self, spec: &pc::SandboxSpec) -> Result<pc::SandboxSpec, pc::SandboxError> {
        self.record(format!("effective_spec {}", spec.scope));
        if self.refuse_projection {
            return Err(pc::SandboxError::invalid("recorded projection refusal"));
        }
        let mut effective = spec.clone();
        effective.scope = format!("{}-effective", spec.scope);
        Ok(effective)
    }

    async fn prewarm_images(&self, _specs: &[pc::SandboxSpec]) -> Result<(), pc::SandboxError> {
        self.record("prewarm_images".into());
        Ok(())
    }

    async fn create_effective_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        effect: SandboxEffect<'_>,
        source_handle: Option<&pc::SandboxHandle>,
        kind: EnvironmentRealizationKind,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        self.record(format!(
            "create {} {} source={} {kind:?}",
            spec.scope,
            effect_name(&effect),
            source_handle.is_some(),
        ));
        Err(pc::SandboxError::not_ready("recorded create"))
    }
}

#[async_trait]
impl EnvironmentAdoption for RecordingAdapter {
    fn effective_adoption_layout(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
    ) -> Result<EnvironmentAdoptionLayout, pc::SandboxError> {
        self.record(format!("layout {} {}", spec.scope, handle.sandbox_id));
        if self.refuse_projection {
            return Err(pc::SandboxError::conflict("recorded layout refusal"));
        }
        let mut layout_spec = spec.clone();
        layout_spec.scope = format!("{}-layout", spec.scope);
        Ok(EnvironmentAdoptionLayout {
            spec: layout_spec,
            historical_owned_paths: None,
        })
    }

    async fn observe_effective(
        &self,
        _spec: &pc::SandboxSpec,
        _handle: &pc::SandboxHandle,
        _retained: Option<&SessionEnvironment>,
        _basis: pc::SandboxObservationBasis<'_>,
        _purpose: pc::SandboxObservationPurpose,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        self.record("observe".into());
        Err(pc::SandboxError::not_ready("recorded observe"))
    }

    fn validate_closed_observation(
        &self,
        _handle: &pc::SandboxHandle,
        _observation: &pc::SandboxObservation,
    ) -> Result<(), pc::SandboxError> {
        self.record("validate_closed_observation".into());
        Err(pc::SandboxError::not_ready("recorded validation"))
    }

    async fn adopt_effective_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        effect: SandboxEffect<'_>,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        self.record(format!(
            "adopt {} {} {}",
            spec.scope,
            handle.sandbox_id,
            effect_name(&effect),
        ));
        Err(pc::SandboxError::not_ready("recorded adopt"))
    }

    async fn rebind_retained(
        &self,
        _spec: &pc::SandboxSpec,
        _handle: &pc::SandboxHandle,
        _retained: &SessionEnvironment,
        _effect: SandboxEffect<'_>,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        self.record("rebind".into());
        Err(pc::SandboxError::not_ready("recorded rebind"))
    }
}

#[async_trait]
impl EnvironmentTerminalRealization for RecordingAdapter {
    fn terminal_data_is_readable(&self) -> bool {
        false
    }

    async fn prepare_terminal_effective_for_effect(
        &self,
        _spec: &pc::SandboxSpec,
        _handle: Option<&pc::SandboxHandle>,
        _expected_effect_fence: Option<&pc::SandboxEffectFence>,
        _terminal_effect_fence: &pc::SandboxEffectFence,
        _authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<Option<SessionEnvironment>, pc::SandboxError> {
        self.record("prepare_terminal".into());
        Err(pc::SandboxError::not_ready("recorded terminal"))
    }

    async fn restore(
        &self,
        _spec: &pc::SandboxSpec,
        _request: &pc::SandboxRestoreRequest,
        _store: &dyn pc::SandboxCheckpointStore,
        _effect: pc::SandboxEffect<'_>,
    ) -> Result<pc::SandboxHandle, pc::SandboxError> {
        self.record("restore".into());
        Err(pc::SandboxError::not_ready("recorded restore"))
    }

    async fn dispose_restored(
        &self,
        _spec: &pc::SandboxSpec,
        _request: &pc::SandboxRestoreRequest,
        _authorization: &pc::SandboxDisposalAuthorization,
        _authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError> {
        self.record("dispose_restored".into());
        Err(pc::SandboxError::not_ready("recorded disposal"))
    }
}

impl pc::SandboxInstallation for RecordingAdapter {
    fn install_memory_mounter(&self, _mounter: Arc<dyn pc::MemoryMounter>) {
        self.record("install_memory_mounter".into());
    }

    fn install_secret_broker(&self, _broker: Arc<dyn pc::SecretBroker>) {
        self.record("install_secret_broker".into());
    }
}

#[tokio::test]
async fn unattached_creation_projects_the_spec_once_then_creates_the_projection() {
    let adapter = Arc::new(RecordingAdapter::default());
    let provider = SandboxProviderRoles::new(adapter.clone());
    let refused = provider
        .realizer()
        .create_unattached(&spec("s"))
        .await
        .err()
        .expect("U1");
    assert_eq!(
        refused,
        pc::SandboxError::not_ready("recorded create"),
        "U1"
    );
    assert_eq!(
        adapter.calls(),
        vec![
            "effective_spec s".to_string(),
            "create s-effective unattached source=false Create".to_string(),
        ],
        "U1"
    );

    let adapter = Arc::new(RecordingAdapter::refusing_projection());
    let provider = SandboxProviderRoles::new(adapter.clone());
    let refused = provider
        .realizer()
        .create_unattached(&spec("s"))
        .await
        .err()
        .expect("U2");
    assert_eq!(
        refused,
        pc::SandboxError::invalid("recorded projection refusal"),
        "U2"
    );
    assert_eq!(adapter.calls(), vec!["effective_spec s".to_string()], "U2");
}

#[tokio::test]
async fn unattached_adoption_reopens_through_the_adapters_own_layout() {
    let handle = pc::SandboxHandle::new("recorded", "h");
    let adapter = Arc::new(RecordingAdapter::default());
    let provider = SandboxProviderRoles::new(adapter.clone());
    let refused = provider
        .adoption()
        .adopt_unattached(&spec("s"), &handle)
        .await
        .err()
        .expect("U3");
    assert_eq!(refused, pc::SandboxError::not_ready("recorded adopt"), "U3");
    assert_eq!(
        adapter.calls(),
        vec![
            "layout s h".to_string(),
            "adopt s-layout h unattached".to_string()
        ],
        "U3"
    );

    let adapter = Arc::new(RecordingAdapter::refusing_projection());
    let provider = SandboxProviderRoles::new(adapter.clone());
    let refused = provider
        .adoption()
        .adopt_unattached(&spec("s"), &handle)
        .await
        .err()
        .expect("U4");
    assert_eq!(
        refused,
        pc::SandboxError::conflict("recorded layout refusal"),
        "U4"
    );
    assert_eq!(adapter.calls(), vec!["layout s h".to_string()], "U4");
}

#[tokio::test]
async fn readiness_is_answered_by_the_one_selected_adapter() {
    let adapter = Arc::new(RecordingAdapter::default());
    let provider = SandboxProviderRoles::new(adapter.clone());
    let readiness: &dyn pc::SandboxReadiness = provider.readiness().as_ref();
    assert_eq!(
        readiness.capabilities(),
        pc::SandboxCapabilities::default(),
        "U5"
    );
    assert_eq!(
        readiness.checkpoint_formats(),
        vec!["recorded-format".to_string()],
        "U5"
    );
    assert_eq!(
        readiness.probe_ready().await,
        Err(pc::SandboxError::not_ready("recorded probe")),
        "U5"
    );
    assert_eq!(
        adapter.calls(),
        vec![
            "capabilities".to_string(),
            "checkpoint_formats".to_string(),
            "probe_ready".to_string(),
        ],
        "U5 same instance"
    );
}
