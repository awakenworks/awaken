//! Pure state-machine evaluation.
//!
//! - [`gate_evaluate`] runs **before** execution: the result is unknown, so it
//!   only checks the precondition (pattern + `from` state) and yields the gate
//!   verdict (deny / ask / warn).
//! - [`advance_evaluate`] runs **after** execution: the result is known, so it
//!   selects the firing transition by `when` (with catch-all and machine-level
//!   `on_unmatched` fallback) and yields the state change plus any message.

use awaken_agent_contract::agent::message::Role;
use serde_json::Value;

use crate::machine::{Emit, EmitTarget, Machine, MachineScope, Transition, ViolationAction};
use crate::result::ToolResultView;
use crate::state::FsmStore;

fn store_for<'a>(scope: MachineScope, thread: &'a FsmStore, run: &'a FsmStore) -> &'a FsmStore {
    match scope {
        MachineScope::Thread => thread,
        MachineScope::Run => run,
    }
}

fn current_state<'a>(machine: &'a Machine, store: &'a FsmStore, key: &str) -> &'a str {
    store
        .current(&machine.name, key)
        .unwrap_or(&machine.initial)
}

fn default_reason(machine: &str, tool_name: &str, current: &str) -> String {
    format!(
        "Tool `{tool_name}` is not permitted by state machine `{machine}` from state `{current}`."
    )
}

// ---------------------------------------------------------------------------
// Gate evaluation (pre-execution)
// ---------------------------------------------------------------------------

/// The gate outcome of evaluating one machine against a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachineEval {
    Allow {
        scope: MachineScope,
        machine: String,
        key: String,
        to: String,
    },
    Violation {
        action: ViolationAction,
        machine: String,
        key: String,
        reason: String,
    },
}

/// A gate-level decision: the strongest blocking violation (`Deny` > `Ask`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateViolation {
    pub action: ViolationAction,
    pub machine: String,
    pub key: String,
    pub reason: String,
}

/// Evaluate every machine against a tool call **before** execution.
#[must_use]
pub fn gate_evaluate(
    machines: &[Machine],
    thread: &FsmStore,
    run: &FsmStore,
    tool_name: &str,
    tool_args: &Value,
) -> Vec<MachineEval> {
    let mut out = Vec::new();
    for machine in machines {
        let Some(key) = machine.render_key(tool_args) else {
            continue;
        };
        let store = store_for(machine.scope, thread, run);
        let current = current_state(machine, store, &key);

        let mut matched_any = false;
        let mut allowed: Option<&str> = None;
        let mut first_violation: Option<&Transition> = None;

        for t in machine.matching_transitions(tool_name, tool_args) {
            matched_any = true;
            if t.allows_from(current) {
                allowed = Some(&t.to);
                break;
            } else if first_violation.is_none() {
                first_violation = Some(t);
            }
        }

        if let Some(to) = allowed {
            out.push(MachineEval::Allow {
                scope: machine.scope,
                machine: machine.name.clone(),
                key,
                to: to.to_string(),
            });
        } else if let Some(t) = first_violation {
            let reason = t
                .on_violation
                .reason
                .as_ref()
                .map(|r| r.render_lossy(tool_args))
                .unwrap_or_else(|| default_reason(&machine.name, tool_name, current));
            out.push(MachineEval::Violation {
                action: t.on_violation.action,
                machine: machine.name.clone(),
                key,
                reason,
            });
        } else if machine.strict && !matched_any {
            out.push(MachineEval::Violation {
                action: ViolationAction::Deny,
                machine: machine.name.clone(),
                key,
                reason: default_reason(&machine.name, tool_name, current),
            });
        }
    }
    out
}

/// Reduce gate evaluations to the strongest blocking decision, if any.
#[must_use]
pub fn gate_decision(evals: &[MachineEval]) -> Option<GateViolation> {
    fn rank(a: ViolationAction) -> u8 {
        match a {
            ViolationAction::Deny => 2,
            ViolationAction::Ask => 1,
            ViolationAction::Warn => 0,
        }
    }
    let mut best: Option<GateViolation> = None;
    for e in evals {
        let MachineEval::Violation {
            action,
            machine,
            key,
            reason,
        } = e
        else {
            continue;
        };
        if rank(*action) == 0 {
            continue;
        }
        let better = best
            .as_ref()
            .map(|b| rank(*action) > rank(b.action))
            .unwrap_or(true);
        if better {
            best = Some(GateViolation {
                action: *action,
                machine: machine.clone(),
                key: key.clone(),
                reason: reason.clone(),
            });
        }
    }
    best
}

// ---------------------------------------------------------------------------
// Advance evaluation (post-execution)
// ---------------------------------------------------------------------------

/// A state change or message injection to apply after a tool executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdvanceOp {
    Transition {
        scope: MachineScope,
        machine: String,
        key: String,
        to: String,
    },
    Emit {
        machine: String,
        key: String,
        reason: EmitReason,
        target: EmitTarget,
        content: String,
        cooldown_turns: u32,
        role: Option<Role>,
    },
}

