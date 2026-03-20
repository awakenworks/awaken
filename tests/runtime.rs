#![allow(missing_docs)]

use awaken::*;

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct HandoffState {
    active_agent: Option<String>,
    requested_agent: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum HandoffAction {
    Request { agent: String },
    Activate { agent: String },
    Clear,
}

impl HandoffState {
    fn reduce(&mut self, action: HandoffAction) {
        match action {
            HandoffAction::Request { agent } => self.requested_agent = Some(agent),
            HandoffAction::Activate { agent } => {
                self.active_agent = Some(agent);
                self.requested_agent = None;
            }
            HandoffAction::Clear => {
                self.active_agent = None;
                self.requested_agent = None;
            }
        }
    }
}

struct HandoffChannel;

impl StateSlot for HandoffChannel {
    const KEY: &'static str = "handoff.state";
    type Value = HandoffState;
    type Update = HandoffAction;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        value.reduce(update);
    }
}

#[derive(Clone)]
struct HandoffPlugin;

impl StatePlugin for HandoffPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            name: "handoff-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_slot::<HandoffChannel>(SlotOptions::default())?;
        Ok(())
    }
}

impl RuntimePlugin for HandoffPlugin {
    fn register_runtime(&self, registrar: &mut RuntimePluginRegistrar) -> Result<(), StateError> {
        registrar.register_scheduled_action::<ActivateRequested, _>(ActivateRequestedHandler)?;
        Ok(())
    }
}

struct ActivateRequested;

impl ScheduledActionSpec for ActivateRequested {
    const KEY: &'static str = "handoff.activate_requested";
    const PHASE: Phase = Phase::BeforeInference;
    type Payload = ();
}

struct ActivateRequestedHandler;

impl TypedScheduledActionHandler<ActivateRequested> for ActivateRequestedHandler {
    fn handle_typed(&self, ctx: &PhaseContext, _payload: ()) -> Result<StateCommand, StateError> {
        let mut cmd = StateCommand::new().with_base_revision(ctx.snapshot.revision());
        if let Some(state) = ctx.get::<HandoffChannel>()
            && let Some(agent) = state.requested_agent.clone()
        {
            cmd.update::<HandoffChannel>(HandoffAction::Activate {
                agent: agent.clone(),
            });
            cmd.effect(RuntimeEffect::AddSystemReminder {
                message: format!("handoff activated: {agent}"),
            })?;
        }
        Ok(cmd)
    }
}

#[derive(Clone, Default)]
struct RuntimeEffectRecorder(Arc<Mutex<Vec<RuntimeEffect>>>);

impl TypedEffectHandler<RuntimeEffect> for RuntimeEffectRecorder {
    fn handle_typed(&self, payload: RuntimeEffect, _snapshot: &Snapshot) -> Result<(), String> {
        self.0.lock().expect("lock poisoned").push(payload);
        Ok(())
    }
}

struct FailingRuntimeEffectHandler;

impl TypedEffectHandler<RuntimeEffect> for FailingRuntimeEffectHandler {
    fn handle_typed(&self, _payload: RuntimeEffect, _snapshot: &Snapshot) -> Result<(), String> {
        Err("synthetic failure".into())
    }
}

struct AlwaysFailingAction;

impl ScheduledActionSpec for AlwaysFailingAction {
    const KEY: &'static str = "test.always_failing";
    const PHASE: Phase = Phase::BeforeInference;
    type Payload = ();
}

struct AlwaysFailingHandler;

impl TypedScheduledActionHandler<AlwaysFailingAction> for AlwaysFailingHandler {
    fn handle_typed(&self, _ctx: &PhaseContext, _payload: ()) -> Result<StateCommand, StateError> {
        Err(StateError::UnknownSlot {
            key: "synthetic".into(),
        })
    }
}

struct SpawnOnceAction;

impl ScheduledActionSpec for SpawnOnceAction {
    const KEY: &'static str = "test.spawn_once";
    const PHASE: Phase = Phase::BeforeInference;
    type Payload = ();
}

struct SpawnOnceHandler;

impl TypedScheduledActionHandler<SpawnOnceAction> for SpawnOnceHandler {
    fn handle_typed(&self, ctx: &PhaseContext, _payload: ()) -> Result<StateCommand, StateError> {
        let mut cmd = StateCommand::new().with_base_revision(ctx.snapshot.revision());
        cmd.schedule_action::<FinishAction>(()).unwrap();
        Ok(cmd)
    }
}

