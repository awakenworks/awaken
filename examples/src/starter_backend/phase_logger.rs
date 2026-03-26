//! A demonstration plugin that registers hooks for all 8 phases.
//! Used to verify the phase execution model works end-to-end.

use async_trait::async_trait;

use awaken_contract::StateError;
use awaken_contract::model::Phase;
use awaken_runtime::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use awaken_runtime::state::StateCommand;
use awaken_runtime::{PhaseContext, PhaseHook};

pub struct PhaseLoggerPlugin;

impl Plugin for PhaseLoggerPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "phase_logger",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        for phase in Phase::ALL {
            registrar.register_phase_hook("phase_logger", phase, PhaseLoggerHook { phase })?;
        }
        Ok(())
    }
}

struct PhaseLoggerHook {
    phase: Phase,
}

#[async_trait]
impl PhaseHook for PhaseLoggerHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let agent_id = ctx.agent_spec.id.clone();
        tracing::info!(
            phase = ?self.phase,
            agent = %agent_id,
            "phase hook fired"
        );
        Ok(StateCommand::new())
    }
}