/// Why a context message was emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmitReason {
    Transition,
    Warning,
}

/// Evaluate every machine against a tool call **after** execution.
#[must_use]
pub fn advance_evaluate(
    machines: &[Machine],
    thread: &FsmStore,
    run: &FsmStore,
    tool_name: &str,
    tool_args: &Value,
    result: &ToolResultView<'_>,
) -> Vec<AdvanceOp> {
    let mut out = Vec::new();
    for machine in machines {
        let Some(key) = machine.render_key(tool_args) else {
            continue;
        };
        let store = store_for(machine.scope, thread, run);
        let current = current_state(machine, store, &key);

        let from_ok: Vec<&Transition> = machine
            .matching_transitions(tool_name, tool_args)
            .filter(|t| t.allows_from(current))
            .collect();

        if !from_ok.is_empty() {
            let fired = from_ok.iter().copied().find(|t| t.result_matches(result));
            match fired {
                Some(t) => {
                    if t.to != current {
                        out.push(AdvanceOp::Transition {
                            scope: machine.scope,
                            machine: machine.name.clone(),
                            key: key.clone(),
                            to: t.to.clone(),
                        });
                    }
                    if let Some(emit) = &t.emit {
                        out.push(emit_op(&machine.name, &key, emit, tool_args));
                    }
                }
                None => {
                    if let Some(to) = &machine.on_unmatched
                        && to.as_str() != current
                    {
                        out.push(AdvanceOp::Transition {
                            scope: machine.scope,
                            machine: machine.name.clone(),
                            key,
                            to: to.clone(),
                        });
                    }
                }
            }
        } else if let Some(t) = machine
            .matching_transitions(tool_name, tool_args)
            .find(|t| !t.allows_from(current))
            && t.on_violation.action == ViolationAction::Warn
        {
            let reason = t
                .on_violation
                .reason
                .as_ref()
                .map(|r| r.render_lossy(tool_args))
                .unwrap_or_else(|| default_reason(&machine.name, tool_name, current));
            out.push(AdvanceOp::Emit {
                machine: machine.name.clone(),
                key,
                reason: EmitReason::Warning,
                target: EmitTarget::SuffixSystem,
                content: reason,
                cooldown_turns: 0,
                role: None,
            });
        }
    }
    out
}

