#![allow(missing_docs)]

use awaken::*;

use async_trait::async_trait;
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
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

impl StateKey for HandoffChannel {
    const KEY: &'static str = "handoff.state";
    type Value = HandoffState;
    type Update = HandoffAction;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        value.reduce(update);
    }
}

#[derive(Clone)]
struct HandoffPlugin;

impl Plugin for HandoffPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "handoff-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_key::<HandoffChannel>(StateKeyOptions::default())?;
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

#[async_trait]
impl TypedScheduledActionHandler<ActivateRequested> for ActivateRequestedHandler {
    async fn handle_typed(
        &self,
        ctx: &PhaseContext,
        _payload: (),
    ) -> Result<StateCommand, StateError> {
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

#[async_trait]
impl TypedEffectHandler<RuntimeEffect> for RuntimeEffectRecorder {
    async fn handle_typed(
        &self,
        payload: RuntimeEffect,
        _snapshot: &Snapshot,
    ) -> Result<(), String> {
        self.0.lock().expect("lock poisoned").push(payload);
        Ok(())
    }
}

/// Plugin wrapper for RuntimeEffect handler.
struct RuntimeEffectPlugin<H: TypedEffectHandler<RuntimeEffect> + Clone + Send + Sync + 'static>(H);

impl<H: TypedEffectHandler<RuntimeEffect> + Clone + Send + Sync + 'static> Plugin
    for RuntimeEffectPlugin<H>
{
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "runtime-effect-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_effect::<RuntimeEffect, _>(self.0.clone())?;
        Ok(())
    }
}

#[derive(Clone)]
struct FailingRuntimeEffectHandler;

#[async_trait]
impl TypedEffectHandler<RuntimeEffect> for FailingRuntimeEffectHandler {
    async fn handle_typed(
        &self,
        _payload: RuntimeEffect,
        _snapshot: &Snapshot,
    ) -> Result<(), String> {
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

#[async_trait]
impl TypedScheduledActionHandler<AlwaysFailingAction> for AlwaysFailingHandler {
    async fn handle_typed(
        &self,
        _ctx: &PhaseContext,
        _payload: (),
    ) -> Result<StateCommand, StateError> {
        Err(StateError::UnknownKey {
            key: "synthetic".into(),
        })
    }
}

/// Plugin wrapper for AlwaysFailingAction handler.
struct AlwaysFailingPlugin;

impl Plugin for AlwaysFailingPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "always-failing-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_scheduled_action::<AlwaysFailingAction, _>(AlwaysFailingHandler)?;
        Ok(())
    }
}

struct SpawnOnceAction;

impl ScheduledActionSpec for SpawnOnceAction {
    const KEY: &'static str = "test.spawn_once";
    const PHASE: Phase = Phase::BeforeInference;
    type Payload = ();
}

struct SpawnOnceHandler;

#[async_trait]
impl TypedScheduledActionHandler<SpawnOnceAction> for SpawnOnceHandler {
    async fn handle_typed(
        &self,
        ctx: &PhaseContext,
        _payload: (),
    ) -> Result<StateCommand, StateError> {
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

#[async_trait]
impl TypedScheduledActionHandler<FinishAction> for FinishHandler {
    async fn handle_typed(
        &self,
        _ctx: &PhaseContext,
        _payload: (),
    ) -> Result<StateCommand, StateError> {
        Ok(StateCommand::new())
    }
}

/// Plugin that registers SpawnOnce + Finish action handlers.
struct SpawnOncePlugin;

impl Plugin for SpawnOncePlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "spawn-once-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_scheduled_action::<SpawnOnceAction, _>(SpawnOnceHandler)?;
        registrar.register_scheduled_action::<FinishAction, _>(FinishHandler)?;
        Ok(())
    }
}

struct InfiniteLoopAction;

impl ScheduledActionSpec for InfiniteLoopAction {
    const KEY: &'static str = "test.infinite_loop";
    const PHASE: Phase = Phase::BeforeInference;
    type Payload = ();
}

struct InfiniteLoopHandler;

#[async_trait]
impl TypedScheduledActionHandler<InfiniteLoopAction> for InfiniteLoopHandler {
    async fn handle_typed(
        &self,
        ctx: &PhaseContext,
        _payload: (),
    ) -> Result<StateCommand, StateError> {
        let mut cmd = StateCommand::new().with_base_revision(ctx.snapshot.revision());
        cmd.schedule_action::<InfiniteLoopAction>(()).unwrap();
        Ok(cmd)
    }
}

/// Plugin for InfiniteLoopAction handler.
struct InfiniteLoopPlugin;

impl Plugin for InfiniteLoopPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "infinite-loop-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_scheduled_action::<InfiniteLoopAction, _>(InfiniteLoopHandler)?;
        Ok(())
    }
}

