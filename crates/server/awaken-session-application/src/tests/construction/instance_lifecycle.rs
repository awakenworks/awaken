//! T01-C03 (实例与关闭), Session-application half, oracle O2: dropping the
//! application releases every use-case component, its Runtime and its store,
//! so no strong reference cycle or detached task keeps the instance alive.
//!
//! Oracles (groups JSON order): O0 foreign instances are refused with zero
//! reads (parent B011); O1 a repeated binding is refused (parent
//! `fixture_execution_completion_validates_existing_bindings_before_filling_missing_slots`
//! R3); O2 no strong reference cycle, leftover task or fabricated shutdown.
//!
//! Negative control: "keeping an original strong reference (or a missed join)
//! must expose the unclosed resource". It is executed: one component handle
//! is retained across the drop, and the same liveness detector that reports an
//! empty set for the clean drop names that component.
use super::*;

type LivenessCheck = Box<dyn Fn() -> bool>;

/// Weak handles to every component, checked after the owner is dropped.
#[derive(Default)]
struct Liveness {
    checks: Vec<(&'static str, LivenessCheck)>,
}

impl Liveness {
    fn track<T: ?Sized + 'static>(&mut self, name: &'static str, component: &Arc<T>) {
        let weak = Arc::downgrade(component);
        self.checks
            .push((name, Box::new(move || weak.upgrade().is_some())));
    }

    fn of(app: &SessionApplication) -> Self {
        let mut liveness = Self::default();
        liveness.track("mutations", &app.mutations);
        liveness.track("lifecycle", &app.lifecycle);
        liveness.track("queries", &app.queries);
        liveness.track("inputs", &app.inputs);
        liveness.track("credentials", &app.credentials);
        liveness.track("work_assignments", &app.work_assignments);
        liveness.track("worker_occupancy", &app.worker_occupancy);
        liveness.track("realizations", &app.realizations);
        liveness.track("terminal_cleanups", &app.terminal_cleanups);
        liveness.track("continuations", &app.continuations);
        liveness.track("activities", &app.activities);
        liveness.track("termination_commands", &app.termination_commands);
        liveness.track("update_commands", &app.update_commands);
        liveness.track("resource_manifests", &app.resource_manifests);
        liveness.track("creation_commands", &app.creation_commands);
        liveness.track("run_admissions", &app.run_admissions);
        liveness.track("observations", &app.observations);
        liveness.track("coordination", &app.coordination);
        liveness.track("event_batches", &app.event_batches);
        liveness.track("recovery", &app.recovery);
        liveness.track("execution_admission", &app.execution_admission);
        liveness
    }

    fn alive(&self) -> Vec<&'static str> {
        self.checks
            .iter()
            .filter(|(_, alive)| alive())
            .map(|(name, _)| *name)
            .collect()
    }
}

async fn used_application(
    id: &str,
) -> (
    SessionApplication,
    Arc<creation::RecordingCreateRuntime>,
    Arc<awaken_session_store::SqliteManagedSessionRepository>,
) {
    let store = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("instance repository"),
    );
    let runtime = Arc::new(creation::RecordingCreateRuntime::default());
    let app = SessionApplicationBuilder::fixture(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        SessionPersistence::new(store.clone(), system_clock()),
        Arc::new(RecordingEnvironmentSource::default()),
        Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
    .build()
    .expect("fixture application");
    // Exercise the instance so lazily created state and any spawned work
    // exist before the drop.
    app.creation_commands
        .create_session(creation::creation_command(id))
        .await
        .expect("used instance");
    (app, runtime, store)
}

#[tokio::test]
async fn dropping_the_application_releases_every_component_runtime_and_store() {
    let (app, runtime, store) = used_application("instance-clean-drop").await;
    let liveness = Liveness::of(&app);
    assert_eq!(
        liveness.alive().len(),
        liveness.checks.len(),
        "all live while owned"
    );
    drop(app);
    tokio::task::yield_now().await;
    assert_eq!(
        liveness.alive(),
        Vec::<&str>::new(),
        "O2 no cycle or detached task retains a component"
    );
    assert_eq!(Arc::strong_count(&runtime), 1, "O2 the Runtime is released");
    assert_eq!(Arc::strong_count(&store), 1, "O2 the store is released");
}

#[tokio::test]
async fn a_retained_component_reference_is_reported_by_the_liveness_detector() {
    let (app, _runtime, _store) = used_application("instance-leaked-drop").await;
    let liveness = Liveness::of(&app);
    // NC: an original strong reference survives the owner's drop.
    let leak = app.run_admissions.clone();
    drop(app);
    tokio::task::yield_now().await;
    let alive = liveness.alive();
    assert!(
        alive.contains(&"run_admissions"),
        "NC the detector names the retained component: {alive:?}"
    );
    drop(leak);
    assert_eq!(
        liveness.alive(),
        Vec::<&str>::new(),
        "releasing the leaked handle closes the instance"
    );
}
