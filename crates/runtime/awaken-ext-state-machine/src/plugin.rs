//! Runtime integration: the `Plugin` that contributes the state machine's gate,
//! `AfterTool` phase hook, run-end guard, and state keys under its
//! `CapabilityBound`.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::state::{FoldStateKey, StateKey, Store};
use awaken_runtime_contract::permission::{GateOutcome, ToolCall, ToolGateHook};
use awaken_runtime_contract::plugin::{
    CapabilityBound, ContextMessages, Contributions, HookReaction, IdBound, PhaseContext,
    PhaseHook, PhaseHookPoint, PhaseKind, Plugin, PluginConfigError, PluginManifest, RunEndContext,
    RunEndDecision, RunEndGuard,
};
use serde_json::{Value, json};

use crate::config::{ContinuationSettings, StateMachineConfig, StateMachineConfigError};
use crate::engine::{
    AdvanceOp, EmitReason, MachineEventView, advance_evaluate, event_evaluate, gate_decision,
    gate_evaluate,
};
use crate::machine::{EmitTarget, Machine, ViolationAction};
use crate::result::ToolResultView;
use crate::state::{
    EmitThrottle, EmitThrottleCell, FsmMetricEvent, FsmMetricUpdate, FsmStore, FsmTransition,
    FsmViolationRecord, InstanceMutation, Metrics, RunInstances, STATE_KEYS, ThreadInstances,
    ViolationAuditAction, ViolationLog,
};

/// The plugin id and the id of every seam it contributes.
pub const STATE_MACHINE_PLUGIN_ID: &str = "state_machine";

/// The state-machine plugin. Machines are compiled once at construction; the
/// contributed seams share the compiled set.
pub struct StateMachinePlugin {
    machines: Arc<[Machine]>,
    continuation: ContinuationSettings,
}

impl StateMachinePlugin {
    /// A plugin with no base machines — driven entirely by the agent's config
    /// section. Register this once; each agent supplies its machines via
    /// `plugin_config["state_machine"]`.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            machines: Vec::new().into(),
            continuation: ContinuationSettings::default(),
        }
    }

    /// Compile a plugin from configuration, validating patterns and templates up
    /// front (fail-fast at construction). The machines become the base set that
    /// every run carries, merged with any per-agent config section.
    pub fn from_config(config: StateMachineConfig) -> Result<Self, StateMachineConfigError> {
        let continuation = config.continuation.clone();
        let machines = config.into_machines()?;
        Ok(Self {
            machines: machines.into(),
            continuation,
        })
    }

    /// Build directly from a compiled machine set.
    #[must_use]
    pub fn new(machines: Vec<Machine>, continuation: ContinuationSettings) -> Self {
        Self {
            machines: machines.into(),
            continuation,
        }
    }

    /// Build the contributions for a resolved machine set.
    fn contribute(
        &self,
        machines: Arc<[Machine]>,
        continuation: ContinuationSettings,
    ) -> Contributions {
        let mut contributions = Contributions::new(STATE_MACHINE_PLUGIN_ID);
        for key in STATE_KEYS {
            contributions.declare_state_key(*key);
        }
        contributions.register_gate(Arc::new(StateMachineGate {
            machines: Arc::clone(&machines),
        }));
        contributions.register_hook(Arc::new(StateMachineObserver {
            machines: Arc::clone(&machines),
        }));
        for point in [
            PhaseHookPoint::StepStart,
            PhaseHookPoint::BeforeInference,
            PhaseHookPoint::AfterInference,
            PhaseHookPoint::StepEnd,
        ] {
            contributions.register_hook(Arc::new(StateMachineEventObserver {
                machines: Arc::clone(&machines),
                point,
            }));
        }
        contributions.register_guard(Arc::new(StateMachineGuard {
            machines,
            continuation,
        }));
        contributions
    }
}