struct OtherPhaseAction;

impl ScheduledActionSpec for OtherPhaseAction {
    const KEY: &'static str = "test.other_phase";
    const PHASE: Phase = Phase::AfterInference;
    type Payload = ();
}

struct OtherPhaseHandler;

#[async_trait]
impl TypedScheduledActionHandler<OtherPhaseAction> for OtherPhaseHandler {
    async fn handle_typed(
        &self,
        _ctx: &PhaseContext,
        _payload: (),
    ) -> Result<StateCommand, StateError> {
        Ok(StateCommand::new())
    }
}

/// Plugin for OtherPhaseAction handler.
struct OtherPhasePlugin;

impl Plugin for OtherPhasePlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "other-phase-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_scheduled_action::<OtherPhaseAction, _>(OtherPhaseHandler)?;
        Ok(())
    }
}

struct LogOnlyAction;

impl ScheduledActionSpec for LogOnlyAction {
    const KEY: &'static str = "test.log_only";
    const PHASE: Phase = Phase::BeforeInference;
    type Payload = ();
}

struct LogOnlyHandler;

#[async_trait]
impl TypedScheduledActionHandler<LogOnlyAction> for LogOnlyHandler {
    async fn handle_typed(
        &self,
        _ctx: &PhaseContext,
        _payload: (),
    ) -> Result<StateCommand, StateError> {
        Ok(StateCommand::new())
    }
}

/// Plugin for LogOnlyAction handler.
struct LogOnlyPlugin;

impl Plugin for LogOnlyPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "log-only-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_scheduled_action::<LogOnlyAction, _>(LogOnlyHandler)?;
        Ok(())
    }
}

struct BadlyEncodedAction;

impl ScheduledActionSpec for BadlyEncodedAction {
    const KEY: &'static str = "test.badly_encoded";
    const PHASE: Phase = Phase::BeforeInference;
    type Payload = String;

    fn encode_payload(_payload: &Self::Payload) -> Result<JsonValue, StateError> {
        Ok(serde_json::json!(42))
    }
}

struct BadlyEncodedActionHandler;

#[async_trait]
impl TypedScheduledActionHandler<BadlyEncodedAction> for BadlyEncodedActionHandler {
    async fn handle_typed(
        &self,
        _ctx: &PhaseContext,
        _payload: String,
    ) -> Result<StateCommand, StateError> {
        Ok(StateCommand::new())
    }
}

/// Plugin for BadlyEncodedAction handler.
struct BadlyEncodedPlugin;

impl Plugin for BadlyEncodedPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "badly-encoded-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_scheduled_action::<BadlyEncodedAction, _>(BadlyEncodedActionHandler)?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MismatchedPayload;

impl Serialize for MismatchedPayload {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(7)
    }
}

impl<'de> Deserialize<'de> for MismatchedPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct StringOnlyVisitor;

        impl Visitor<'_> for StringOnlyVisitor {
            type Value = MismatchedPayload;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a string payload")
            }

            fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(MismatchedPayload)
            }
        }

        deserializer.deserialize_str(StringOnlyVisitor)
    }
}

