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
use crate::state::{FsmStore, MachineInstance};

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
        let render = instance_context(
            tool_args.clone(),
            current,
            store.instance(&machine.name, &key),
        );

        let mut matched_any = false;
        let mut allowed: Option<&str> = None;
        let mut strongest_violation: Option<&Transition> = None;

        for t in machine.matching_transitions(tool_name, tool_args) {
            matched_any = true;
            if t.allows_from(current) {
                allowed = Some(&t.to);
                break;
            }
            // Among the non-allowing matches keep the STRONGEST action (Deny >
            // Ask > Warn), not the first one seen: an author ordering a `warn`
            // transition before a `deny` transition for the same tool/state must
            // not weaken enforcement to a non-blocking warn (a within-machine
            // fail-open). This mirrors the cross-machine reduction in
            // `gate_decision`.
            let stronger = strongest_violation.is_none_or(|prev| {
                action_rank(t.on_violation.action) > action_rank(prev.on_violation.action)
            });
            if stronger {
                strongest_violation = Some(t);
            }
        }

        if let Some(to) = allowed {
            out.push(MachineEval::Allow {
                scope: machine.scope,
                machine: machine.name.clone(),
                key,
                to: to.to_string(),
            });
        } else if let Some(t) = strongest_violation {
            let reason = t
                .on_violation
                .reason
                .as_ref()
                .map(|r| r.render_lossy(&render))
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

/// Rank a violation action by severity: `Deny` > `Ask` > `Warn`. The single
/// source of truth for "which violation wins", used both within a machine
/// (strongest non-allowing transition) and across machines (`gate_decision`).
fn action_rank(a: ViolationAction) -> u8 {
    match a {
        ViolationAction::Deny => 2,
        ViolationAction::Ask => 1,
        ViolationAction::Warn => 0,
    }
}

/// Reduce gate evaluations to the strongest blocking decision, if any.
#[must_use]
pub fn gate_decision(evals: &[MachineEval]) -> Option<GateViolation> {
    let rank = action_rank;
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
    Update {
        scope: MachineScope,
        machine: String,
        key: String,
        initial: String,
        data: Vec<(String, String)>,
        increment: Vec<String>,
        reset: Vec<String>,
    },
    Emit {
        machine: String,
        key: String,
        reason: EmitReason,
        target: EmitTarget,
        content: String,
        cooldown_steps: u32,
        role: Option<Role>,
    },
}

/// Generic non-tool event consumed by the same machine aggregate. Event names
/// are configuration vocabulary; the core does not enumerate business events.
#[derive(Debug, Clone, Copy)]
pub struct MachineEventView<'a> {
    pub name: &'a str,
    pub data: &'a Value,
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
        let render = instance_context(
            tool_context(tool_name, tool_args, result),
            current,
            store.instance(&machine.name, &key),
        );

        let from_ok: Vec<&Transition> = machine
            .matching_transitions(tool_name, tool_args)
            .filter(|t| {
                t.allows_from(current) && counters_match(t, store.instance(&machine.name, &key))
            })
            .collect();

        if !from_ok.is_empty() {
            let fired = from_ok.iter().copied().find(|t| t.result_matches(result));
            match fired {
                Some(t) => {
                    if t.to != current || store.instance(&machine.name, &key).is_none() {
                        out.push(AdvanceOp::Transition {
                            scope: machine.scope,
                            machine: machine.name.clone(),
                            key: key.clone(),
                            to: t.to.clone(),
                        });
                    }
                    push_update(&mut out, machine, &key, t, &render);
                    if let Some(emit) = &t.emit {
                        out.push(emit_op(&machine.name, &key, emit, &render));
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
                .map(|r| r.render_lossy(&render))
                .unwrap_or_else(|| default_reason(&machine.name, tool_name, current));
            out.push(AdvanceOp::Emit {
                machine: machine.name.clone(),
                key,
                reason: EmitReason::Warning,
                target: EmitTarget::SuffixSystem,
                content: reason,
                cooldown_steps: 0,
                role: None,
            });
        }
    }
    out
}

/// Evaluate a generic runtime event. Unlike a tool transition this is a single
/// atomic phase: matching, state/data/counter updates, then optional reminder.
#[must_use]
pub fn event_evaluate(
    machines: &[Machine],
    thread: &FsmStore,
    run: &FsmStore,
    event: MachineEventView<'_>,
) -> Vec<AdvanceOp> {
    let mut out = Vec::new();
    for machine in machines {
        let event_render = event_context(event);
        let Some(key) = machine.render_key(&event_render) else {
            continue;
        };
        let store = store_for(machine.scope, thread, run);
        let current = current_state(machine, store, &key);
        let render = instance_context(event_render, current, store.instance(&machine.name, &key));
        let Some(transition) = machine
            .matching_event_transitions(event.name, event.data)
            .find(|transition| {
                transition.allows_from(current)
                    && counters_match(transition, store.instance(&machine.name, &key))
            })
        else {
            continue;
        };

        if transition.to != current || store.instance(&machine.name, &key).is_none() {
            out.push(AdvanceOp::Transition {
                scope: machine.scope,
                machine: machine.name.clone(),
                key: key.clone(),
                to: transition.to.clone(),
            });
        }
        push_update(&mut out, machine, &key, transition, &render);
        if let Some(emit) = &transition.emit {
            out.push(emit_op(&machine.name, &key, emit, &render));
        }
    }
    out
}

fn counters_match(transition: &Transition, instance: Option<&MachineInstance>) -> bool {
    transition.counters.iter().all(|(name, condition)| {
        let value = instance
            .and_then(|instance| instance.counters.get(name))
            .copied()
            .unwrap_or_default();
        condition.matches(value)
    })
}

fn push_update(
    out: &mut Vec<AdvanceOp>,
    machine: &Machine,
    key: &str,
    transition: &Transition,
    context: &Value,
) {
    if transition.update.capture.is_empty()
        && transition.update.increment.is_empty()
        && transition.update.reset.is_empty()
    {
        return;
    }
    let data = transition
        .update
        .capture
        .iter()
        .map(|(name, template)| (name.clone(), template.render_lossy(context)))
        .collect();
    out.push(AdvanceOp::Update {
        scope: machine.scope,
        machine: machine.name.clone(),
        key: key.to_string(),
        initial: machine.initial.clone(),
        data,
        increment: transition.update.increment.clone(),
        reset: transition.update.reset.clone(),
    });
}

fn event_context(event: MachineEventView<'_>) -> Value {
    let mut context = match event.data {
        Value::Object(map) => map.clone(),
        value => serde_json::Map::from_iter([("value".to_string(), value.clone())]),
    };
    context.insert(
        "event".to_string(),
        serde_json::json!({ "name": event.name, "data": event.data }),
    );
    Value::Object(context)
}

fn instance_context(
    mut context: Value,
    current: &str,
    instance: Option<&MachineInstance>,
) -> Value {
    let Value::Object(ref mut map) = context else {
        return context;
    };
    map.insert(
        "instance".to_string(),
        serde_json::json!({
            "state": current,
            "data": instance.map(|value| &value.data).cloned().unwrap_or_default(),
            "counters": instance
                .map(|value| &value.counters)
                .cloned()
                .unwrap_or_default(),
        }),
    );
    context
}

fn tool_context(tool_name: &str, tool_args: &Value, result: &ToolResultView<'_>) -> Value {
    let mut context = match tool_args {
        Value::Object(map) => map.clone(),
        value => serde_json::Map::from_iter([("arguments".to_string(), value.clone())]),
    };
    let result_content = serde_json::from_str::<Value>(result.content)
        .unwrap_or_else(|_| Value::String(result.content.to_string()));
    context.insert(
        "event".to_string(),
        serde_json::json!({
            "name": "tool.result",
            "data": {
                "tool": tool_name,
                "arguments": tool_args,
                "is_error": result.is_error,
                "content": result_content,
            }
        }),
    );
    Value::Object(context)
}

fn emit_op(machine: &str, key: &str, emit: &Emit, tool_args: &Value) -> AdvanceOp {
    AdvanceOp::Emit {
        machine: machine.to_string(),
        key: key.to_string(),
        reason: EmitReason::Transition,
        target: emit.target,
        content: emit.content.render_lossy(tool_args),
        cooldown_steps: emit.cooldown_steps,
        role: emit.role,
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
                {"on":"Write(file_path ~ \"*\")","from":["read","written"],"to":"written",
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

    // REGRESSION (within-machine fail-open): two transitions match the same tool
    // from a state that allows neither — the first is a `warn` violation, the
    // second a `deny`. The gate must surface the STRONGEST action (Deny), not the
    // first-listed (Warn); otherwise reordering rules silently disables the deny.
    #[test]
    fn a_warn_transition_before_a_deny_does_not_weaken_enforcement() {
        let ms = machines(
            r#"{"machines":[{
                "name":"ord","scope":"thread","key":"{file_path}","initial":"s0",
                "transitions":[
                    {"on":"Write(file_path ~ \"*\")","from":["other"],"to":"x",
                     "on_violation":{"action":"warn","reason":"soft"}},
                    {"on":"Write(file_path ~ \"*\")","from":["other"],"to":"y",
                     "on_violation":{"action":"deny","reason":"hard"}}
                ]}]}"#,
        );
        let (thread, run) = (FsmStore::default(), FsmStore::default());
        let evals = gate_evaluate(&ms, &thread, &run, "Write", &json!({"file_path":"a.rs"}));
        let gate = gate_decision(&evals).expect("the deny must still block");
        assert_eq!(gate.action, ViolationAction::Deny);
        assert_eq!(gate.reason, "hard");
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
    fn repeated_write_after_one_read_is_allowed() {
        let thread = advanced("rbw", "a.rs", "written");
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
    fn advance_read_moves_to_read_and_self_loop_preserves_materialized_instance() {
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
        assert!(ops.is_empty(), "an existing no-update self-loop is a no-op");
    }

    #[test]
    fn event_transition_materializes_updates_and_emits_after_counter_threshold() {
        let ms = machines(
            r#"{"machines":[{"name":"todo","scope":"thread","key":"","initial":"tracking",
                "transitions":[
                  {"on":{"event":"step.after_inference"},"from":"tracking","to":"tracking",
                   "update":{"increment":["steps"]}},
                  {"on":{"event":"step.before_inference"},"from":"tracking","to":"tracking",
                   "counters":{"steps":{"gte":1}},
                   "emit":{"target":"context","content":"review todos at {step}"},
                   "update":{"reset":["steps"]}}
                ]}]}"#,
        );
        let mut thread = FsmStore::default();
        let run = FsmStore::default();
        let after = json!({"step": 0});
        let ops = event_evaluate(
            &ms,
            &thread,
            &run,
            MachineEventView {
                name: "step.after_inference",
                data: &after,
            },
        );
        assert!(matches!(ops[0], AdvanceOp::Transition { .. }));
        for op in ops {
            match op {
                AdvanceOp::Transition {
                    machine, key, to, ..
                } => {
                    thread.mutate(crate::state::InstanceMutation {
                        machine,
                        key,
                        initial: "tracking".into(),
                        to: Some(to),
                        data: vec![],
                        increment: vec![],
                        reset: vec![],
                    });
                }
                AdvanceOp::Update {
                    machine,
                    key,
                    initial,
                    data,
                    increment,
                    reset,
                    ..
                } => thread.mutate(crate::state::InstanceMutation {
                    machine,
                    key,
                    initial,
                    to: None,
                    data,
                    increment,
                    reset,
                }),
                AdvanceOp::Emit { .. } => {}
            }
        }
        assert_eq!(thread.instance("todo", "").unwrap().counters["steps"], 1);

        let before = json!({"step": 1});
        let ops = event_evaluate(
            &ms,
            &thread,
            &run,
            MachineEventView {
                name: "step.before_inference",
                data: &before,
            },
        );
        assert!(ops.iter().any(|op| matches!(
            op,
            AdvanceOp::Emit { target: EmitTarget::Context, content, .. }
                if content == "review todos at 1"
        )));
    }

    #[test]
    fn captured_data_is_available_to_later_transition_templates() {
        let ms = machines(
            r#"{"machines":[{"name":"facts","scope":"thread","key":"","initial":"tracking",
                "transitions":[
                  {"on":{"event":"todo.changed"},"from":"tracking","to":"tracking",
                   "update":{"capture":{"snapshot":"{event.data.snapshot}"}}},
                  {"on":{"event":"step.before_inference"},"from":"tracking","to":"tracking",
                   "emit":{"target":"context","content":"TODO: {instance.data.snapshot}"}}
                ]}]}"#,
        );
        let mut thread = FsmStore::default();
        let run = FsmStore::default();
        let changed = json!({"snapshot": "ship backend"});
        for op in event_evaluate(
            &ms,
            &thread,
            &run,
            MachineEventView {
                name: "todo.changed",
                data: &changed,
            },
        ) {
            match op {
                AdvanceOp::Transition {
                    machine, key, to, ..
                } => thread.mutate(crate::state::InstanceMutation {
                    machine,
                    key,
                    initial: "tracking".into(),
                    to: Some(to),
                    data: vec![],
                    increment: vec![],
                    reset: vec![],
                }),
                AdvanceOp::Update {
                    machine,
                    key,
                    initial,
                    data,
                    increment,
                    reset,
                    ..
                } => thread.mutate(crate::state::InstanceMutation {
                    machine,
                    key,
                    initial,
                    to: None,
                    data,
                    increment,
                    reset,
                }),
                AdvanceOp::Emit { .. } => {}
            }
        }

        let before = json!({"step": 1});
        let ops = event_evaluate(
            &ms,
            &thread,
            &run,
            MachineEventView {
                name: "step.before_inference",
                data: &before,
            },
        );
        assert!(ops.iter().any(|op| matches!(
            op,
            AdvanceOp::Emit { content, .. } if content == "TODO: ship backend"
        )));
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

    // --- Gate decision precedence across machines (Deny > Ask > Warn) ---

    #[test]
    fn gate_deny_outranks_ask_regardless_of_machine_order() {
        // Two keyless machines both violate the same `Write`: one denies, one asks.
        // The gate decision must resolve to the strongest block (Deny) no matter
        // which machine is evaluated first.
        let deny_first = machines(
            r#"{"machines":[
                {"name":"d","key":"","initial":"s","transitions":[{"on":"Write","from":"other","to":"x","on_violation":"deny"}]},
                {"name":"a","key":"","initial":"s","transitions":[{"on":"Write","from":"other","to":"x","on_violation":"ask"}]}
            ]}"#,
        );
        let ask_first = machines(
            r#"{"machines":[
                {"name":"a","key":"","initial":"s","transitions":[{"on":"Write","from":"other","to":"x","on_violation":"ask"}]},
                {"name":"d","key":"","initial":"s","transitions":[{"on":"Write","from":"other","to":"x","on_violation":"deny"}]}
            ]}"#,
        );
        let (thread, run) = (FsmStore::default(), FsmStore::default());
        for ms in [deny_first, ask_first] {
            let evals = gate_evaluate(&ms, &thread, &run, "Write", &json!({}));
            assert_eq!(evals.len(), 2, "both machines produce a violation");
            assert_eq!(gate_decision(&evals).unwrap().action, ViolationAction::Deny);
        }
    }

    #[test]
    fn gate_warn_only_violation_does_not_block() {
        // A lone `warn` violation is not a blocking decision: `gate_decision`
        // yields `None` (the warn surfaces later, at advance).
        let ms = machines(
            r#"{"machines":[{"name":"w","key":"","initial":"s",
                "transitions":[{"on":"Write","from":"other","to":"x","on_violation":"warn"}]}]}"#,
        );
        let (thread, run) = (FsmStore::default(), FsmStore::default());
        let evals = gate_evaluate(&ms, &thread, &run, "Write", &json!({}));
        assert!(matches!(
            evals.as_slice(),
            [MachineEval::Violation {
                action: ViolationAction::Warn,
                ..
            }]
        ));
        assert!(gate_decision(&evals).is_none());
    }

    #[test]
    fn gate_non_strict_unmatched_tool_produces_no_eval() {
        // The key renders (empty template ⇒ `""`), but no transition matches the
        // tool and the machine is not `strict`: no eval is produced (allow). This
        // is distinct from the keyless case where the machine is skipped outright.
        let ms = machines(
            r#"{"machines":[{"name":"m","key":"","initial":"s",
                "transitions":[{"on":"Read","from":"s","to":"r"}]}]}"#,
        );
        let (thread, run) = (FsmStore::default(), FsmStore::default());
        assert!(gate_evaluate(&ms, &thread, &run, "Write", &json!({})).is_empty());
    }

    // --- Advance: on_unmatched fallback when no `when` fires ---

    #[test]
    fn advance_on_unmatched_fires_when_no_result_condition_matches() {
        // A matching, from-allowed transition whose `when` does not match the
        // result routes the instance to the machine-level `on_unmatched` state.
        let ms = machines(
            r#"{"machines":[{"name":"t","scope":"run","key":"","initial":"idle","on_unmatched":"flaky",
                "transitions":[{"on":"Test","from":["idle"],"when":{"content":"*ok*"},"to":"passing"}]}]}"#,
        );
        let (thread, run) = (FsmStore::default(), FsmStore::default());
        // Success, but the content condition ("*ok*") does not match "boom".
        let ops = advance_evaluate(&ms, &thread, &run, "Test", &json!({}), &ok("boom"));
        assert_eq!(
            ops,
            vec![AdvanceOp::Transition {
                scope: MachineScope::Run,
                machine: "t".into(),
                key: String::new(),
                to: "flaky".into(),
            }]
        );
    }

    #[test]
    fn advance_stays_when_no_result_matches_and_no_on_unmatched() {
        // No `on_unmatched`: when the result matches no from-allowed transition,
        // the instance stays put and no op is emitted.
        let ms = machines(
            r#"{"machines":[{"name":"t","scope":"run","key":"","initial":"idle",
                "transitions":[{"on":"Test","from":["idle"],"when":{"status":"error"},"to":"failed"}]}]}"#,
        );
        let (thread, run) = (FsmStore::default(), FsmStore::default());
        // Success result, but the only transition fires on error ⇒ nothing.
        let ops = advance_evaluate(&ms, &thread, &run, "Test", &json!({}), &ok("done"));
        assert!(ops.is_empty());
    }
}
