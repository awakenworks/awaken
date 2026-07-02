//! Runtime integration: the `Plugin` that contributes the state machine's gate,
//! tool-outcome hook, run-end guard, and state keys under its `CapabilityBound`.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::state::Store;
use awaken_runtime_contract::permission::{GateOutcome, PermissionContext, ToolGateHook};
use awaken_runtime_contract::plugin::{
    CapabilityBound, Contributions, Plugin, PluginConfigError, PluginManifest, RunEndContext,
    RunEndDecision, RunEndGuard, ToolOutcomeHook, ToolReaction,
};
use awaken_runtime_contract::tool::{ToolCall, ToolOutput};
use serde_json::{Value, json};

use crate::config::{ContinuationSettings, StateMachineConfig, StateMachineConfigError};
use crate::engine::{AdvanceOp, EmitReason, advance_evaluate, gate_decision, gate_evaluate};
use crate::machine::{EmitTarget, Machine, ViolationAction};
use crate::result::ToolResultView;
use crate::state::{
    FsmMetricEvent, FsmMetricUpdate, FsmStore, FsmTransition, FsmViolationRecord, Metrics,
    RunInstances, STATE_KEYS, StateCell, ThreadInstances, ViolationAuditAction, ViolationLog,
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
        contributions.state_keys = STATE_KEYS.iter().map(|k| (*k).to_string()).collect();
        contributions.tool_gates.push(Arc::new(StateMachineGate {
            machines: Arc::clone(&machines),
        }));
        contributions
            .tool_observers
            .push(Arc::new(StateMachineObserver {
                machines: Arc::clone(&machines),
            }));
        contributions
            .run_end_guards
            .push(Arc::new(StateMachineGuard {
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
                state_keys: STATE_KEYS.iter().map(|k| (*k).to_string()).collect(),
                tool_gates: vec![STATE_MACHINE_PLUGIN_ID.into()],
                tool_observers: vec![STATE_MACHINE_PLUGIN_ID.into()],
                run_end_guards: vec![STATE_MACHINE_PLUGIN_ID.into()],
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

    async fn gate(&self, ctx: &PermissionContext, state: &Store) -> GateOutcome {
        let thread = ThreadInstances::load(state);
        let run = RunInstances::load(state);
        let evals = gate_evaluate(&self.machines, &thread, &run, &ctx.tool_id, &ctx.arguments);
        match gate_decision(&evals) {
            Some(v) if v.action == ViolationAction::Deny => GateOutcome::Block { reason: v.reason },
            Some(v) if v.action == ViolationAction::Ask => GateOutcome::Suspend {
                ticket_id: format!("fsm-{}", ctx.call_id),
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
impl ToolOutcomeHook for StateMachineObserver {
    fn id(&self) -> &str {
        STATE_MACHINE_PLUGIN_ID
    }

    async fn after_tool(
        &self,
        call: &ToolCall,
        output: &ToolOutput,
        state: &Store,
    ) -> ToolReaction {
        let thread_base = ThreadInstances::load(state);
        let run_base = RunInstances::load(state);

        // Local folded copies; one whole-value command per touched cell is
        // emitted at the end so multiple folds within this reaction compose.
        let mut thread = thread_base.clone();
        let mut run = run_base.clone();
        let mut metrics = Metrics::load(state);
        let mut vlog = ViolationLog::load(state);
        let (mut thread_dirty, mut run_dirty) = (false, false);
        let (mut metrics_dirty, mut vlog_dirty) = (false, false);
        let mut reaction = ToolReaction::default();

        // A blocked result that this machine set would deny: record the deny.
        if output.is_error {
            let evals = gate_evaluate(
                &self.machines,
                &thread_base,
                &run_base,
                &call.tool_id,
                &call.arguments,
            );
            if let Some(v) = gate_decision(&evals)
                && v.action == ViolationAction::Deny
            {
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
                AdvanceOp::Emit {
                    machine,
                    key,
                    reason,
                    target,
                    content,
                    role,
                    ..
                } => {
                    let idx = reaction.messages.len();
                    reaction.messages.push(build_message(
                        &call.call_id,
                        idx,
                        target,
                        role,
                        content.clone(),
                    ));
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
        reaction
    }
}

/// Build a reminder message. System targets are a system-role note; positioned
/// targets default to a user-role turn (or the configured role).
fn build_message(
    call_id: &str,
    idx: usize,
    target: EmitTarget,
    role: Option<Role>,
    content: String,
) -> Message {
    let msg_role = match target {
        EmitTarget::System | EmitTarget::SuffixSystem => Role::System,
        EmitTarget::Session | EmitTarget::Conversation => role.unwrap_or(Role::User),
    };
    Message::text(MessageId(format!("fsm-{call_id}-{idx}")), msg_role, content)
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
        let thread = ThreadInstances::load(ctx.state);
        let run = RunInstances::load(ctx.state);
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
            for (key, state) in instances {
                if !machine.is_terminal(state) {
                    out.push(format!("{}[{}]={}", machine.name, key, state));
                }
            }
        }
    }
    out
}