struct FinishAction;

impl ScheduledActionSpec for FinishAction {
    const KEY: &'static str = "test.finish";
    const PHASE: Phase = Phase::BeforeInference;
    type Payload = ();
}

struct FinishHandler;

impl TypedScheduledActionHandler<FinishAction> for FinishHandler {
    fn handle_typed(&self, _ctx: &PhaseContext, _payload: ()) -> Result<StateCommand, StateError> {
        Ok(StateCommand::new())
    }
}

struct InfiniteLoopAction;

impl ScheduledActionSpec for InfiniteLoopAction {
    const KEY: &'static str = "test.infinite_loop";
    const PHASE: Phase = Phase::BeforeInference;
    type Payload = ();
}

struct InfiniteLoopHandler;

impl TypedScheduledActionHandler<InfiniteLoopAction> for InfiniteLoopHandler {
    fn handle_typed(&self, ctx: &PhaseContext, _payload: ()) -> Result<StateCommand, StateError> {
        let mut cmd = StateCommand::new().with_base_revision(ctx.snapshot.revision());
        cmd.schedule_action::<InfiniteLoopAction>(()).unwrap();
        Ok(cmd)
    }
}

#[test]
fn unregistered_action_handler_is_rejected_on_submit() {
    let app = AppRuntime::new().unwrap();
    let mut cmd = StateCommand::new();
    cmd.schedule_action::<ActivateRequested>(()).unwrap();
    let err = app.submit_command(cmd).unwrap_err();
    assert!(matches!(
        err,
        StateError::UnknownScheduledActionHandler { .. }
    ));
}

#[test]
fn unregistered_effect_handler_is_rejected_on_submit() {
    let app = AppRuntime::new().unwrap();
    let mut cmd = StateCommand::new();
    cmd.effect(RuntimeEffect::PublishJson {
        topic: "test".into(),
        payload: serde_json::json!(null),
    })
    .unwrap();
    let err = app.submit_command(cmd).unwrap_err();
    assert!(matches!(err, StateError::UnknownEffectHandler { .. }));
}

#[test]
fn phase_runtime_stages_and_reduces_actions() {
    let store = StateStore::new();
    let runtime = PhaseRuntime::new(store.clone()).unwrap();
    runtime.install_plugin(HandoffPlugin).unwrap();
    let recorder = RuntimeEffectRecorder::default();
    runtime
        .register_effect::<RuntimeEffect, _>(recorder.clone())
        .unwrap();

    let mut cmd = StateCommand::new().with_base_revision(store.revision());
    cmd.update::<HandoffChannel>(HandoffAction::Request {
        agent: "fast".into(),
    });
    cmd.schedule_action::<ActivateRequested>(()).unwrap();
    runtime.submit_command(cmd).unwrap();

    assert_eq!(
        store
            .read_slot::<PendingScheduledActions>()
            .unwrap_or_default()
            .len(),
        1
    );

    let report = runtime.run_phase(Phase::BeforeInference).unwrap();
    assert_eq!(report.processed_scheduled_actions, 1);
    assert_eq!(report.effect_report.dispatched, 1);

    let handoff = store.read_slot::<HandoffChannel>().unwrap();
    assert_eq!(handoff.active_agent.as_deref(), Some("fast"));
    assert_eq!(handoff.requested_agent, None);
    assert_eq!(
        store
            .read_slot::<PendingScheduledActions>()
            .unwrap_or_default()
            .len(),
        0
    );
    assert_eq!(
        recorder.0.lock().expect("lock poisoned").clone(),
        vec![RuntimeEffect::AddSystemReminder {
            message: "handoff activated: fast".into(),
        }]
    );
}

#[test]
fn effect_failures_are_reported_immediately() {
    let store = StateStore::new();
    let runtime = PhaseRuntime::new(store.clone()).unwrap();
    runtime
        .register_effect::<RuntimeEffect, _>(FailingRuntimeEffectHandler)
        .unwrap();

    let mut cmd = StateCommand::new();
    cmd.effect(RuntimeEffect::PublishJson {
        topic: "demo".into(),
        payload: serde_json::json!({"ok": true}),
    })
    .unwrap();
    let report = runtime.submit_command(cmd).unwrap();
    assert_eq!(report.effect_report.attempted, 1);
    assert_eq!(report.effect_report.failed, 1);
    assert_eq!(store.read_slot::<EffectLog>().unwrap_or_default().len(), 1);
}

