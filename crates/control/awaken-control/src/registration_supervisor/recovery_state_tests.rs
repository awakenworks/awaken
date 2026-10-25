//! T21-C03 / T41-C02: recovery-state transitions of the production
//! `StaticRegistrationSupervisor::start_with_config`, per domain, and the
//! correspondence between its health snapshot and real executable capability.
//!
//! Only the domain collaborators (Agent reconciler, Environment registrar) are
//! fakes; the supervisor loop, `EnvironmentApplication` and the in-memory
//! Environment authority are production code.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use awaken_environment_application::EnvironmentApplication;
use awaken_environment_contract::{
    EnvironmentDefinitionStore, EnvironmentRegistrationIntentFilter,
};
use awaken_executable_environment_contract::{
    ExecutableEnvironmentRegistrar, ExecutableEnvironmentRegistration,
    ExecutableEnvironmentRegistrationError, ExecutableEnvironmentRegistrationOutcome,
    ExecutableEnvironmentWithdrawal, ExecutableEnvironmentWithdrawalOutcome,
};

use super::*;

const QUIET: RegistrationSupervisorConfig = RegistrationSupervisorConfig {
    retry_min: Duration::from_secs(60),
    retry_max: Duration::from_secs(60),
    settled_interval: Duration::from_secs(60),
};

/// Bounded wait for a fact the supervisor publishes only through atomics.
async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    // awaken-allow(wait): the supervisor exposes pass results only through RegistrationHealth atomics and recorded calls; no committed-fact signal exists to await.
    let polled = tokio::time::timeout(Duration::from_secs(10), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(polled.is_ok(), "timed out waiting for {what}");
}

/// Agent domain collaborator: fails its first `failures` recovery calls and
/// counts which recovery shape each pass used.
struct RecordingAgents {
    failures: AtomicUsize,
    full_replays: AtomicUsize,
    pending_reconciles: AtomicUsize,
}

impl RecordingAgents {
    fn failing(failures: usize) -> Self {
        Self {
            failures: AtomicUsize::new(failures),
            full_replays: AtomicUsize::new(0),
            pending_reconciles: AtomicUsize::new(0),
        }
    }

    fn passes(&self) -> usize {
        self.full_replays.load(Ordering::SeqCst) + self.pending_reconciles.load(Ordering::SeqCst)
    }

    fn outcome(&self) -> Result<usize, String> {
        let remaining = self.failures.load(Ordering::SeqCst);
        if remaining > 0 {
            self.failures.store(remaining - 1, Ordering::SeqCst);
            Err("agent recovery unavailable".into())
        } else {
            Ok(0)
        }
    }
}

#[async_trait::async_trait]
impl PublicationBindingReconciler for RecordingAgents {
    async fn reconcile(&self) -> Result<usize, String> {
        self.pending_reconciles.fetch_add(1, Ordering::SeqCst);
        self.outcome()
    }

    async fn reconcile_all(&self) -> Result<usize, String> {
        self.pending_reconciles.fetch_add(1, Ordering::SeqCst);
        self.outcome()
    }

    async fn recover_registrations(&self) -> Result<usize, String> {
        self.full_replays.fetch_add(1, Ordering::SeqCst);
        self.outcome()
    }
}

/// Environment registrar collaborator: records every registered custom
/// Environment (the executable capability) and can fail the custom one once.
#[derive(Default)]
struct RecordingRegistrar {
    registered: Mutex<BTreeSet<String>>,
    fail_custom_once: AtomicBool,
}

impl RecordingRegistrar {
    fn serves_custom(&self) -> bool {
        self.registered
            .lock()
            .unwrap()
            .iter()
            .any(|id| id != awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID)
    }
}

#[async_trait::async_trait]
impl ExecutableEnvironmentRegistrar for RecordingRegistrar {
    async fn restore_historical_registration(
        &self,
        registration: ExecutableEnvironmentRegistration,
    ) -> Result<(), ExecutableEnvironmentRegistrationError> {
        registration.validate()
    }

    async fn register(
        &self,
        registration: ExecutableEnvironmentRegistration,
    ) -> Result<ExecutableEnvironmentRegistrationOutcome, ExecutableEnvironmentRegistrationError>
    {
        let custom =
            registration.definition.id != awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID;
        if custom && self.fail_custom_once.swap(false, Ordering::SeqCst) {
            return Err(ExecutableEnvironmentRegistrationError::Unavailable(
                "injected Environment recovery outage".into(),
            ));
        }
        self.registered
            .lock()
            .unwrap()
            .insert(registration.definition.id.clone());
        Ok(ExecutableEnvironmentRegistrationOutcome::RegisteredCurrent)
    }

    async fn withdraw(
        &self,
        _withdrawal: ExecutableEnvironmentWithdrawal,
    ) -> Result<ExecutableEnvironmentWithdrawalOutcome, ExecutableEnvironmentRegistrationError>
    {
        Ok(ExecutableEnvironmentWithdrawalOutcome::WithdrawnCurrent)
    }
}

struct Harness {
    registry: Arc<awaken_env_store::InMemoryEnvRegistry>,
    registrar: Arc<RecordingRegistrar>,
    agents: Arc<RecordingAgents>,
    supervisor: Arc<StaticRegistrationSupervisor>,
    lifecycle: awaken_service_lifecycle::ServiceLifecycle,
}