impl Plugin for StateMachinePlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: STATE_MACHINE_PLUGIN_ID.into(),
            requires: Vec::new(),
            config_sections: vec![STATE_MACHINE_PLUGIN_ID.into()],
            bound: CapabilityBound {
                state_keys: IdBound::Exact(STATE_KEYS.iter().map(|k| (*k).to_string()).collect()),
                tool_gates: IdBound::Exact(vec![STATE_MACHINE_PLUGIN_ID.into()]),
                phase_hooks: vec![
                    PhaseHookPoint::StepStart,
                    PhaseHookPoint::BeforeInference,
                    PhaseHookPoint::AfterInference,
                    PhaseHookPoint::AfterTool,
                    PhaseHookPoint::StepEnd,
                ],
                run_end_guards: IdBound::Exact(vec![STATE_MACHINE_PLUGIN_ID.into()]),
                ..Default::default()
            },
        }
    }

    fn resolve(&self) -> Contributions {
        self.contribute(Arc::clone(&self.machines), self.continuation.clone())
    }

    fn resolve_configured(
        &self,
        config: Option<&Value>,
    ) -> Result<Contributions, PluginConfigError> {
        let Some(value) = config else {
            return Ok(self.resolve());
        };
        let config = StateMachineConfig::from_value(value.clone())
            .map_err(|e| PluginConfigError::new(STATE_MACHINE_PLUGIN_ID, e.to_string()))?;
        let continuation = config.continuation.clone();
        let configured = config
            .into_machines()
            .map_err(|e| PluginConfigError::new(STATE_MACHINE_PLUGIN_ID, e.to_string()))?;
        let machines = merge_machines(&self.machines, configured)?;
        Ok(self.contribute(machines.into(), continuation))
    }
}

