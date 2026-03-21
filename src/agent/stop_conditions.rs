//! Built-in stop condition plugins.

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use crate::contract::lifecycle::TerminationReason;
use crate::error::StateError;
use crate::model::{Phase, RuntimeEffect};
use crate::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use crate::runtime::{PhaseContext, PhaseHook};
use crate::state::StateCommand;

/// Plugin that terminates the run after a maximum number of steps.
pub struct MaxRoundsPlugin {
    max_rounds: usize,
}

impl MaxRoundsPlugin {
    pub fn new(max_rounds: usize) -> Self {
        Self { max_rounds }
    }
}

impl Plugin for MaxRoundsPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "stop-condition:max-rounds",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_phase_hook(
            "stop-condition:max-rounds",
            Phase::AfterInference,
            MaxRoundsHook {
                max_rounds: self.max_rounds,
                current_step: AtomicUsize::new(0),
            },
        )
    }
}

struct MaxRoundsHook {
    max_rounds: usize,
    current_step: AtomicUsize,
}

#[async_trait]
impl PhaseHook for MaxRoundsHook {
    async fn run(&self, _ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let step = self.current_step.fetch_add(1, Ordering::SeqCst) + 1;
        if step > self.max_rounds {
            let mut cmd = StateCommand::new();
            cmd.effect(RuntimeEffect::Terminate {
                reason: TerminationReason::stopped_with_detail(
                    "max_rounds",
                    format!("exceeded {max} rounds", max = self.max_rounds),
                ),
            })?;
            return Ok(cmd);
        }
        Ok(StateCommand::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{ExecutionEnv, PhaseRuntime, TypedEffectHandler};
    use crate::state::{Snapshot, StateStore};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Recorder {
        effects: Arc<Mutex<Vec<RuntimeEffect>>>,
    }

    impl Plugin for Recorder {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                name: "test-recorder",
            }
        }
        fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
            registrar.register_effect::<RuntimeEffect, _>(RecorderHandler(self.effects.clone()))
        }
    }

    struct RecorderHandler(Arc<Mutex<Vec<RuntimeEffect>>>);

    #[async_trait]
    impl TypedEffectHandler<RuntimeEffect> for RecorderHandler {
        async fn handle_typed(&self, payload: RuntimeEffect, _: &Snapshot) -> Result<(), String> {
            self.0.lock().unwrap().push(payload);
            Ok(())
        }
    }

    #[tokio::test]
    async fn max_rounds_plugin_installs_and_registers_hook() {
        let store = StateStore::new();
        let runtime = PhaseRuntime::new(store).unwrap();
        let recorder = Recorder::default();

        let plugins: Vec<Arc<dyn Plugin>> = vec![
            Arc::new(recorder.clone()),
            Arc::new(MaxRoundsPlugin::new(5)),
        ];
        let env = ExecutionEnv::from_plugins(&plugins).unwrap();

        for _ in 0..5 {
            runtime
                .run_phase(&env, Phase::AfterInference)
                .await
                .unwrap();
        }
        let report = runtime
            .run_phase(&env, Phase::AfterInference)
            .await
            .unwrap();
        assert_eq!(report.effect_report.attempted, 1);
        assert_eq!(report.effect_report.dispatched, 1);
    }

    #[tokio::test]
    async fn max_rounds_plugin_emits_terminate_with_correct_reason() {
        let store = StateStore::new();
        let runtime = PhaseRuntime::new(store).unwrap();
        let recorder = Recorder::default();

        let plugins: Vec<Arc<dyn Plugin>> = vec![
            Arc::new(recorder.clone()),
            Arc::new(MaxRoundsPlugin::new(2)),
        ];
        let env = ExecutionEnv::from_plugins(&plugins).unwrap();

        runtime
            .run_phase(&env, Phase::AfterInference)
            .await
            .unwrap();
        runtime
            .run_phase(&env, Phase::AfterInference)
            .await
            .unwrap();
        assert!(recorder.effects.lock().unwrap().is_empty());

        runtime
            .run_phase(&env, Phase::AfterInference)
            .await
            .unwrap();
        let effects = recorder.effects.lock().unwrap();
        assert_eq!(effects.len(), 1);
        match &effects[0] {
            RuntimeEffect::Terminate { reason } => {
                assert!(matches!(reason, TerminationReason::Stopped(s) if s.code == "max_rounds"));
            }
            other => panic!("expected Terminate, got {other:?}"),
        }
    }
}