struct MismatchedEffect;

impl EffectSpec for MismatchedEffect {
    const KEY: &'static str = "test.mismatched_effect";
    type Payload = MismatchedPayload;
}

struct MismatchedEffectHandler;

#[async_trait]
impl TypedEffectHandler<MismatchedEffect> for MismatchedEffectHandler {
    async fn handle_typed(
        &self,
        _payload: MismatchedPayload,
        _snapshot: &Snapshot,
    ) -> Result<(), String> {
        Ok(())
    }
}

/// Plugin for MismatchedEffect handler.
struct MismatchedEffectPlugin;

impl Plugin for MismatchedEffectPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "mismatched-effect-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_effect::<MismatchedEffect, _>(MismatchedEffectHandler)?;
        Ok(())
    }
}

#[tokio::test]
async fn unregistered_action_handler_is_rejected_on_submit() {
    let app = AppRuntime::new().unwrap();
    let env = ExecutionEnv::empty();
    let mut cmd = StateCommand::new();
    cmd.schedule_action::<ActivateRequested>(()).unwrap();
    let err = app.submit_command(&env, cmd).await.unwrap_err();
    assert!(matches!(
        err,
        StateError::UnknownScheduledActionHandler { .. }
    ));
}

#[tokio::test]
async fn unregistered_effect_handler_is_rejected_on_submit() {
    let app = AppRuntime::new().unwrap();
    let env = ExecutionEnv::empty();
    let mut cmd = StateCommand::new();
    cmd.effect(RuntimeEffect::PublishJson {
        topic: "test".into(),
        payload: serde_json::json!(null),
    })
    .unwrap();
    let err = app.submit_command(&env, cmd).await.unwrap_err();
    assert!(matches!(err, StateError::UnknownEffectHandler { .. }));
}

