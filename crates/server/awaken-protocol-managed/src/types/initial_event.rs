//! Shared admission rules for `initial_events` collections.
//!
//! Sessions and Deployments expose different tagged unions and different empty-list
//! semantics, but their count, outcome-cardinality, and iteration constraints are
//! one concern.  Both wire owners classify their own variants and delegate those
//! cross-resource invariants here.

use std::ops::RangeInclusive;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InitialEventClass {
    UserMessage,
    SystemMessage,
    UserDefineOutcome { max_iterations: Option<u32> },
    Other(&'static str),
}

pub(crate) trait InitialEventSpec {
    fn initial_event_class(&self) -> InitialEventClass;
}

pub(crate) struct InitialEventPolicy {
    pub min_count: usize,
    pub max_count: usize,
    pub allow_system_message: bool,
    pub max_outcomes: Option<usize>,
    pub outcome_iterations: Option<RangeInclusive<u32>>,
}

pub(crate) fn validate_initial_events<T: InitialEventSpec>(
    events: &[T],
    policy: &InitialEventPolicy,
) -> Result<(), String> {
    if !(policy.min_count..=policy.max_count).contains(&events.len()) {
        return if policy.min_count == 0 {
            Err(format!(
                "initial_events must contain at most {} events",
                policy.max_count
            ))
        } else {
            Err(format!(
                "initial_events must contain between {} and {} events",
                policy.min_count, policy.max_count
            ))
        };
    }

    let mut outcomes = 0usize;
    for event in events {
        match event.initial_event_class() {
            InitialEventClass::UserMessage => {}
            InitialEventClass::SystemMessage if policy.allow_system_message => {}
            InitialEventClass::SystemMessage => {
                return Err(
                    "initial_events contains unsupported event type `system.message`".into(),
                );
            }
            InitialEventClass::UserDefineOutcome { max_iterations } => {
                outcomes += 1;
                if let (Some(iterations), Some(allowed)) =
                    (max_iterations, policy.outcome_iterations.as_ref())
                    && !allowed.contains(&iterations)
                {
                    return Err(format!(
                        "max_iterations must be between {} and {}",
                        allowed.start(),
                        allowed.end()
                    ));
                }
            }
            InitialEventClass::Other(kind) => {
                return Err(format!(
                    "initial_events contains unsupported event type `{kind}`"
                ));
            }
        }
    }
    if policy
        .max_outcomes
        .is_some_and(|maximum| outcomes > maximum)
    {
        return Err(format!(
            "initial_events allows at most {} user.define_outcome event",
            policy.max_outcomes.expect("checked above")
        ));
    }
    Ok(())
}
