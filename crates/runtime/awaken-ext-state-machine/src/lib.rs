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
    let mut schema = serde_json::to_value(schemars::schema_for!(config::StateMachineConfig))
        .unwrap_or(serde_json::Value::Null);
    // The structural schema alone doesn't teach the DSL that lives INSIDE strings — the
    // `on` tool-pattern grammar, `key` templates, and the `strict` caveat — so an author
    // (human or LLM) guesses them. Attach that guidance + a worked example here so every
    // consumer that reads this schema (the console form AND the assistant, which authors
    // from `config_schema`) gets it. Belongs with the config: it documents this config.
    if let serde_json::Value::Object(map) = &mut schema {
        map.insert("description".into(), serde_json::json!(AUTHORING_GUIDE));
        map.insert(
            "examples".into(),
            serde_json::json!([EXAMPLE_READ_BEFORE_WRITE()]),
        );
    }
    schema
}

/// The DSL rules the raw JSON Schema can't express (they live inside strings).
#[cfg(feature = "schema")]
const AUTHORING_GUIDE: &str = "\
A state machine over tool calls. Authoring rules:\n\
- `on` is a TOOL PATTERN: `<tool_id>` matches that tool, `<tool_id>(<arg> ~ \"<glob>\")` \
also matches on an argument, and `*` matches any tool. The `<tool_id>` and `<arg>` must be \
the EXACT ids the tools use (they are case-sensitive — the built-in file tools are \
`read`/`write` with a `path` argument, NOT `Read`/`Write`/`file_path`).\n\
- `key` is a TEMPLATE: use `{<arg>}` (e.g. `{path}`) to track a separate instance per \
distinct argument value (per-file). A literal string keys ALL calls to one instance.\n\
- `from` may match multiple states (a list). A transition whose tool matches but whose \
`from` doesn't hold in the current state fires its `on_violation` ({action: deny|warn}).\n\
- `emit` injects a system reminder (`target: suffix_system`); `cooldown_turns` throttles it.\n\
- `strict: true` DENIES any tool not matched by a transition — usually leave it false.";

/// A canonical, correct read-before-write machine (the shape authors should mirror).
#[cfg(feature = "schema")]
#[allow(non_snake_case)]
fn EXAMPLE_READ_BEFORE_WRITE() -> serde_json::Value {
    serde_json::json!({
        "machines": [{
            "name": "read-before-write",
            "scope": "thread",
            "key": "{path}",
            "initial": "unread",
            "terminal": ["written"],
            "transitions": [
                { "on": "read(path ~ \"*\")", "from": ["unread", "written", "read"], "to": "read" },
                { "on": "write(path ~ \"*\")", "from": "read", "to": "written",
                  "on_violation": { "action": "deny", "reason": "Read {path} before writing it." } }
            ]
        }]
    })
}
pub use result::{ContentMatcher, ResultMatcher, StatusMatcher, ToolResultView, result_matches};
pub use state::{
    FsmMetricCounts, FsmMetricEvent, FsmMetricUpdate, FsmMetrics, FsmStore, FsmTransition,
    FsmViolationLog, FsmViolationRecord, Metrics, RunInstances, STATE_KEYS, ThreadInstances,
    ViolationAuditAction, ViolationLog,
};