#[tokio::test]
async fn phase_runtime_stages_and_reduces_actions() {
    let store = StateStore::new();
    let runtime = PhaseRuntime::new(store.clone()).unwrap();
    store.install_plugin(HandoffPlugin).unwrap();
    let recorder = RuntimeEffectRecorder::default();
    let plugins: Vec<Arc<dyn Plugin>> = vec![
        Arc::new(HandoffPlugin),
        Arc::new(RuntimeEffectPlugin(recorder.clone())),
    ];
    let env = ExecutionEnv::from_plugins(&plugins).unwrap();

    let mut cmd = StateCommand::new().with_base_revision(store.revision());
    cmd.update::<HandoffChannel>(HandoffAction::Request {
        agent: "fast".into(),
    });
    cmd.schedule_action::<ActivateRequested>(()).unwrap();
    runtime.submit_command(&env, cmd).await.unwrap();

    assert_eq!(
        store
            .read::<PendingScheduledActions>()
            .unwrap_or_default()
            .len(),
        1
    );

    let report = runtime
        .run_phase(&env, Phase::BeforeInference)
        .await
        .unwrap();
    assert_eq!(report.processed_scheduled_actions, 1);
    assert_eq!(report.effect_report.dispatched, 1);

    let handoff = store.read::<HandoffChannel>().unwrap();
    assert_eq!(handoff.active_agent.as_deref(), Some("fast"));
    assert_eq!(handoff.requested_agent, None);
    assert_eq!(
        store
            .read::<PendingScheduledActions>()
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

#[tokio::test]
async fn effect_failures_are_reported_immediately() {
    let store = StateStore::new();
    let runtime = PhaseRuntime::new(store.clone()).unwrap();
    let plugins: Vec<Arc<dyn Plugin>> =
        vec![Arc::new(RuntimeEffectPlugin(FailingRuntimeEffectHandler))];
    let env = ExecutionEnv::from_plugins(&plugins).unwrap();

    let mut cmd = StateCommand::new();
    cmd.effect(RuntimeEffect::PublishJson {
        topic: "demo".into(),
        payload: serde_json::json!({"ok": true}),
    })
    .unwrap();
    let report = runtime.submit_command(&env, cmd).await.unwrap();
    assert_eq!(report.effect_report.attempted, 1);
    assert_eq!(report.effect_report.failed, 1);
}

#[tokio::test]
async fn app_runtime_wraps_store_and_phase_runtime() {
    let app = AppRuntime::new().unwrap();
    app.store().install_plugin(HandoffPlugin).unwrap();
    let plugins: Vec<Arc<dyn Plugin>> = vec![
        Arc::new(HandoffPlugin),
        Arc::new(RuntimeEffectPlugin(RuntimeEffectRecorder::default())),
    ];
    let env = ExecutionEnv::from_plugins(&plugins).unwrap();

    let mut cmd = StateCommand::new().with_base_revision(app.revision());
    cmd.update::<HandoffChannel>(HandoffAction::Request {
        agent: "planner".into(),
    });
    cmd.schedule_action::<ActivateRequested>(()).unwrap();
    app.submit_command(&env, cmd).await.unwrap();

    let report = app.run_phase(&env, Phase::BeforeInference).await.unwrap();
    assert_eq!(report.processed_scheduled_actions, 1);
    assert_eq!(
        app.store()
            .read::<HandoffChannel>()
            .unwrap()
            .active_agent
            .as_deref(),
        Some("planner")
    );
}

#[test]
fn duplicate_typed_handler_registration_is_rejected() {
    // Two plugins registering the same action key → error from ExecutionEnv::from_plugins
    struct ActionPlugin1;
    impl Plugin for ActionPlugin1 {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor { name: "action1" }
        }
        fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
            r.register_scheduled_action::<ActivateRequested, _>(ActivateRequestedHandler)
        }
    }
    struct ActionPlugin2;
    impl Plugin for ActionPlugin2 {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor { name: "action2" }
        }
        fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
            r.register_scheduled_action::<ActivateRequested, _>(ActivateRequestedHandler)
        }
    }

    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(ActionPlugin1), Arc::new(ActionPlugin2)];
    let err = ExecutionEnv::from_plugins(&plugins)
        .err()
        .expect("should fail");
    assert!(matches!(err, StateError::HandlerAlreadyRegistered { .. }));
}

#[test]
fn duplicate_effect_handler_registration_is_rejected() {
    struct EffectPlugin1;
    impl Plugin for EffectPlugin1 {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor { name: "effect1" }
        }
        fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
            r.register_effect::<RuntimeEffect, _>(RuntimeEffectRecorder::default())
        }
    }
    struct EffectPlugin2;
    impl Plugin for EffectPlugin2 {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor { name: "effect2" }
        }
        fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
            r.register_effect::<RuntimeEffect, _>(RuntimeEffectRecorder::default())
        }
    }

    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(EffectPlugin1), Arc::new(EffectPlugin2)];
    let err = ExecutionEnv::from_plugins(&plugins)
        .err()
        .expect("should fail");
    assert!(matches!(
        err,
        StateError::EffectHandlerAlreadyRegistered { .. }
    ));
}

#[test]
fn duplicate_runtime_plugin_install_is_rejected() {
    let store = StateStore::new();
    store.install_plugin(HandoffPlugin).unwrap();

    let err = store.install_plugin(HandoffPlugin).unwrap_err();
    assert!(matches!(err, StateError::PluginAlreadyInstalled { .. }));
}

#[test]
fn uninstalling_unknown_runtime_plugin_is_rejected() {
    let store = StateStore::new();

    let err = store.uninstall_plugin::<HandoffPlugin>().unwrap_err();
    assert!(matches!(err, StateError::PluginNotInstalled { .. }));
}