fn emit_op(machine: &str, key: &str, emit: &Emit, tool_args: &Value) -> AdvanceOp {
    AdvanceOp::Emit {
        machine: machine.to_string(),
        key: key.to_string(),
        reason: EmitReason::Transition,
        target: emit.target,
        content: emit.content.render_lossy(tool_args),
        cooldown_turns: emit.cooldown_turns,
        role: emit.role.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StateMachineConfig;
    use crate::state::FsmStore;
    use serde_json::json;

    fn machines(json: &str) -> Vec<Machine> {
        StateMachineConfig::from_json_str(json)
            .unwrap()
            .into_machines()
            .unwrap()
    }

    fn read_before_write() -> Vec<Machine> {
        machines(
            r#"{"machines":[{
            "name":"rbw","scope":"thread","key":"{file_path}","initial":"unread",
            "transitions":[
                {"on":"Read(file_path ~ \"*\")","from":["unread","written","read"],"to":"read"},
                {"on":"Write(file_path ~ \"*\")","from":"read","to":"written",
                 "on_violation":{"action":"deny","reason":"Read {file_path} before writing."}}
            ]}]}"#,
        )
    }

    fn advanced(machine: &str, key: &str, to: &str) -> FsmStore {
        let mut store = FsmStore::default();
        store
            .machines
            .entry(machine.into())
            .or_default()
            .insert(key.into(), to.into());
        store
    }

    fn ok(content: &str) -> ToolResultView<'_> {
        ToolResultView::new(false, content)
    }

    #[test]
    fn write_before_read_is_denied() {
        let (thread, run) = (FsmStore::default(), FsmStore::default());
        let evals = gate_evaluate(
            &read_before_write(),
            &thread,
            &run,
            "Write",
            &json!({"file_path":"a.rs"}),
        );
        let gate = gate_decision(&evals).unwrap();
        assert_eq!(gate.action, ViolationAction::Deny);
        assert_eq!(gate.reason, "Read a.rs before writing.");
    }

    #[test]
    fn write_after_read_is_allowed() {
        let thread = advanced("rbw", "a.rs", "read");
        let run = FsmStore::default();
        let evals = gate_evaluate(
            &read_before_write(),
            &thread,
            &run,
            "Write",
            &json!({"file_path":"a.rs"}),
        );
        assert!(gate_decision(&evals).is_none());
    }

    #[test]
    fn path_normalized_keys_share_instance() {
        let ms = machines(
            r#"{"machines":[{"name":"rbw","scope":"thread","key":"{file_path}",
            "key_normalizer":"path","initial":"unread",
            "transitions":[
                {"on":"Read(file_path ~ \"*\")","from":["unread","read"],"to":"read"},
                {"on":"Write(file_path ~ \"*\")","from":"read","to":"written"}
            ]}]}"#,
        );
        let (thread, run) = (FsmStore::default(), FsmStore::default());
        let ops = advance_evaluate(
            &ms,
            &thread,
            &run,
            "Read",
            &json!({"file_path":"./src/../src/main.rs"}),
            &ok("x"),
        );
        assert_eq!(
            ops,
            vec![AdvanceOp::Transition {
                scope: MachineScope::Thread,
                machine: "rbw".into(),
                key: "src/main.rs".into(),
                to: "read".into(),
            }]
        );
    }

    #[test]
    fn strict_denies_undeclared() {
        let ms = machines(
            r#"{"machines":[{"name":"lock","scope":"run","key":"","initial":"start","strict":true,
            "transitions":[{"on":"Plan","from":"start","to":"planned"}]}]}"#,
        );
        let (thread, run) = (FsmStore::default(), FsmStore::default());
        let evals = gate_evaluate(&ms, &thread, &run, "Edit", &json!({}));
        assert_eq!(gate_decision(&evals).unwrap().action, ViolationAction::Deny);
    }

    #[test]
    fn advance_read_moves_to_read_and_self_loop_noop() {
        let ms = read_before_write();
        let (thread, run) = (FsmStore::default(), FsmStore::default());
        let ops = advance_evaluate(
            &ms,
            &thread,
            &run,
            "Read",
            &json!({"file_path":"a.rs"}),
            &ok("x"),
        );
        assert!(matches!(&ops[0], AdvanceOp::Transition { to, .. } if to == "read"));

        let thread = advanced("rbw", "a.rs", "read");
        let ops = advance_evaluate(
            &ms,
            &thread,
            &run,
            "Read",
            &json!({"file_path":"a.rs"}),
            &ok("x"),
        );
        assert!(ops.is_empty());
    }

    fn test_flow() -> Vec<Machine> {
        machines(
            r#"{"machines":[{
            "name":"test","scope":"run","key":"","initial":"idle","on_unmatched":"flaky",
            "transitions":[
                {"on":"Test","from":["idle","passing","failing","flaky"],"when":{"status":"success"},"to":"passing"},
                {"on":"Test","from":["idle","passing","failing","flaky"],"when":{"status":"error"},"to":"failing"}
            ]}]}"#,
        )
    }

    #[test]
    fn result_success_routes_to_passing() {
        let (thread, run) = (FsmStore::default(), FsmStore::default());
        let ops = advance_evaluate(&test_flow(), &thread, &run, "Test", &json!({}), &ok("ok"));
        assert!(matches!(&ops[0], AdvanceOp::Transition { to, .. } if to == "passing"));
    }

    #[test]
    fn result_error_routes_to_failing() {
        let (thread, run) = (FsmStore::default(), FsmStore::default());
        let err = ToolResultView::new(true, "boom");
        let ops = advance_evaluate(&test_flow(), &thread, &run, "Test", &json!({}), &err);
        assert!(matches!(&ops[0], AdvanceOp::Transition { to, .. } if to == "failing"));
    }

    #[test]
    fn warn_violation_surfaces_at_advance() {
        let ms = machines(
            r#"{"machines":[{
            "name":"m","scope":"run","key":"{file_path}","initial":"unread",
            "transitions":[{"on":"Write(file_path ~ \"*\")","from":"read","to":"written",
                "on_violation":{"action":"warn","reason":"writing unread {file_path}"}}]}]}"#,
        );
        let (thread, run) = (FsmStore::default(), FsmStore::default());
        let ops = advance_evaluate(
            &ms,
            &thread,
            &run,
            "Write",
            &json!({"file_path":"a.rs"}),
            &ok("ok"),
        );
        assert!(matches!(&ops[0], AdvanceOp::Emit { reason, content, .. }
            if *reason == EmitReason::Warning && content == "writing unread a.rs"));
    }

    #[test]
    fn transition_emits_context_message() {
        let ms = machines(
            r#"{"machines":[{
            "name":"m","scope":"run","key":"","initial":"a",
            "transitions":[{"on":"X","from":"a","to":"b",
                "emit":{"target":"suffix_system","content":"moved to b"}}]}]}"#,
        );
        let (thread, run) = (FsmStore::default(), FsmStore::default());
        let ops = advance_evaluate(&ms, &thread, &run, "X", &json!({}), &ok("ok"));
        assert_eq!(ops.len(), 2);
        assert!(matches!(&ops[0], AdvanceOp::Transition { to, .. } if to == "b"));
        assert!(matches!(&ops[1], AdvanceOp::Emit { content, .. } if content == "moved to b"));
    }

    #[test]
    fn missing_key_skips_machine() {
        let ms = read_before_write();
        let (thread, run) = (FsmStore::default(), FsmStore::default());
        assert!(gate_evaluate(&ms, &thread, &run, "Write", &json!({"x":1})).is_empty());
        assert!(
            advance_evaluate(&ms, &thread, &run, "Write", &json!({"x":1}), &ok("ok")).is_empty()
        );
    }
}
