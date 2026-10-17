//! Bounded proofs of the Environment transition function. The fact and event
//! domains are finite and closed, so each harness covers every combination of
//! the production `transition_environment` inputs.

use super::laws::{
    EVENT_COUNT, STEPS, effects_require_current_lease, event, steps_follow_declared_edges,
    terminal_never_reenters,
};
use super::*;

fn any_facts() -> SessionEnvironmentFacts {
    SessionEnvironmentFacts {
        step: STEPS[usize::from(kani::any::<u8>() % 8)],
        terminality: match kani::any::<u8>() % 3 {
            0 => SessionTerminality::Active,
            1 => SessionTerminality::CleanupJoining,
            _ => SessionTerminality::Terminal,
        },
        execution_idle: kani::any(),
        activities_open: kani::any(),
        lease: match kani::any::<u8>() % 3 {
            0 => RealizationLeaseStanding::Fenced,
            1 => RealizationLeaseStanding::Unfenced,
            _ => RealizationLeaseStanding::Stale,
        },
        resident_generated: kani::any(),
        suspend_due: kani::any(),
        suspend_evidence_consistent: kani::any(),
        generation_live: kani::any(),
        checkpoint_live: kani::any(),
        continuation_topology_supported: kani::any(),
        suspend_memory_admitted: kani::any(),
    }
}

fn any_event() -> SessionEnvironmentEvent {
    event(usize::from(kani::any::<u8>()) % EVENT_COUNT)
}

/// @obligation session_environment.terminal_session_never_reenters_a_live_environment
#[kani::proof]
fn terminal_session_never_reenters_a_live_environment() {
    let facts = any_facts();
    let event = any_event();
    assert!(terminal_never_reenters(
        facts,
        event,
        transition_environment(facts, event)
    ));
}

/// @obligation session_environment.resident_is_entered_only_by_materialization_or_restore
#[kani::proof]
fn environment_steps_follow_only_declared_edges() {
    let facts = any_facts();
    let event = any_event();
    assert!(steps_follow_declared_edges(
        facts,
        event,
        transition_environment(facts, event)
    ));
}

/// @obligation session_environment.effect_instructions_require_a_current_realization_lease
#[kani::proof]
fn environment_effects_require_a_current_realization_lease() {
    let facts = any_facts();
    let event = any_event();
    assert!(effects_require_current_lease(
        facts,
        event,
        transition_environment(facts, event)
    ));
}