#[tokio::test]
async fn runtime_plugin_can_be_uninstalled_and_reinstalled() {
    let app = AppRuntime::new().unwrap();
    app.store().install_plugin(HandoffPlugin).unwrap();

    let plugins: Vec<Arc<dyn Plugin>> = vec![
        Arc::new(HandoffPlugin),
        Arc::new(RuntimeEffectPlugin(RuntimeEffectRecorder::default())),
    ];
    let env = ExecutionEnv::from_plugins(&plugins).unwrap();

    // Write some state
    let mut cmd = StateCommand::new().with_base_revision(app.revision());
    cmd.update::<HandoffChannel>(HandoffAction::Request {
        agent: "test".into(),
    });
    app.submit_command(&env, cmd).await.unwrap();

    app.store().uninstall_plugin::<HandoffPlugin>().unwrap();
    assert!(app.store().read::<HandoffChannel>().is_none());

    app.store().install_plugin(HandoffPlugin).unwrap();

    let mut cmd = StateCommand::new().with_base_revision(app.revision());
    cmd.update::<HandoffChannel>(HandoffAction::Request {
        agent: "reloaded".into(),
    });
    cmd.schedule_action::<ActivateRequested>(()).unwrap();
    app.submit_command(&env, cmd).await.unwrap();

    let report = app.run_phase(&env, Phase::BeforeInference).await.unwrap();
    assert_eq!(report.processed_scheduled_actions, 1);
    assert_eq!(
        app.store()
            .read::<HandoffChannel>()
            .unwrap()
            .active_agent
            .as_deref(),
        Some("reloaded")
    );
}

#[tokio::test]
async fn failed_scheduled_actions_are_dead_lettered() {
    let app = AppRuntime::new().unwrap();
    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(AlwaysFailingPlugin)];
    let env = ExecutionEnv::from_plugins(&plugins).unwrap();

    let mut cmd = StateCommand::new();
    cmd.schedule_action::<AlwaysFailingAction>(()).unwrap();
    app.submit_command(&env, cmd).await.unwrap();

    let report = app.run_phase(&env, Phase::BeforeInference).await.unwrap();
    assert_eq!(report.failed_scheduled_actions, 1);
    assert_eq!(
        app.store()
            .read::<PendingScheduledActions>()
            .unwrap_or_default()
            .len(),
        0
    );
    let failed = app
        .store()
        .read::<FailedScheduledActions>()
        .unwrap_or_default();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].action.key, AlwaysFailingAction::KEY);
}

#[tokio::test]
async fn run_phase_processes_same_phase_actions_across_rounds() {
    let app = AppRuntime::new().unwrap();
    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(SpawnOncePlugin)];
    let env = ExecutionEnv::from_plugins(&plugins).unwrap();

    let mut cmd = StateCommand::new();
    cmd.schedule_action::<SpawnOnceAction>(()).unwrap();
    app.submit_command(&env, cmd).await.unwrap();

    let report = app.run_phase(&env, Phase::BeforeInference).await.unwrap();
    assert_eq!(report.rounds, 3);
    assert_eq!(report.processed_scheduled_actions, 2);
    assert_eq!(
        app.store()
            .read::<PendingScheduledActions>()
            .unwrap_or_default()
            .len(),
        0
    );
}

#[tokio::test]
async fn run_phase_reports_skipped_actions_from_other_phases() {
    let app = AppRuntime::new().unwrap();
    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(OtherPhasePlugin)];
    let env = ExecutionEnv::from_plugins(&plugins).unwrap();

    let mut cmd = StateCommand::new();
    cmd.schedule_action::<OtherPhaseAction>(()).unwrap();
    app.submit_command(&env, cmd).await.unwrap();

    let report = app.run_phase(&env, Phase::BeforeInference).await.unwrap();
    assert_eq!(report.processed_scheduled_actions, 0);
    assert_eq!(report.skipped_scheduled_actions, 1);
    assert_eq!(
        app.store()
            .read::<PendingScheduledActions>()
            .unwrap_or_default()
            .len(),
        1
    );
}

