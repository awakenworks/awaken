//! Runtime instance state for tool state machines.
//!
//! Four `(scope, key)` cells back the machine set: thread- and run-scoped
//! instance state, plus thread-scoped audit metrics and a bounded violation log.
//! Each is a folding state key — the contract's [`StateKey`] (address/scope/value)
//! plus [`FoldStateKey`] (a typed `apply` delta), ADR-0055: a read deserializes
//! the whole value, an update folds a typed delta with `apply`, and a commit
//! writes the whole value back as one `Command`. All cells use
//! `MergePolicy::Disjoint` — each has a single producer (this extension), so a
//! later whole-value write replaces the earlier one and replay reproduces the
//! folded value. These cells tolerate a shape drift by resetting to the default,
//! so callers read through [`StateKey::load_or_default`].

use std::collections::HashMap;

use awaken_agent_contract::agent::state::{FoldStateKey, Scope, StateKey};
use serde::Serialize;

/// The keys this extension may contribute, kept in sync with the plugin's
/// `CapabilityBound.state_keys` (G30).
pub const STATE_KEYS: &[&str] = &[
    ThreadInstances::KEY,
    RunInstances::KEY,
    Metrics::KEY,
    ViolationLog::KEY,
    EmitThrottleCell::KEY,
];

// ---------------------------------------------------------------------------
// Instance state
// ---------------------------------------------------------------------------

/// A transition record: set `machine[key] = to`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsmTransition {
    pub machine: String,
    pub key: String,
    pub to: String,
}

/// Instance-state store: `machine name -> (instance key -> current state)`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(default, transparent)]
pub struct FsmStore {
    pub machines: HashMap<String, HashMap<String, String>>,
}

impl FsmStore {
    /// Current state of an instance, if it has ever transitioned.
    #[must_use]
    pub fn current(&self, machine: &str, key: &str) -> Option<&str> {
        self.machines.get(machine)?.get(key).map(String::as_str)
    }

    fn reduce(&mut self, t: FsmTransition) {
        self.machines
            .entry(t.machine)
            .or_default()
            .insert(t.key, t.to);
    }
}

/// Thread-scoped instance state (persists across runs on the same thread).
pub struct ThreadInstances;
impl StateKey for ThreadInstances {
    const KEY: &'static str = "tool_fsm_thread_state";
    const SCOPE: Scope = Scope::Thread;
    type Value = FsmStore;
}
impl FoldStateKey for ThreadInstances {
    type Update = FsmTransition;
    fn apply(value: &mut Self::Value, update: Self::Update) {
        value.reduce(update);
    }
}

/// Run-scoped instance state (reset at the start of every run).
pub struct RunInstances;
impl StateKey for RunInstances {
    const KEY: &'static str = "tool_fsm_run_state";
    const SCOPE: Scope = Scope::Run;
    type Value = FsmStore;
}
impl FoldStateKey for RunInstances {
    type Update = FsmTransition;
    fn apply(value: &mut Self::Value, update: Self::Update) {
        value.reduce(update);
    }
}

// ---------------------------------------------------------------------------
// Audit metrics
// ---------------------------------------------------------------------------

/// Counted protocol events for audit and evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsmMetricEvent {
    Asked,
    Denied,
    Warned,
    Transitioned,
    Emitted,
}

/// Increment one metric bucket for a machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsmMetricUpdate {
    pub machine: String,
    pub event: FsmMetricEvent,
}

/// Metric counters for a single machine or the aggregate total.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(default)]
pub struct FsmMetricCounts {
    pub asked: u64,
    pub denied: u64,
    pub warned: u64,
    pub transitioned: u64,
    pub emitted: u64,
}

impl FsmMetricCounts {
    fn increment(&mut self, event: FsmMetricEvent) {
        match event {
            FsmMetricEvent::Asked => self.asked += 1,
            FsmMetricEvent::Denied => self.denied += 1,
            FsmMetricEvent::Warned => self.warned += 1,
            FsmMetricEvent::Transitioned => self.transitioned += 1,
            FsmMetricEvent::Emitted => self.emitted += 1,
        }
    }
}

