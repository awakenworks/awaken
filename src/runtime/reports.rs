use crate::foundation::Phase;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectDispatchReport {
    pub attempted: usize,
    pub dispatched: usize,
    pub failed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitCommandReport {
    pub revision: u64,
    pub effect_report: EffectDispatchReport,
}

pub const DEFAULT_MAX_PHASE_ROUNDS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseRunReport {
    pub phase: Phase,
    pub rounds: usize,
    pub processed_scheduled_actions: usize,
    pub skipped_scheduled_actions: usize,
    pub failed_scheduled_actions: usize,
    pub generated_effects: usize,
    pub effect_report: EffectDispatchReport,
}