#[tokio::test]
async fn run_phase_returns_error_on_infinite_loop() {
    let app = AppRuntime::new().unwrap();
    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(InfiniteLoopPlugin)];
    let env = ExecutionEnv::from_plugins(&plugins).unwrap();

    let mut cmd = StateCommand::new();
    cmd.schedule_action::<InfiniteLoopAction>(()).unwrap();
    app.submit_command(&env, cmd).await.unwrap();

    let err = app
        .run_phase(&env, Phase::BeforeInference)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        StateError::PhaseRunLoopExceeded {
            phase: Phase::BeforeInference,
            max_rounds: DEFAULT_MAX_PHASE_ROUNDS,
        }
    ));
}

#[tokio::test]
async fn run_phase_with_custom_limit() {
    let app = AppRuntime::new().unwrap();
    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(InfiniteLoopPlugin)];
    let env = ExecutionEnv::from_plugins(&plugins).unwrap();

    let mut cmd = StateCommand::new();
    cmd.schedule_action::<InfiniteLoopAction>(()).unwrap();
    app.submit_command(&env, cmd).await.unwrap();

    let err = app
        .phase_runtime()
        .run_phase_with_limit(&env, Phase::BeforeInference, 3)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        StateError::PhaseRunLoopExceeded {
            phase: Phase::BeforeInference,
            max_rounds: 3,
        }
    ));
}

#[tokio::test]
async fn malformed_action_payloads_are_dead_lettered() {
    let app = AppRuntime::new().unwrap();
    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(BadlyEncodedPlugin)];
    let env = ExecutionEnv::from_plugins(&plugins).unwrap();

    let mut cmd = StateCommand::new();
    cmd.schedule_action::<BadlyEncodedAction>("broken".into())
        .unwrap();
    app.submit_command(&env, cmd).await.unwrap();

    let report = app.run_phase(&env, Phase::BeforeInference).await.unwrap();
    assert_eq!(report.failed_scheduled_actions, 1);
    let failed = app
        .store()
        .read::<FailedScheduledActions>()
        .unwrap_or_default();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].action.key, BadlyEncodedAction::KEY);
}

#[tokio::test]
async fn malformed_effect_payloads_are_reported_as_failed_dispatch() {
    let runtime = PhaseRuntime::new(StateStore::new()).unwrap();
    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(MismatchedEffectPlugin)];
    let env = ExecutionEnv::from_plugins(&plugins).unwrap();

    let mut cmd = StateCommand::new();
    cmd.emit::<MismatchedEffect>(MismatchedPayload).unwrap();

    let report = runtime.submit_command(&env, cmd).await.unwrap();
    assert_eq!(report.effect_report.attempted, 1);
    assert_eq!(report.effect_report.dispatched, 0);
    assert_eq!(report.effect_report.failed, 1);
}

// --- Phase hook tests ---

struct CountingHook(Arc<std::sync::atomic::AtomicUsize>);

#[async_trait]
impl PhaseHook for CountingHook {
    async fn run(&self, _ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(StateCommand::new())
    }
}

struct MutatingHook;

#[async_trait]
impl PhaseHook for MutatingHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let mut cmd = StateCommand::new().with_base_revision(ctx.snapshot.revision());
        cmd.update::<HandoffChannel>(HandoffAction::Request {
            agent: "from-hook".into(),
        });
        Ok(cmd)
    }
}

struct ActionEnqueuingHook;

#[async_trait]
impl PhaseHook for ActionEnqueuingHook {
    async fn run(&self, _ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let mut cmd = StateCommand::new();
        cmd.schedule_action::<LogOnlyAction>(()).unwrap();
        Ok(cmd)
    }
}

struct HookPlugin {
    hook_count: Arc<std::sync::atomic::AtomicUsize>,
}