/// Durable audit counters for state-machine enforcement.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(default)]
pub struct FsmMetrics {
    pub total: FsmMetricCounts,
    pub by_machine: HashMap<String, FsmMetricCounts>,
}

/// Thread-scoped audit counters.
pub struct Metrics;
impl StateKey for Metrics {
    const KEY: &'static str = "tool_fsm_metrics";
    const SCOPE: Scope = Scope::Thread;
    type Value = FsmMetrics;
}
impl FoldStateKey for Metrics {
    type Update = FsmMetricUpdate;
    fn apply(value: &mut Self::Value, update: Self::Update) {
        value.total.increment(update.event);
        value
            .by_machine
            .entry(update.machine)
            .or_default()
            .increment(update.event);
    }
}

// ---------------------------------------------------------------------------
// Violation log
// ---------------------------------------------------------------------------

/// Enforcement action recorded in the bounded violation log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViolationAuditAction {
    Ask,
    Deny,
    Warn,
}

/// A bounded durable sample of a protocol violation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct FsmViolationRecord {
    pub machine: String,
    pub key: String,
    pub tool_name: String,
    pub action: ViolationAuditAction,
    pub reason: String,
}

/// Durable bounded violation samples for protocol audit (newest 64 kept).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(default)]
pub struct FsmViolationLog {
    pub records: Vec<FsmViolationRecord>,
}

impl FsmViolationLog {
    const MAX_RECORDS: usize = 64;
}

/// Thread-scoped bounded violation samples.
pub struct ViolationLog;
impl StateKey for ViolationLog {
    const KEY: &'static str = "tool_fsm_violation_log";
    const SCOPE: Scope = Scope::Thread;
    type Value = FsmViolationLog;
}
impl FoldStateKey for ViolationLog {
    type Update = FsmViolationRecord;
    fn apply(value: &mut Self::Value, update: Self::Update) {
        value.records.push(update);
        let overflow = value
            .records
            .len()
            .saturating_sub(FsmViolationLog::MAX_RECORDS);
        if overflow > 0 {
            value.records.drain(0..overflow);
        }
    }
}

// ---------------------------------------------------------------------------
// Emit throttle (cooldown)
// ---------------------------------------------------------------------------

/// Per-emit-key cooldown state: a monotonic tick (bumped once per tool result
/// that fires an emit) and the tick each emit key last fired at. A reminder is
/// re-injected only when `tick - last >= cooldown_turns`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(default)]
pub struct EmitThrottle {
    pub tick: u64,
    pub last: HashMap<String, u64>,
}

impl EmitThrottle {
    /// Whether `key` is still cooling down at the current `tick`.
    #[must_use]
    pub fn on_cooldown(&self, key: &str, cooldown_turns: u32) -> bool {
        cooldown_turns > 0
            && self
                .last
                .get(key)
                .is_some_and(|last| self.tick.saturating_sub(*last) < u64::from(cooldown_turns))
    }

    /// Record that `key` fired at the current tick.
    pub fn mark(&mut self, key: String) {
        self.last.insert(key, self.tick);
    }
}

/// Thread-scoped emit cooldown state.
pub struct EmitThrottleCell;
impl StateKey for EmitThrottleCell {
    const KEY: &'static str = "tool_fsm_emit_throttle";
    const SCOPE: Scope = Scope::Thread;
    type Value = EmitThrottle;
}

#[cfg(test)]
mod tests {
    use awaken_agent_contract::agent::state::{MergePolicy, Store};

    use super::*;

    #[test]
    fn throttle_gates_by_cooldown() {
        let mut t = EmitThrottle {
            tick: 1,
            ..Default::default()
        };
        assert!(!t.on_cooldown("k", 3)); // never fired
        t.mark("k".into());
        t.tick = 2;
        assert!(t.on_cooldown("k", 3)); // 2-1 < 3
        t.tick = 4;
        assert!(!t.on_cooldown("k", 3)); // 4-1 >= 3
        assert!(!t.on_cooldown("k", 0)); // disabled
    }

