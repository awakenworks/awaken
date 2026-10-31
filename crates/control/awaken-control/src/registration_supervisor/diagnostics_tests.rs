//! T41-C01: stable, secret-free diagnostics from the static registration
//! supervisor (production entrypoint `StaticRegistrationSupervisor::start_with_config`).
//!
//! Causes: C1 Agent recovery fails with an error whose text carries a
//! credential URL and token canary; C2 Environment registrar fails with the
//! same canary; C3 the supervisor publishes health and tracing diagnostics.
//!
//! | Rule | observed surface | oracle |
//! |---|---|---|
//! | D1 | health snapshot | degraded (2 pending domains, >=1 failure), process stays ready |
//! | D2 | snapshot Debug | no canary |
//! | D3 | captured events | one diagnostic per pending domain from this module |
//! | D4 | captured events | no canary in any field (reproducer) |
//! | D5 | captured events | stable `code`, `domain`, `recovery` fields (reproducer) |
//!
//! Negative controls executed through the same checks: a leaking emitter
//! (prints the raw error) must trip the canary check, and an English-text-only
//! emitter must trip the stable-code check; a reference emitter passes both.
//!
//! Capture uses a scoped local subscriber (same design as
//! `awaken_reliability_testkit::CapturedEvents`): adding that dev-dependency
//! to this crate would change Cargo.lock.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use awaken_environment_application::EnvironmentApplication;
use awaken_executable_environment_contract::{
    ExecutableEnvironmentRegistrar, ExecutableEnvironmentRegistration,
    ExecutableEnvironmentRegistrationError, ExecutableEnvironmentRegistrationOutcome,
    ExecutableEnvironmentWithdrawal, ExecutableEnvironmentWithdrawalOutcome,
};

use super::*;

const CANARY: &str = "CANARY";
const CANARY_ERROR: &str = "postgres://u:CANARY@h/db token=CANARY";
const SUPERVISOR_TARGET: &str = "awaken_control::registration_supervisor";

type Event = BTreeMap<String, String>;

#[derive(Clone)]
struct Captured {
    events: Arc<Mutex<Vec<Event>>>,
    next_span: Arc<AtomicU64>,
    _interest_guard: tracing::Dispatch,
}

impl Default for Captured {
    fn default() -> Self {
        Self {
            events: Arc::default(),
            next_span: Arc::default(),
            _interest_guard: tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()),
        }
    }
}

impl Captured {
    fn snapshot(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }
}

struct Fields(Event);

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().into(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().into(), value.into());
    }
}

