//! Stateful tool-call ordering constraints for Awaken.
//!
//! A [`StateMachinePlugin`] loads a declarative FSM DSL (JSON/YAML) that
//! constrains the order of tool calls — e.g. "read a file before writing it".
//! It contributes a pre-execution gate (deny/ask/warn), a post-execution
//! outcome hook (advance state + emit reminders), and a run-end continuation
//! guard, all under its declared `CapabilityBound`. See
//! `docs/design/tool-state-machine.md`.

mod config;
mod engine;
mod machine;
mod plugin;
mod result;
mod state;

pub use config::{ContinuationSettings, StateMachineConfig, StateMachineConfigError};
pub use engine::{
    AdvanceOp, EmitReason, GateViolation, MachineEval, advance_evaluate, gate_decision,
    gate_evaluate,
};
pub use machine::{
    Emit, EmitTarget, KeyNormalizer, KeyTemplate, KeyTemplateError, Machine, MachineScope,
    Transition, Violation, ViolationAction,
};
pub use plugin::{STATE_MACHINE_PLUGIN_ID, StateMachinePlugin};

/// The JSON Schema for this plugin's config section, derived from
/// [`StateMachineConfig`]. A config frontend renders and validates the section
/// against it; the authoritative check remains a dry-run `resolve_configured`.
#[cfg(feature = "schema")]
#[must_use]
pub fn config_schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(config::StateMachineConfig))
        .unwrap_or(serde_json::Value::Null)
}
pub use result::{ContentMatcher, ResultMatcher, StatusMatcher, ToolResultView, result_matches};
pub use state::{
    FsmMetricCounts, FsmMetricEvent, FsmMetricUpdate, FsmMetrics, FsmStore, FsmTransition,
    FsmViolationLog, FsmViolationRecord, Metrics, RunInstances, STATE_KEYS, StateCell,
    ThreadInstances, ViolationAuditAction, ViolationLog,
};
