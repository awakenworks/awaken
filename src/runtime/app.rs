use crate::foundation::{Phase, Snapshot, StateError};
use crate::patch::StateCommand;
use crate::store::StateStore;

use super::engine::PhaseRuntime;
use super::registry::RuntimePlugin;
use super::reports::{PhaseRunReport, SubmitCommandReport};

#[derive(Clone)]
pub struct AppRuntime {
    store: StateStore,
    phase_runtime: PhaseRuntime,
}

impl AppRuntime {
    pub fn new() -> Result<Self, StateError> {
        let store = StateStore::new();
        let phase_runtime = PhaseRuntime::new(store.clone())?;
        Ok(Self {
            store,
            phase_runtime,
        })
    }

    pub fn store(&self) -> &StateStore {
        &self.store
    }

    pub fn phase_runtime(&self) -> &PhaseRuntime {
        &self.phase_runtime
    }

    pub fn snapshot(&self) -> Snapshot {
        self.store.snapshot()
    }

    pub fn revision(&self) -> u64 {
        self.store.revision()
    }

    pub fn submit_command(&self, command: StateCommand) -> Result<SubmitCommandReport, StateError> {
        self.phase_runtime.submit_command(command)
    }

    pub fn run_phase(&self, phase: Phase) -> Result<PhaseRunReport, StateError> {
        self.phase_runtime.run_phase(phase)
    }

    pub fn run_phase_with_limit(
        &self,
        phase: Phase,
        max_rounds: usize,
    ) -> Result<PhaseRunReport, StateError> {
        self.phase_runtime.run_phase_with_limit(phase, max_rounds)
    }

    pub fn trim_logs(&self, keep: usize) -> Result<(), StateError> {
        self.phase_runtime.trim_logs(keep)
    }

    pub fn clear_logs(&self) -> Result<(), StateError> {
        self.phase_runtime.clear_logs()
    }

    pub fn install_plugin<P>(&self, plugin: P) -> Result<(), StateError>
    where
        P: RuntimePlugin,
    {
        self.phase_runtime.install_plugin(plugin)
    }

    pub fn uninstall_plugin<P>(&self) -> Result<(), StateError>
    where
        P: RuntimePlugin,
    {
        self.phase_runtime.uninstall_plugin::<P>()
    }
}
