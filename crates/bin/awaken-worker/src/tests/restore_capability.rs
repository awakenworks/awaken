//! T27-C01: checkpoint/restore enabled with a provider that owns no restore rung.
//!
//! Cause/effect table (production entrypoint `WorkerNodeBuilder::build`):
//!
//! | Rule | provider formats | checkpoint store | restoration() | Effect |
//! |---|---|---|---|---|
//! | K1 | none | absent | Unsupported | ordinary mode builds; no provider format advertised |
//! | K2 | hosted | absent | Unsupported | refused (format/store pairing) |
//! | K3 | none | present | Unsupported | refused (store without format) |
//! | K4 | hosted | present | Unsupported | refused at construction, zero creates |
//!
//! K4 is the restore-capability rung: a manifest that advertises checkpoint
//! formats promises restore, so the restore rung must be checked when the
//! Worker is constructed, before any external Environment is created or a
//! lifecycle that cannot be restored is committed.
//!
//! Negative control: a validator that admits construction and only learns of
//! the missing restore capability mid-run (through an `Option`) is executed
//! through the same oracle check and must be detected (it created an
//! Environment first).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_provisioning_contract as pc;

use super::{UnusedCheckpointStore, WorkerNodeBuilder};

/// A provider whose physical create path is observable. Its restore rung is
/// `Unsupported` in every rule; only the advertised formats vary.
struct CountingRestoreProvider {
    formats: Vec<String>,
    creates: Arc<AtomicUsize>,
}

impl CountingRestoreProvider {
    fn new(formats: &[&str]) -> Self {
        Self {
            formats: formats.iter().map(|format| (*format).to_string()).collect(),
            creates: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn creates(&self) -> usize {
        self.creates.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl pc::SandboxReadiness for CountingRestoreProvider {
    fn capabilities(&self) -> pc::SandboxCapabilities {
        Default::default()
    }

    fn checkpoint_formats(&self) -> Vec<String> {
        self.formats.clone()
    }

    async fn probe_ready(&self) -> Result<(), pc::SandboxError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl pc::SandboxProvider for CountingRestoreProvider {
    type Environment = dyn pc::SessionSandbox;

    fn install_memory_mounter(&self, _mounter: Arc<dyn pc::MemoryMounter>) {}

    fn install_secret_broker(&self, _broker: Arc<dyn pc::SecretBroker>) {}

    async fn prewarm_images(&self, _specs: &[pc::SandboxSpec]) -> Result<(), pc::SandboxError> {
        Ok(())
    }

    fn adoption_layout(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<pc::SandboxAdoptionLayout, pc::SandboxError> {
        awaken_sandbox_container::container_adoption_layout(handle)
    }

    fn validate_closed_observation(
        &self,
        handle: &pc::SandboxHandle,
        observation: &pc::SandboxObservation,
    ) -> Result<(), pc::SandboxError> {
        awaken_sandbox_container::validate_container_closed_observation(handle, observation)
    }

    fn claim_for_effect<'a>(
        &'a self,
        spec: &'a pc::SandboxSpec,
        effect: pc::SandboxEffect<'a>,
        intent: pc::SandboxRealizationIntent,
    ) -> pc::ClaimedEffect<'a, Arc<dyn pc::SessionSandbox>> {
        let claim = pc::EffectClaim::in_process(pc::test_support::retain_abandoned_effect(
            spec.scope.clone(),
        ));
        pc::ClaimedEffect::new(claim, async move {
            intent.admit_under(effect)?;
            // Every reach of the physical create edge is counted, whatever
            // the effect; the fixture realizes no live container.
            self.creates.fetch_add(1, Ordering::SeqCst);
            Err(pc::SandboxError::unsupported(
                "counting double records the create edge without realizing it",
            ))
        })
    }

    async fn observe(
        &self,
        _adoption: pc::SandboxAdoption<'_>,
        _basis: pc::SandboxObservationBasis<'_>,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "counting double does not observe Environments",
        ))
    }

    async fn adopt_for_effect(
        &self,
        _adoption: pc::SandboxAdoption<'_>,
        _effect: pc::SandboxEffect<'_>,
    ) -> Result<Arc<dyn pc::SessionSandbox>, pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "counting double does not adopt Environments",
        ))
    }

    async fn prepare_terminal_for_effect(
        &self,
        _spec: &pc::SandboxSpec,
        _handle: Option<&pc::SandboxHandle>,
        _expected_effect_fence: Option<&pc::SandboxEffectFence>,
        _terminal_effect_fence: &pc::SandboxEffectFence,
        _authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<Option<Arc<dyn pc::SessionSandbox>>, pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "counting double does not prepare terminal Environments",
        ))
    }

    fn restoration(&self) -> pc::SandboxRestoration<'_, Arc<dyn pc::SessionSandbox>> {
        pc::SandboxRestoration::Unsupported
    }
}

fn builder(provider: Arc<CountingRestoreProvider>, with_store: bool) -> WorkerNodeBuilder {
    let builder = WorkerNodeBuilder::new(
        awaken_worker_transport_security::WorkerUpstream::new("https://control.test"),
        awaken_time::system_clock(),
    )
    .with_session_container_provider("hosted", provider)
    .with_hand_executor_factory(crate::relay_hand_executor_factory())
    .with_standard_manifest(Default::default());
    if with_store {
        builder.with_environment_checkpoint_store(Arc::new(UnusedCheckpointStore))
    } else {
        builder
    }
}

fn run_spec() -> pc::SandboxSpec {
    pc::SandboxSpec {
        process_tree_supervision: false,
        scope: "t27-c01-session".into(),
        isolation: pc::IsolationClass::Container,
        environment: None,
        command: vec!["unused".into()],
        deny_tool_egress: false,
        mounts: Vec::new(),
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::None,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: Default::default(),
        filesystem_continuity: pc::FilesystemContinuity::Ephemeral,
        control_services: Default::default(),
        lease_ttl_secs: None,
    }
}