async fn harness(agent_failures: usize, fail_custom_once: bool) -> Harness {
    let registry = Arc::new(awaken_env_store::InMemoryEnvRegistry::new(
        awaken_time::system_clock(),
    ));
    registry
        .create_once(awaken_environment_contract::CreateEnvironmentCommand {
            command_id: "recovery-state".into(),
            name: "custom".into(),
            description: None,
            metadata: Default::default(),
            scope: None,
            config: awaken_environment_contract::EnvironmentConfig::SelfHosted,
            sandbox_policy: None,
        })
        .await
        .unwrap();
    let registrar = Arc::new(RecordingRegistrar {
        registered: Mutex::default(),
        fail_custom_once: AtomicBool::new(fail_custom_once),
    });
    let environments = Arc::new(EnvironmentApplication::new(
        registry.clone(),
        registrar.clone(),
        None,
    ));
    let agents = Arc::new(RecordingAgents::failing(agent_failures));
    let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
    let supervisor = StaticRegistrationSupervisor::start_with_config(
        agents.clone(),
        environments.registration_delivery(),
        QUIET,
        &lifecycle,
    );
    Harness {
        registry,
        registrar,
        agents,
        supervisor,
        lifecycle,
    }
}

impl Harness {
    /// Wait until `passes` passes have read both domains and the health record
    /// satisfies `expected` (the record is published after the joined pass).
    async fn settle(
        &self,
        passes: usize,
        what: &str,
        expected: impl Fn(RegistrationHealthSnapshot) -> bool,
    ) {
        let agents = self.agents.clone();
        let registry = self.registry.clone();
        let health = self.supervisor.health();
        eventually(what, move || {
            agents.passes() >= passes
                && registry.registration_intent_filters().len() >= passes
                && expected(health.snapshot())
        })
        .await;
    }

    async fn stop(self) {
        self.lifecycle
            .shutdown(Duration::from_secs(1))
            .await
            .expect("test supervisor stops cooperatively");
    }
}

/// Domain isolation table (T21-C03 oracles 0 and 1): R1 Agent fails, the
/// Environment replay completes => the Environment crosses to Pending on the
/// next pass even though the process-wide pass was only a partial success;
/// R2 Agent still failing => Environment stays Pending (no return to All);
/// R3 both recovered => zero pending domains.
#[tokio::test]
async fn an_agent_failure_does_not_hold_the_environment_domain_on_full_replay() {
    let harness = harness(2, false).await;
    harness
        .settle(1, "R1", |h| h.consecutive_failures == 1)
        .await;
    assert_eq!(
        harness.supervisor.health().snapshot().pending_domains,
        1,
        "R1"
    );
    harness.supervisor.wake();
    harness
        .settle(2, "R2", |h| h.consecutive_failures == 2)
        .await;
    assert_eq!(
        harness.supervisor.health().snapshot().pending_domains,
        1,
        "R2"
    );
    harness.supervisor.wake();
    harness
        .settle(3, "R3", |h| h.consecutive_failures == 0)
        .await;
    let pending = harness.supervisor.health().snapshot().pending_domains;
    let filters = harness.registry.registration_intent_filters();
    assert_eq!(pending, 0, "R3");
    assert_eq!(
        filters[..3],
        [
            EnvironmentRegistrationIntentFilter::All,
            EnvironmentRegistrationIntentFilter::Pending,
            EnvironmentRegistrationIntentFilter::Pending,
        ],
        "R1-R3 the Environment domain owns its own recovery boundary"
    );
    harness.stop().await;
}

/// Negative-control mutant for T41-C02: collapse every service state into the
/// serving-ready bool.
fn merged_bool(snapshot: RegistrationHealthSnapshot) -> bool {
    snapshot.ready
}

/// T41-C02: H1 while the custom Environment registration is failing, serving
/// stays ready and the health snapshot reports exactly one degraded domain,
/// matching the absent executable capability; H2 after recovery the degraded
/// signal clears exactly when the capability is served. The merged-bool
/// mutant reports the same value in H1 and H2 although capability differs,
/// which the correspondence check detects.
#[tokio::test]
async fn health_snapshot_tracks_the_real_executable_capability() {
    let harness = harness(0, true).await;
    harness
        .settle(1, "H1", |h| h.consecutive_failures == 1)
        .await;
    let degraded = harness.supervisor.health().snapshot();
    let served_degraded = harness.registrar.serves_custom();
    assert!(degraded.ready, "H1 serving readiness is isolated");
    assert_eq!(degraded.pending_domains, 1, "H1");
    assert!(degraded.consecutive_failures >= 1, "H1");
    assert!(!served_degraded, "H1 the custom Environment is not served");
    assert_eq!(
        degraded.pending_domains == 0,
        served_degraded,
        "H1 correspondence"
    );

    harness.supervisor.wake();
    harness
        .settle(2, "H2", |h| {
            h.pending_domains == 0 && h.consecutive_failures == 0
        })
        .await;
    let recovered = harness.supervisor.health().snapshot();
    let served_recovered = harness.registrar.serves_custom();
    assert!(recovered.ready, "H2");
    assert_eq!(recovered.pending_domains, 0, "H2");
    assert_eq!(recovered.consecutive_failures, 0, "H2");
    assert!(served_recovered, "H2 the custom Environment is served");
    assert_eq!(
        recovered.pending_domains == 0,
        served_recovered,
        "H2 correspondence"
    );

    assert_eq!(
        merged_bool(degraded),
        merged_bool(recovered),
        "NC the merged bool cannot tell H1 from H2"
    );
    assert_ne!(
        merged_bool(degraded),
        served_degraded,
        "NC the correspondence check rejects the merged bool in H1"
    );
    harness.stop().await;
}