impl Plugin for HookPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "hook-plugin",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_phase_hook(
            "hook-plugin",
            Phase::BeforeInference,
            CountingHook(Arc::clone(&self.hook_count)),
        )?;
        Ok(())
    }
}

#[tokio::test]
async fn phase_hook_runs_during_run_phase() {
    let app = AppRuntime::new().unwrap();
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hook_plugin = Arc::new(HookPlugin {
        hook_count: Arc::clone(&count),
    });
    let plugins: Vec<Arc<dyn Plugin>> = vec![hook_plugin];
    let env = ExecutionEnv::from_plugins(&plugins).unwrap();

    app.run_phase(&env, Phase::BeforeInference).await.unwrap();
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn phase_hook_can_mutate_state() {
    let app = AppRuntime::new().unwrap();
    app.store().install_plugin(HandoffPlugin).unwrap();

    struct MutatingHookPlugin;
    impl Plugin for MutatingHookPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "mutating-hook-plugin",
            }
        }
        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
            registrar.register_phase_hook(
                "mutating-hook-plugin",
                Phase::BeforeInference,
                MutatingHook,
            )?;
            Ok(())
        }
    }

    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(HandoffPlugin), Arc::new(MutatingHookPlugin)];
    let env = ExecutionEnv::from_plugins(&plugins).unwrap();

    app.run_phase(&env, Phase::BeforeInference).await.unwrap();

    let state = app.store().read::<HandoffChannel>().unwrap();
    assert_eq!(state.requested_agent.as_deref(), Some("from-hook"));
}

#[tokio::test]
async fn phase_hook_can_enqueue_actions() {
    let app = AppRuntime::new().unwrap();

    struct EnqueuePlugin;
    impl Plugin for EnqueuePlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "enqueue-plugin",
            }
        }
        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
            registrar.register_phase_hook(
                "enqueue-plugin",
                Phase::BeforeInference,
                ActionEnqueuingHook,
            )?;
            registrar.register_scheduled_action::<LogOnlyAction, _>(LogOnlyHandler)?;
            Ok(())
        }
    }

    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(EnqueuePlugin)];
    let env = ExecutionEnv::from_plugins(&plugins).unwrap();

    let report = app.run_phase(&env, Phase::BeforeInference).await.unwrap();
    assert_eq!(report.processed_scheduled_actions, 1);
    assert_eq!(
        app.store()
            .read::<PendingScheduledActions>()
            .unwrap_or_default()
            .len(),
        0
    );
}

#[tokio::test]
async fn phase_hooks_execute_in_registration_order() {
    let order = Arc::new(Mutex::new(Vec::<&str>::new()));

    struct OrderHook {
        label: &'static str,
        order: Arc<Mutex<Vec<&'static str>>>,
    }
    #[async_trait]
    impl PhaseHook for OrderHook {
        async fn run(&self, _ctx: &PhaseContext) -> Result<StateCommand, StateError> {
            self.order.lock().unwrap().push(self.label);
            Ok(StateCommand::new())
        }
    }

    let order_clone = Arc::clone(&order);
    struct OrderPlugin {
        order: Arc<Mutex<Vec<&'static str>>>,
    }
    impl Plugin for OrderPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "order-plugin",
            }
        }
        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
            registrar.register_phase_hook(
                "order-plugin",
                Phase::BeforeInference,
                OrderHook {
                    label: "first",
                    order: Arc::clone(&self.order),
                },
            )?;
            registrar.register_phase_hook(
                "order-plugin",
                Phase::BeforeInference,
                OrderHook {
                    label: "second",
                    order: Arc::clone(&self.order),
                },
            )?;
            Ok(())
        }
    }

    let app = AppRuntime::new().unwrap();
    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(OrderPlugin { order: order_clone })];
    let env = ExecutionEnv::from_plugins(&plugins).unwrap();

    app.run_phase(&env, Phase::BeforeInference).await.unwrap();
    assert_eq!(*order.lock().unwrap(), vec!["first", "second"]);
}