#[test]
fn app_runtime_wraps_store_and_phase_runtime() {
    let app = AppRuntime::new().unwrap();
    app.install_plugin(HandoffPlugin).unwrap();
    app.phase_runtime()
        .register_effect::<RuntimeEffect, _>(RuntimeEffectRecorder::default())
        .unwrap();

    let mut cmd = StateCommand::new().with_base_revision(app.revision());
    cmd.update::<HandoffChannel>(HandoffAction::Request {
        agent: "planner".into(),
    });
    cmd.schedule_action::<ActivateRequested>(()).unwrap();
    app.submit_command(cmd).unwrap();

    let report = app.run_phase(Phase::BeforeInference).unwrap();
    assert_eq!(report.processed_scheduled_actions, 1);
    assert_eq!(
        app.store()
            .read_slot::<HandoffChannel>()
            .unwrap()
            .active_agent
            .as_deref(),
        Some("planner")
    );
}

#[test]
fn runtime_logs_actions_and_effects() {
    let app = AppRuntime::new().unwrap();
    app.install_plugin(HandoffPlugin).unwrap();
    app.phase_runtime()
        .register_effect::<RuntimeEffect, _>(RuntimeEffectRecorder::default())
        .unwrap();

    let mut cmd = StateCommand::new().with_base_revision(app.revision());
    cmd.update::<HandoffChannel>(HandoffAction::Request {
        agent: "logger".into(),
    });
    cmd.schedule_action::<ActivateRequested>(()).unwrap();
    cmd.effect(RuntimeEffect::PublishJson {
        topic: "demo".into(),
        payload: serde_json::json!({"kind":"log"}),
    })
    .unwrap();
    app.submit_command(cmd).unwrap();

    let scheduled_action_log = app
        .store()
        .read_slot::<ScheduledActionLog>()
        .unwrap_or_default();
    let effect_log = app.store().read_slot::<EffectLog>().unwrap_or_default();

    assert_eq!(scheduled_action_log.len(), 1);
    assert_eq!(scheduled_action_log[0].key, ActivateRequested::KEY);
    assert_eq!(effect_log.len(), 1);
    assert_eq!(effect_log[0].key, RuntimeEffect::KEY);
}

#[test]
fn duplicate_typed_handler_registration_is_rejected() {
    let app = AppRuntime::new().unwrap();
    app.phase_runtime()
        .register_scheduled_action::<ActivateRequested, _>(ActivateRequestedHandler)
        .unwrap();
    let err = app
        .phase_runtime()
        .register_scheduled_action::<ActivateRequested, _>(ActivateRequestedHandler)
        .unwrap_err();
    assert!(matches!(err, StateError::HandlerAlreadyRegistered { .. }));
}

#[test]
fn runtime_plugin_can_be_uninstalled_and_reinstalled() {
    let app = AppRuntime::new().unwrap();
    app.install_plugin(HandoffPlugin).unwrap();
    app.phase_runtime()
        .register_effect::<RuntimeEffect, _>(RuntimeEffectRecorder::default())
        .unwrap();
    app.uninstall_plugin::<HandoffPlugin>().unwrap();
    assert!(app.store().read_slot::<HandoffChannel>().is_none());

    app.install_plugin(HandoffPlugin).unwrap();

    let mut cmd = StateCommand::new().with_base_revision(app.revision());
    cmd.update::<HandoffChannel>(HandoffAction::Request {
        agent: "reloaded".into(),
    });
    cmd.schedule_action::<ActivateRequested>(()).unwrap();
    app.submit_command(cmd).unwrap();

    let report = app.run_phase(Phase::BeforeInference).unwrap();
    assert_eq!(report.processed_scheduled_actions, 1);
    assert_eq!(
        app.store()
            .read_slot::<HandoffChannel>()
            .unwrap()
            .active_agent
            .as_deref(),
        Some("reloaded")
    );
}

#[test]
fn failed_scheduled_actions_are_dead_lettered() {
    let app = AppRuntime::new().unwrap();
    app.phase_runtime()
        .register_scheduled_action::<AlwaysFailingAction, _>(AlwaysFailingHandler)
        .unwrap();

    let mut cmd = StateCommand::new();
    cmd.schedule_action::<AlwaysFailingAction>(()).unwrap();
    app.submit_command(cmd).unwrap();

    let report = app.run_phase(Phase::BeforeInference).unwrap();
    assert_eq!(report.failed_scheduled_actions, 1);
    assert_eq!(
        app.store()
            .read_slot::<PendingScheduledActions>()
            .unwrap_or_default()
            .len(),
        0
    );
    let failed = app
        .store()
        .read_slot::<FailedScheduledActions>()
        .unwrap_or_default();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].action.key, AlwaysFailingAction::KEY);
}