impl tracing::Subscriber for Captured {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(self.next_span.fetch_add(1, Ordering::Relaxed) + 1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut fields = Fields(BTreeMap::new());
        event.record(&mut fields);
        fields
            .0
            .insert("target".into(), event.metadata().target().into());
        self.events.lock().unwrap().push(fields.0);
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

/// D4 oracle: every canary-bearing field (key, value) of every event.
fn canary_leaks(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .flat_map(|event| event.iter())
        .filter(|(key, value)| key.contains(CANARY) || value.contains(CANARY))
        .map(|(key, value)| format!("{key}={value}"))
        .collect()
}

/// D5 oracle: diagnostics that lack a stable code, domain identity or
/// recovery direction (the English `message` is deliberately not accepted).
fn unstable_diagnostics(events: &[Event]) -> Vec<Event> {
    events
        .iter()
        .filter(|event| {
            ["code", "domain", "recovery"].iter().any(|field| {
                event
                    .get(*field)
                    .is_none_or(|value| value.trim().is_empty())
            })
        })
        .cloned()
        .collect()
}

fn supervisor_events(events: &[Event]) -> Vec<Event> {
    events
        .iter()
        .filter(|event| event.get("target").map(String::as_str) == Some(SUPERVISOR_TARGET))
        .cloned()
        .collect()
}

struct LeakingAgentRecovery;

#[async_trait::async_trait]
impl PublicationBindingReconciler for LeakingAgentRecovery {
    async fn reconcile(&self) -> Result<usize, String> {
        Ok(0)
    }

    async fn reconcile_all(&self) -> Result<usize, String> {
        Ok(0)
    }

    async fn recover_registrations(&self) -> Result<usize, String> {
        Err(CANARY_ERROR.into())
    }
}

struct LeakingRegistrar;

#[async_trait::async_trait]
impl ExecutableEnvironmentRegistrar for LeakingRegistrar {
    async fn restore_historical_registration(
        &self,
        registration: ExecutableEnvironmentRegistration,
    ) -> Result<(), ExecutableEnvironmentRegistrationError> {
        registration.validate()
    }

    async fn register(
        &self,
        _registration: ExecutableEnvironmentRegistration,
    ) -> Result<ExecutableEnvironmentRegistrationOutcome, ExecutableEnvironmentRegistrationError>
    {
        Err(ExecutableEnvironmentRegistrationError::Unavailable(
            CANARY_ERROR.into(),
        ))
    }

    async fn withdraw(
        &self,
        _withdrawal: ExecutableEnvironmentWithdrawal,
    ) -> Result<ExecutableEnvironmentWithdrawalOutcome, ExecutableEnvironmentRegistrationError>
    {
        Ok(ExecutableEnvironmentWithdrawalOutcome::WithdrawnCurrent)
    }
}

/// Run one degraded supervisor pass under capture and return (snapshot, events).
async fn degraded_pass() -> (RegistrationHealthSnapshot, Vec<Event>) {
    let captured = Captured::default();
    let _guard = tracing::subscriber::set_default(captured.clone());
    let envs = Arc::new(awaken_env_store::InMemoryEnvRegistry::new(
        awaken_time::system_clock(),
    ));
    let environments = Arc::new(EnvironmentApplication::new(
        envs,
        Arc::new(LeakingRegistrar),
        Arc::new(awaken_sandbox_policy_store::InMemorySandboxExecutionPolicyStore::default()),
    ));
    let service_lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
    let supervisor = StaticRegistrationSupervisor::start_with_config(
        Arc::new(LeakingAgentRecovery),
        environments.registration_delivery(),
        RegistrationSupervisorConfig {
            retry_min: Duration::from_secs(60),
            retry_max: Duration::from_secs(60),
            settled_interval: Duration::from_secs(60),
        },
        &service_lifecycle,
    );
    for _ in 0..1_000 {
        if supervisor.health().snapshot().consecutive_failures >= 1
            && supervisor_events(&captured.snapshot()).len() >= 2
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    let snapshot = supervisor.health().snapshot();
    service_lifecycle
        .shutdown(Duration::from_secs(1))
        .await
        .expect("test supervisor stops cooperatively");
    (snapshot, supervisor_events(&captured.snapshot()))
}

#[tokio::test]
async fn degraded_registration_health_is_observable_secret_free_and_leaks_are_detected() {
    let (snapshot, events) = degraded_pass().await;
    // D1 degraded but serving.
    assert!(snapshot.ready, "D1 process readiness is preserved");
    assert_eq!(snapshot.pending_domains, 2, "D1 both domains pending");
    assert!(snapshot.consecutive_failures >= 1, "D1 failure counted");
    // D2 snapshot is secret-free.
    assert!(
        !format!("{snapshot:?}").contains(CANARY),
        "D2 snapshot Debug: {snapshot:?}"
    );
    // D3 one diagnostic per pending domain was emitted by the supervisor.
    assert!(events.len() >= 2, "D3 events: {events:?}");

    // Negative controls through the same checks.
    let captured = Captured::default();
    {
        let _guard = tracing::subscriber::set_default(captured.clone());
        let error = CANARY_ERROR.to_string();
        tracing::warn!(error = %error, "leaking emitter");
    }
    let leaking = captured.snapshot();
    assert!(
        !canary_leaks(&leaking).is_empty(),
        "NC: a leaking emitter must trip the canary check"
    );

    let captured = Captured::default();
    {
        let _guard = tracing::subscriber::set_default(captured.clone());
        tracing::warn!("Agent registration recovery remains pending");
    }
    assert_eq!(
        unstable_diagnostics(&captured.snapshot()).len(),
        1,
        "NC: an English-text-only diagnostic must trip the stable-code check"
    );

    let captured = Captured::default();
    {
        let _guard = tracing::subscriber::set_default(captured.clone());
        tracing::warn!(
            code = "registration_recovery_pending",
            domain = "agent",
            recovery = "retry_with_backoff",
            "reference emitter"
        );
    }
    let reference = captured.snapshot();
    assert!(
        canary_leaks(&reference).is_empty(),
        "reference is secret-free"
    );
    assert!(
        unstable_diagnostics(&reference).is_empty(),
        "reference carries code/domain/recovery"
    );
}

#[tokio::test]
async fn degraded_registration_diagnostics_never_contain_the_secret_canary() {
    let (_, events) = degraded_pass().await;
    assert!(events.len() >= 2, "diagnostics emitted: {events:?}");
    assert_eq!(canary_leaks(&events), Vec::<String>::new(), "D4");
}

#[tokio::test]
async fn degraded_registration_diagnostics_carry_stable_code_identity_and_recovery() {
    let (_, events) = degraded_pass().await;
    assert!(events.len() >= 2, "diagnostics emitted: {events:?}");
    assert_eq!(unstable_diagnostics(&events), Vec::<Event>::new(), "D5");
    let domains = events
        .iter()
        .filter_map(|event| event.get("domain").cloned())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        domains.len(),
        2,
        "D5 each pending domain is identified: {domains:?}"
    );
}