    #[test]
    fn instance_cell_round_trips_through_store() {
        let mut store = Store::new();
        store.apply(
            &ThreadInstances::commit(
                &store,
                FsmTransition {
                    machine: "m".into(),
                    key: "a.rs".into(),
                    to: "read".into(),
                },
            )
            .unwrap(),
        );
        assert_eq!(
            ThreadInstances::load_or_default(&store).current("m", "a.rs"),
            Some("read")
        );
        assert_eq!(
            ThreadInstances::load_or_default(&store).current("m", "b.rs"),
            None
        );
    }

    #[test]
    fn instance_cell_overwrites_same_instance() {
        let mut store = Store::new();
        for to in ["read", "written"] {
            store.apply(
                &ThreadInstances::commit(
                    &store,
                    FsmTransition {
                        machine: "m".into(),
                        key: "k".into(),
                        to: to.into(),
                    },
                )
                .unwrap(),
            );
        }
        assert_eq!(
            ThreadInstances::load_or_default(&store).current("m", "k"),
            Some("written")
        );
    }

    #[test]
    fn scopes_and_keys_are_declared() {
        assert_eq!(ThreadInstances::SCOPE, Scope::Thread);
        assert_eq!(RunInstances::SCOPE, Scope::Run);
        assert_eq!(Metrics::MERGE, MergePolicy::Disjoint);
        assert_eq!(STATE_KEYS.len(), 5);
    }

    #[test]
    fn run_scoped_instances_reset_across_a_run_boundary_but_thread_scoped_persist() {
        // Behavioral counterpart to the scope *declaration* above: a run boundary is
        // modeled by replaying only the durable (thread-scoped) commands, since a
        // run-scoped command belongs to the finished run and is not carried forward.
        // Thread-scoped instance state must survive; run-scoped must reset to default.
        let mut store = Store::new();
        let transition = |machine: &str| FsmTransition {
            machine: machine.into(),
            key: "a.rs".into(),
            to: "written".into(),
        };
        // The same logical transition recorded once under each scope.
        let thread_cmd = ThreadInstances::commit(&store, transition("rbw")).unwrap();
        store.apply(&thread_cmd);
        let run_cmd = RunInstances::commit(&store, transition("lock")).unwrap();
        store.apply(&run_cmd);

        // Within the run both cells hold their value.
        assert_eq!(
            ThreadInstances::load_or_default(&store).current("rbw", "a.rs"),
            Some("written")
        );
        assert_eq!(
            RunInstances::load_or_default(&store).current("lock", "a.rs"),
            Some("written")
        );

        // The next run replays only the thread-scoped command.
        let next_run = Store::rebuild(&[thread_cmd]);
        assert_eq!(
            ThreadInstances::load_or_default(&next_run).current("rbw", "a.rs"),
            Some("written"),
            "thread-scoped instance state persists across runs on the same thread"
        );
        assert_eq!(
            RunInstances::load_or_default(&next_run).current("lock", "a.rs"),
            None,
            "run-scoped instance state resets at the start of the next run"
        );
    }

    #[test]
    fn metrics_count_total_and_per_machine() {
        let mut store = Store::new();
        for event in [
            FsmMetricEvent::Denied,
            FsmMetricEvent::Transitioned,
            FsmMetricEvent::Transitioned,
        ] {
            store.apply(
                &Metrics::commit(
                    &store,
                    FsmMetricUpdate {
                        machine: "m".into(),
                        event,
                    },
                )
                .unwrap(),
            );
        }
        let metrics = Metrics::load_or_default(&store);
        assert_eq!(metrics.total.denied, 1);
        assert_eq!(metrics.total.transitioned, 2);
        assert_eq!(metrics.by_machine["m"].transitioned, 2);
    }

    #[test]
    fn violation_log_keeps_last_records() {
        let mut store = Store::new();
        for index in 0..70 {
            store.apply(
                &ViolationLog::commit(
                    &store,
                    FsmViolationRecord {
                        machine: "m".into(),
                        key: format!("k{index}"),
                        tool_name: "Tool".into(),
                        action: ViolationAuditAction::Deny,
                        reason: "reason".into(),
                    },
                )
                .unwrap(),
            );
        }
        let log = ViolationLog::load_or_default(&store);
        assert_eq!(log.records.len(), 64);
        assert_eq!(log.records[0].key, "k6");
        assert_eq!(log.records[63].key, "k69");
    }
}