#[test]
fn run_phase_processes_same_phase_actions_across_rounds() {
    let app = AppRuntime::new().unwrap();
    app.phase_runtime()
        .register_scheduled_action::<SpawnOnceAction, _>(SpawnOnceHandler)
        .unwrap();
    app.phase_runtime()
        .register_scheduled_action::<FinishAction, _>(FinishHandler)
        .unwrap();

    let mut cmd = StateCommand::new();
    cmd.schedule_action::<SpawnOnceAction>(()).unwrap();
    app.submit_command(cmd).unwrap();

    let report = app.run_phase(Phase::BeforeInference).unwrap();
    assert_eq!(report.rounds, 3);
    assert_eq!(report.processed_scheduled_actions, 2);
    assert_eq!(
        app.store()
            .read_slot::<PendingScheduledActions>()
            .unwrap_or_default()
            .len(),
        0
    );
}

#[test]
fn run_phase_returns_error_on_infinite_loop() {
    let app = AppRuntime::new().unwrap();
    app.phase_runtime()
        .register_scheduled_action::<InfiniteLoopAction, _>(InfiniteLoopHandler)
        .unwrap();

    let mut cmd = StateCommand::new();
    cmd.schedule_action::<InfiniteLoopAction>(()).unwrap();
    app.submit_command(cmd).unwrap();

    let err = app.run_phase(Phase::BeforeInference).unwrap_err();
    assert!(matches!(
        err,
        StateError::PhaseRunLoopExceeded {
            phase: Phase::BeforeInference,
            max_rounds: DEFAULT_MAX_PHASE_ROUNDS,
        }
    ));
}

#[test]
fn run_phase_with_custom_limit() {
    let app = AppRuntime::new().unwrap();
    app.phase_runtime()
        .register_scheduled_action::<InfiniteLoopAction, _>(InfiniteLoopHandler)
        .unwrap();

    let mut cmd = StateCommand::new();
    cmd.schedule_action::<InfiniteLoopAction>(()).unwrap();
    app.submit_command(cmd).unwrap();

    let err = app
        .phase_runtime()
        .run_phase_with_limit(Phase::BeforeInference, 3)
        .unwrap_err();
    assert!(matches!(
        err,
        StateError::PhaseRunLoopExceeded {
            phase: Phase::BeforeInference,
            max_rounds: 3,
        }
    ));
}

#[test]
fn runtime_logs_can_be_trimmed() {
    let app = AppRuntime::new().unwrap();
    app.phase_runtime()
        .register_effect::<RuntimeEffect, _>(RuntimeEffectRecorder::default())
        .unwrap();

    for index in 0..3 {
        let mut cmd = StateCommand::new();
        cmd.effect(RuntimeEffect::PublishJson {
            topic: format!("demo-{index}"),
            payload: serde_json::json!({"i": index}),
        })
        .unwrap();
        app.submit_command(cmd).unwrap();
    }

    app.trim_logs(2).unwrap();

    let effect_log = app.store().read_slot::<EffectLog>().unwrap_or_default();
    assert_eq!(effect_log.len(), 2);
    assert!(
        effect_log
            .iter()
            .all(|entry| entry.key == RuntimeEffect::KEY)
    );
}

#[test]
fn runtime_logs_can_be_cleared() {
    let app = AppRuntime::new().unwrap();
    app.phase_runtime()
        .register_effect::<RuntimeEffect, _>(RuntimeEffectRecorder::default())
        .unwrap();

    let mut cmd = StateCommand::new();
    cmd.effect(RuntimeEffect::PublishJson {
        topic: "demo".into(),
        payload: serde_json::json!({"ok": true}),
    })
    .unwrap();
    app.submit_command(cmd).unwrap();

    app.clear_logs().unwrap();

    assert!(
        app.store()
            .read_slot::<EffectLog>()
            .unwrap_or_default()
            .is_empty()
    );
    assert!(
        app.store()
            .read_slot::<ScheduledActionLog>()
            .unwrap_or_default()
            .is_empty()
    );
}