/// Merge base machines with the agent's configured machines, rejecting a name
/// declared in both (a config error, fail closed).
fn merge_machines(
    base: &[Machine],
    configured: Vec<Machine>,
) -> Result<Vec<Machine>, PluginConfigError> {
    let mut out: Vec<Machine> = base.to_vec();
    for machine in configured {
        if out.iter().any(|b| b.name == machine.name) {
            return Err(PluginConfigError::new(
                STATE_MACHINE_PLUGIN_ID,
                format!("duplicate machine name `{}`", machine.name),
            ));
        }
        out.push(machine);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Pre-execution gate
// ---------------------------------------------------------------------------

struct StateMachineGate {
    machines: Arc<[Machine]>,
}

#[async_trait]
impl ToolGateHook for StateMachineGate {
    fn id(&self) -> &str {
        STATE_MACHINE_PLUGIN_ID
    }

    async fn gate(&self, ctx: &ToolCall, state: &Store) -> GateOutcome {
        let thread = ThreadInstances::load_or_default(state);
        let run = RunInstances::load_or_default(state);
        let evals = gate_evaluate(&self.machines, &thread, &run, &ctx.tool_id, &ctx.arguments);
        match gate_decision(&evals) {
            Some(v) if v.action == ViolationAction::Deny => GateOutcome::Block { reason: v.reason },
            Some(v) if v.action == ViolationAction::Ask => GateOutcome::RequireConfirmation {
                correlation_id: format!("fsm-{}", ctx.call_id),
            },
            // Warn and no-violation both allow; a warn surfaces at advance.
            _ => GateOutcome::Allow,
        }
    }
}

// ---------------------------------------------------------------------------
// Post-execution outcome hook
// ---------------------------------------------------------------------------

struct StateMachineObserver {
    machines: Arc<[Machine]>,
}

#[async_trait]
impl PhaseHook for StateMachineObserver {
    fn point(&self) -> PhaseHookPoint {
        PhaseHookPoint::AfterTool
    }

    async fn on_phase(
        &self,
        ctx: &PhaseContext,
        _conversation: &[Message],
        state: &Store,
    ) -> HookReaction {
        // `AfterTool` carries the executed call and its output; nothing to react
        // to at any other point.
        let PhaseKind::AfterTool(after) = &ctx.kind else {
            return HookReaction::default();
        };
        let call = &after.call;
        let output = &after.output;
        let thread_base = ThreadInstances::load_or_default(state);
        let run_base = RunInstances::load_or_default(state);

        // Local folded copies; one whole-value command per touched cell is
        // emitted at the end so multiple folds within this reaction compose.
        let mut thread = thread_base.clone();
        let mut run = run_base.clone();
        let mut metrics = Metrics::load_or_default(state);
        let mut vlog = ViolationLog::load_or_default(state);
        let (mut thread_dirty, mut run_dirty) = (false, false);
        let (mut metrics_dirty, mut vlog_dirty) = (false, false);
        // Loaded lazily on the first emit. The lifecycle hook advances the
        // thread-wide tick once per completed inference step.
        let mut throttle: Option<EmitThrottle> = None;
        let mut reaction = HookReaction::default();
        let mut request_context: Option<BTreeMap<String, Vec<Message>>> = None;

        // Attribute the gate verdict for this call: a blocked result this machine
        // set denies records a deny; an *executed* call it would ask about — a
        // resumed, approved ask, since a fresh ask suspends before this hook —
        // records an ask. Warn is surfaced below at advance.
        let gate = gate_decision(&gate_evaluate(
            &self.machines,
            &thread_base,
            &run_base,
            &call.tool_id,
            &call.arguments,
        ));
        match gate {
            Some(v) if v.action == ViolationAction::Deny && output.is_error => {
                Metrics::apply(
                    &mut metrics,
                    FsmMetricUpdate {
                        machine: v.machine.clone(),
                        event: FsmMetricEvent::Denied,
                    },
                );
                ViolationLog::apply(
                    &mut vlog,
                    FsmViolationRecord {
                        machine: v.machine,
                        key: v.key,
                        tool_name: call.tool_id.clone(),
                        action: ViolationAuditAction::Deny,
                        reason: v.reason,
                    },
                );
                metrics_dirty = true;
                vlog_dirty = true;
            }
            Some(v) if v.action == ViolationAction::Ask => {
                Metrics::apply(
                    &mut metrics,
                    FsmMetricUpdate {
                        machine: v.machine.clone(),
                        event: FsmMetricEvent::Asked,
                    },
                );
                ViolationLog::apply(
                    &mut vlog,
                    FsmViolationRecord {
                        machine: v.machine,
                        key: v.key,
                        tool_name: call.tool_id.clone(),
                        action: ViolationAuditAction::Ask,
                        reason: v.reason,
                    },
                );
                metrics_dirty = true;
                vlog_dirty = true;
            }
            _ => {}
        }

        let result = ToolResultView::new(output.is_error, &output.content);
        for op in advance_evaluate(
            &self.machines,
            &thread_base,
            &run_base,
            &call.tool_id,
            &call.arguments,
            &result,
        ) {
            match op {
                AdvanceOp::Transition {
                    scope,
                    machine,
                    key,
                    to,
                } => {
                    let transition = FsmTransition {
                        machine: machine.clone(),
                        key,
                        to,
                    };
                    match scope {
                        crate::machine::MachineScope::Thread => {
                            ThreadInstances::apply(&mut thread, transition);
                            thread_dirty = true;
                        }
                        crate::machine::MachineScope::Run => {
                            RunInstances::apply(&mut run, transition);
                            run_dirty = true;
                        }
                    }
                    Metrics::apply(
                        &mut metrics,
                        FsmMetricUpdate {
                            machine,
                            event: FsmMetricEvent::Transitioned,
                        },
                    );
                    metrics_dirty = true;
                }
                AdvanceOp::Update {
                    scope,
                    machine,
                    key,
                    initial,
                    data,
                    increment,
                    reset,
                } => match scope {
                    crate::machine::MachineScope::Thread => {
                        thread.mutate(InstanceMutation {
                            machine,
                            key,
                            initial,
                            to: None,
                            data,
                            increment,
                            reset,
                        });
                        thread_dirty = true;
                    }
                    crate::machine::MachineScope::Run => {
                        run.mutate(InstanceMutation {
                            machine,
                            key,
                            initial,
                            to: None,
                            data,
                            increment,
                            reset,
                        });
                        run_dirty = true;
                    }
                },
                AdvanceOp::Emit {
                    machine,
                    key,
                    reason,
                    target,
                    content,
                    cooldown_steps,
                    role,
                } => {
                    // Spend a tick and skip a reminder that is still cooling down.
                    let msg_key = format!("{machine}.{key}");
                    let tick =
                        throttle.get_or_insert_with(|| EmitThrottleCell::load_or_default(state));
                    if tick.on_cooldown(&msg_key, cooldown_steps) {
                        continue;
                    }
                    tick.mark(msg_key);
                    let idx = reaction.messages.len();
                    let message = build_message(&call.call_id, idx, target, role, content.clone());
                    if target == EmitTarget::Context {
                        request_context
                            .get_or_insert_with(|| ContextMessages::load_or_default(state))
                            .entry(STATE_MACHINE_PLUGIN_ID.to_string())
                            .or_default()
                            .push(message);
                    } else {
                        reaction.messages.push(message);
                    }
                    let event = match reason {
                        EmitReason::Transition => FsmMetricEvent::Emitted,
                        EmitReason::Warning => FsmMetricEvent::Warned,
                    };
                    Metrics::apply(
                        &mut metrics,
                        FsmMetricUpdate {
                            machine: machine.clone(),
                            event,
                        },
                    );
                    metrics_dirty = true;
                    if reason == EmitReason::Warning {
                        ViolationLog::apply(
                            &mut vlog,
                            FsmViolationRecord {
                                machine,
                                key,
                                tool_name: call.tool_id.clone(),
                                action: ViolationAuditAction::Warn,
                                reason: content,
                            },
                        );
                        vlog_dirty = true;
                    }
                }
            }
        }

        if thread_dirty {
            reaction.state.push(ThreadInstances::write(&thread));
        }
        if run_dirty {
            reaction.state.push(RunInstances::write(&run));
        }
        if metrics_dirty {
            reaction.state.push(Metrics::write(&metrics));
        }
        if vlog_dirty {
            reaction.state.push(ViolationLog::write(&vlog));
        }
        if let Some(throttle) = throttle {
            reaction.state.push(EmitThrottleCell::write(&throttle));
        }
        if let Some(context) = request_context {
            reaction.state.push(ContextMessages::write(&context));
        }
        reaction
    }
}

/// Build a reminder message. System targets are a system-role note; positioned
/// targets default to a user-role message (or the configured role).
fn build_message(
    call_id: &str,
    idx: usize,
    target: EmitTarget,
    role: Option<Role>,
    content: String,
) -> Message {
    let msg_role = match target {
        EmitTarget::Context | EmitTarget::System | EmitTarget::SuffixSystem => Role::System,
        EmitTarget::Session | EmitTarget::Conversation => role.unwrap_or(Role::User),
    };
    Message::text(MessageId(format!("fsm-{call_id}-{idx}")), msg_role, content)
}

// ---------------------------------------------------------------------------
// Generic lifecycle-event hook
// ---------------------------------------------------------------------------

struct StateMachineEventObserver {
    machines: Arc<[Machine]>,
    point: PhaseHookPoint,
}

#[async_trait]
impl PhaseHook for StateMachineEventObserver {
    fn point(&self) -> PhaseHookPoint {
        self.point
    }

    async fn on_phase(
        &self,
        ctx: &PhaseContext,
        _conversation: &[Message],
        state: &Store,
    ) -> HookReaction {
        let event_name = match ctx.kind {
            PhaseKind::StepStart => "step.started",
            PhaseKind::BeforeInference => "step.before_inference",
            PhaseKind::AfterInference => "step.after_inference",
            PhaseKind::StepEnd => "step.ended",
            PhaseKind::AfterTool(_) => return HookReaction::default(),
        };
        let event_data = json!({ "run_id": ctx.run_id.0, "step": ctx.step });
        let thread_base = ThreadInstances::load_or_default(state);
        let run_base = RunInstances::load_or_default(state);
        let mut thread = thread_base.clone();
        let mut run = run_base.clone();
        let mut metrics = Metrics::load_or_default(state);
        let mut context = ContextMessages::load_or_default(state);
        let mut throttle = EmitThrottleCell::load_or_default(state);
        let (mut thread_dirty, mut run_dirty, mut metrics_dirty) = (false, false, false);
        let mut context_dirty = false;
        let mut throttle_dirty = false;

        // Request-only reminders survive until the inference consumes them, then
        // the AfterInference phase clears this plugin's band before staging any
        // reminder intended for the next step.
        if self.point == PhaseHookPoint::AfterInference
            && context
                .get(STATE_MACHINE_PLUGIN_ID)
                .is_some_and(|messages| !messages.is_empty())
        {
            // ContextMessages is Commutative: omitting our map key would merge
            // with (and retain) the previous value. An explicit empty producer
            // band replaces it and makes the next request observe no reminder.
            context.insert(STATE_MACHINE_PLUGIN_ID.to_string(), Vec::new());
            context_dirty = true;
        }
        if self.point == PhaseHookPoint::AfterInference {
            throttle.tick += 1;
            throttle_dirty = true;
        }

        let mut reminder_index = 0;
        for op in event_evaluate(
            &self.machines,
            &thread_base,
            &run_base,
            MachineEventView {
                name: event_name,
                data: &event_data,
            },
        ) {
            match op {
                AdvanceOp::Transition {
                    scope,
                    machine,
                    key,
                    to,
                } => {
                    let transition = FsmTransition {
                        machine: machine.clone(),
                        key,
                        to,
                    };
                    match scope {
                        crate::machine::MachineScope::Thread => {
                            ThreadInstances::apply(&mut thread, transition);
                            thread_dirty = true;
                        }
                        crate::machine::MachineScope::Run => {
                            RunInstances::apply(&mut run, transition);
                            run_dirty = true;
                        }
                    }
                    Metrics::apply(
                        &mut metrics,
                        FsmMetricUpdate {
                            machine,
                            event: FsmMetricEvent::Transitioned,
                        },
                    );
                    metrics_dirty = true;
                }
                AdvanceOp::Update {
                    scope,
                    machine,
                    key,
                    initial,
                    data,
                    increment,
                    reset,
                } => match scope {
                    crate::machine::MachineScope::Thread => {
                        thread.mutate(InstanceMutation {
                            machine,
                            key,
                            initial,
                            to: None,
                            data,
                            increment,
                            reset,
                        });
                        thread_dirty = true;
                    }
                    crate::machine::MachineScope::Run => {
                        run.mutate(InstanceMutation {
                            machine,
                            key,
                            initial,
                            to: None,
                            data,
                            increment,
                            reset,
                        });
                        run_dirty = true;
                    }
                },
                AdvanceOp::Emit {
                    machine,
                    key,
                    target: EmitTarget::Context,
                    content,
                    cooldown_steps,
                    ..
                } => {
                    let reminder_key = format!("{machine}.{key}");
                    if throttle.on_cooldown(&reminder_key, cooldown_steps) {
                        continue;
                    }
                    throttle.mark(reminder_key);
                    throttle_dirty = true;
                    context
                        .entry(STATE_MACHINE_PLUGIN_ID.to_string())
                        .or_default()
                        .push(build_message(
                            &format!("{}-{}", ctx.run_id.0, ctx.step),
                            reminder_index,
                            EmitTarget::Context,
                            None,
                            content,
                        ));
                    reminder_index += 1;
                    context_dirty = true;
                    Metrics::apply(
                        &mut metrics,
                        FsmMetricUpdate {
                            machine,
                            event: FsmMetricEvent::Emitted,
                        },
                    );
                    metrics_dirty = true;
                }
                AdvanceOp::Emit { .. } => {
                    // Lifecycle reminders are request context. Committed messages
                    // remain a tool-transition effect, where they are atomic with
                    // the corresponding tool result.
                }
            }
        }

        let mut reaction = HookReaction::default();
        if thread_dirty {
            reaction.state.push(ThreadInstances::write(&thread));
        }
        if run_dirty {
            reaction.state.push(RunInstances::write(&run));
        }
        if metrics_dirty {
            reaction.state.push(Metrics::write(&metrics));
        }
        if context_dirty {
            reaction.state.push(ContextMessages::write(&context));
        }
        if throttle_dirty {
            reaction.state.push(EmitThrottleCell::write(&throttle));
        }
        reaction
    }
}

// ---------------------------------------------------------------------------
// Run-end continuation guard
// ---------------------------------------------------------------------------

struct StateMachineGuard {
    machines: Arc<[Machine]>,
    continuation: ContinuationSettings,
}

#[async_trait]
impl RunEndGuard for StateMachineGuard {
    fn id(&self) -> &str {
        STATE_MACHINE_PLUGIN_ID
    }

    async fn evaluate(&self, ctx: &RunEndContext<'_>) -> RunEndDecision {
        let complete = || RunEndDecision::Complete { detail: json!({}) };
        if self.continuation.max_continuations == 0
            || ctx.forced_continuations >= self.continuation.max_continuations as usize
        {
            return complete();
        }
        let thread = ThreadInstances::load_or_default(ctx.state);
        let run = RunInstances::load_or_default(ctx.state);
        let mut incomplete = incomplete_instances(&self.machines, &thread, &run);
        if incomplete.is_empty() {
            return complete();
        }
        incomplete.sort();
        let summary = incomplete.join(", ");
        let template = self
            .continuation
            .message
            .clone()
            .unwrap_or_else(|| "Finish the protocol work: {summary}".to_string());
        RunEndDecision::Steer {
            feedback: template.replace("{summary}", &summary),
            detail: json!({ "incomplete": incomplete }),
        }
    }
}

/// The `machine[key]=state` entries that have a terminal set but are not yet
/// terminal.
fn incomplete_instances(machines: &[Machine], thread: &FsmStore, run: &FsmStore) -> Vec<String> {
    let mut out = Vec::new();
    for machine in machines {
        if machine.terminal_states.is_empty() {
            continue;
        }
        let store = match machine.scope {
            crate::machine::MachineScope::Thread => thread,
            crate::machine::MachineScope::Run => run,
        };
        if let Some(instances) = store.machines.get(&machine.name) {
            for (key, instance) in instances {
                if !machine.is_terminal(&instance.state) {
                    out.push(format!("{}[{}]={}", machine.name, key, instance.state));
                }
            }
        }
    }
    out
}