/// What one construction attempt did, as observed by the oracle.
#[derive(Debug, PartialEq, Eq)]
enum Construction {
    /// Construction refused; `creates` physical creates had happened by then.
    Refused { creates: usize },
    /// Construction completed (a Worker/run owner exists).
    Completed { creates: usize },
}

/// The T27-C01 oracle: restore-enabled composition over a provider without a
/// restore rung must be refused at construction with zero external creates.
fn fail_closed_violation(outcome: &Construction) -> Option<String> {
    match outcome {
        Construction::Refused { creates: 0 } => None,
        Construction::Refused { creates } => Some(format!(
            "refused only after {creates} external Environment create(s)"
        )),
        Construction::Completed { creates } => Some(format!(
            "construction completed without a restore rung (creates={creates})"
        )),
    }
}

/// Correct in-test reference: consult the restore rung before construction.
fn reference_validator(provider: &CountingRestoreProvider, store: bool) -> Construction {
    let restore_enabled = !pc::SandboxReadiness::checkpoint_formats(provider).is_empty() && store;
    if restore_enabled
        && matches!(
            pc::SandboxProvider::restoration(provider),
            pc::SandboxRestoration::Unsupported
        )
    {
        return Construction::Refused {
            creates: provider.creates(),
        };
    }
    Construction::Completed {
        creates: provider.creates(),
    }
}

/// Mutant: admits construction, then on the first run creates an external
/// Environment and only afterwards learns from an `Option` that restore is
/// missing.
async fn late_option_validator(provider: &CountingRestoreProvider) -> Construction {
    // "Construction" never looks at restoration().
    let spec = run_spec();
    let _create = pc::SandboxProvider::claim_for_effect(
        provider,
        &spec,
        pc::SandboxEffect::Unattached,
        pc::SandboxRealizationIntent::Create,
    )
    .await;
    let restore: Option<()> = match pc::SandboxProvider::restoration(provider) {
        pc::SandboxRestoration::Unsupported => None,
        pc::SandboxRestoration::Targets(_) | pc::SandboxRestoration::Restorable(_) => Some(()),
    };
    // Discovered mid-run: the run refuses, but construction already completed.
    assert!(restore.is_none(), "mutant fixture has no restore rung");
    Construction::Completed {
        creates: provider.creates(),
    }
}

#[tokio::test]
async fn restore_capability_pairing_and_ordinary_mode_hold_and_late_option_discovery_is_detected() {
    // K1 ordinary mode: same Unsupported-restore provider, no formats, no store.
    let ordinary = Arc::new(CountingRestoreProvider::new(&[]));
    let worker = builder(ordinary.clone(), false)
        .build()
        .expect("K1 ordinary execution mode must still build");
    assert!(
        !worker
            .manifest()
            .checkpoint_formats
            .contains("hosted-csi-v1"),
        "K1 ordinary mode advertises no provider filesystem checkpoint format"
    );
    assert_eq!(ordinary.creates(), 0, "K1 construction creates nothing");

    // K2 formats without a store.
    let formats_only = Arc::new(CountingRestoreProvider::new(&["hosted-csi-v1"]));
    assert_eq!(
        builder(formats_only.clone(), false)
            .build()
            .err()
            .expect("K2 must fail closed")
            .to_string(),
        "checkpoint-capable Session provider requires a checkpoint store",
        "K2"
    );
    assert_eq!(formats_only.creates(), 0, "K2 zero creates");

    // K3 store without formats.
    let store_only = Arc::new(CountingRestoreProvider::new(&[]));
    assert_eq!(
        builder(store_only.clone(), true)
            .build()
            .err()
            .expect("K3 must fail closed")
            .to_string(),
        "checkpoint store requires a checkpoint-capable Session provider",
        "K3"
    );
    assert_eq!(store_only.creates(), 0, "K3 zero creates");

    // Positive control for the oracle: the reference validator refuses K4
    // before any create and the oracle accepts it.
    let reference = CountingRestoreProvider::new(&["hosted-csi-v1"]);
    let outcome = reference_validator(&reference, true);
    assert_eq!(outcome, Construction::Refused { creates: 0 });
    assert_eq!(fail_closed_violation(&outcome), None, "reference passes");

    // Negative control: the late-Option mutant runs through the same oracle.
    let mutant = CountingRestoreProvider::new(&["hosted-csi-v1"]);
    let outcome = late_option_validator(&mutant).await;
    assert_eq!(mutant.creates(), 1, "NC mutant reached the create edge");
    assert_eq!(outcome, Construction::Completed { creates: 1 });
    assert!(
        fail_closed_violation(&outcome).is_some(),
        "NC: mid-run Option discovery must be detected as a violation"
    );
}

#[tokio::test]
#[ignore = "product gap: WorkerNodeBuilder::build never consults SandboxProvider::restoration() for checkpoint formats+store"]
async fn restore_enabled_worker_with_unsupported_restoration_is_refused_at_construction() {
    // K4: formats + store, restoration() == Unsupported.
    let provider = Arc::new(CountingRestoreProvider::new(&["hosted-csi-v1"]));
    let outcome = match builder(provider.clone(), true).build() {
        Ok(worker) => {
            let _ = worker.manifest();
            Construction::Completed {
                creates: provider.creates(),
            }
        }
        Err(_) => Construction::Refused {
            creates: provider.creates(),
        },
    };
    assert_eq!(provider.creates(), 0, "K4 zero external creates");
    assert_eq!(
        fail_closed_violation(&outcome),
        None,
        "K4 restore-enabled composition without a restore rung must be refused at construction"
    );
}