#[tokio::test]
async fn phase_hooks_are_cleaned_up_on_uninstall() {
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // First run: with the hook plugin
    let app = AppRuntime::new().unwrap();
    let hook_plugin = Arc::new(HookPlugin {
        hook_count: Arc::clone(&count),
    });
    let env_with_hook = ExecutionEnv::from_plugins(&[hook_plugin as Arc<dyn Plugin>]).unwrap();

    app.run_phase(&env_with_hook, Phase::BeforeInference)
        .await
        .unwrap();
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Second run: without the hook plugin (simulates uninstall)
    let env_without_hook = ExecutionEnv::empty();

    app.run_phase(&env_without_hook, Phase::BeforeInference)
        .await
        .unwrap();
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn phase_hook_does_not_fire_for_other_phases() {
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let app = AppRuntime::new().unwrap();
    let hook_plugin = Arc::new(HookPlugin {
        hook_count: Arc::clone(&count),
    });
    let env = ExecutionEnv::from_plugins(&[hook_plugin as Arc<dyn Plugin>]).unwrap();

    app.run_phase(&env, Phase::AfterInference).await.unwrap();
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn phase_hooks_fire_for_step_start_and_step_end() {
    let step_start_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let step_end_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    struct StepHookPlugin {
        start_count: Arc<std::sync::atomic::AtomicUsize>,
        end_count: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Plugin for StepHookPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "step-hook-plugin",
            }
        }

        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
            registrar.register_phase_hook(
                "step-hook-plugin",
                Phase::StepStart,
                CountingHook(Arc::clone(&self.start_count)),
            )?;
            registrar.register_phase_hook(
                "step-hook-plugin",
                Phase::StepEnd,
                CountingHook(Arc::clone(&self.end_count)),
            )?;
            Ok(())
        }
    }

    let app = AppRuntime::new().unwrap();
    let step_plugin = Arc::new(StepHookPlugin {
        start_count: Arc::clone(&step_start_count),
        end_count: Arc::clone(&step_end_count),
    });
    let env = ExecutionEnv::from_plugins(&[step_plugin as Arc<dyn Plugin>]).unwrap();

    app.run_phase(&env, Phase::StepStart).await.unwrap();
    app.run_phase(&env, Phase::StepEnd).await.unwrap();

    assert_eq!(
        step_start_count.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(step_end_count.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn phase_hooks_do_not_cross_fire_between_step_phases() {
    let start_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    struct StepStartOnlyPlugin {
        count: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Plugin for StepStartOnlyPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "step-start-only",
            }
        }

        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
            registrar.register_phase_hook(
                "step-start-only",
                Phase::StepStart,
                CountingHook(Arc::clone(&self.count)),
            )?;
            Ok(())
        }
    }

    let app = AppRuntime::new().unwrap();
    let step_plugin = Arc::new(StepStartOnlyPlugin {
        count: Arc::clone(&start_count),
    });
    let env = ExecutionEnv::from_plugins(&[step_plugin as Arc<dyn Plugin>]).unwrap();

    // StepEnd should NOT trigger StepStart hook
    app.run_phase(&env, Phase::StepEnd).await.unwrap();
    assert_eq!(start_count.load(std::sync::atomic::Ordering::SeqCst), 0);

    // StepStart should trigger it
    app.run_phase(&env, Phase::StepStart).await.unwrap();
    assert_eq!(start_count.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn all_eight_phases_can_run_without_hooks() {
    let app = AppRuntime::new().unwrap();
    let env = ExecutionEnv::empty();
    for phase in Phase::ALL {
        let report = app.run_phase(&env, phase).await.unwrap();
        assert_eq!(report.phase, phase);
        assert_eq!(report.processed_scheduled_actions, 0);
    }
}
