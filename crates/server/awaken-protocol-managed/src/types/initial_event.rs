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
    UserDefineOutcome {
        max_iterations: Option<u32>,
        description_nonempty: bool,
        rubric_nonempty: bool,
    },
    Other(&'static str),
}

pub(crate) trait InitialEventSpec {
    fn initial_event_class(&self) -> InitialEventClass;
}

pub(crate) struct InitialEventPolicy {
    pub min_count: usize,
    pub max_count: usize,
    pub allow_system_message: bool,
    pub require_final_system_after_user: bool,
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
    let mut system_messages = 0usize;
    for (ordinal, event) in events.iter().enumerate() {
        match event.initial_event_class() {
            InitialEventClass::UserMessage => {}
            InitialEventClass::SystemMessage if policy.allow_system_message => {
                system_messages += 1;
                if policy.require_final_system_after_user
                    && (ordinal + 1 != events.len()
                        || ordinal == 0
                        || !matches!(
                            events[ordinal - 1].initial_event_class(),
                            InitialEventClass::UserMessage
                        ))
                {
                    return Err(
                        "system.message must be final and immediately follow its user.message"
                            .into(),
                    );
                }
            }
            InitialEventClass::SystemMessage => {
                return Err(
                    "initial_events contains unsupported event type `system.message`".into(),
                );
            }
            InitialEventClass::UserDefineOutcome {
                max_iterations,
                description_nonempty,
                rubric_nonempty,
            } => {
                outcomes += 1;
                if !description_nonempty {
                    return Err("user.define_outcome description must not be blank".into());
                }
                if !rubric_nonempty {
                    return Err("user.define_outcome rubric must not be blank".into());
                }
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
    if system_messages > 1 {
        return Err("initial_events allows at most one system.message event".into());
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

/// The one Deployment create/update policy owner. Both the public Deployment
/// union and its lowered Session input must pass these same batch invariants;
/// keeping the literal here prevents the two admission boundaries from drifting.
pub(crate) fn validate_deployment_initial_events<T: InitialEventSpec>(
    events: &[T],
) -> Result<(), String> {
    validate_initial_events(
        events,
        &InitialEventPolicy {
            min_count: 1,
            max_count: 50,
            allow_system_message: true,
            require_final_system_after_user: true,
            max_outcomes: None,
            outcome_iterations: Some(1..=20),
        },
    )
}

/// Lower an already wire-validated create batch into the one neutral Session
/// root plan. Stable operation/Run/Outcome ids are minted by the neutral
/// contract so ordinary Event admission can reuse the same formulas.
pub(crate) fn compile_session_initial_event_plan(
    session_id: &str,
    events: &[super::session::InboundEvent],
) -> Result<Option<awaken_session_contract::SessionInitialEventPlan>, String> {
    use awaken_session_contract::{SessionEventInput, SessionOutcomeRubric};

    if events.is_empty() {
        return Ok(None);
    }
    let inputs = events
        .iter()
        .map(|event| match event {
            super::session::InboundEvent::UserMessage { content } => {
                Ok(SessionEventInput::UserMessage {
                    content: content.clone(),
                })
            }
            super::session::InboundEvent::SystemMessage { content } => {
                Ok(SessionEventInput::SystemMessage {
                    content: content.clone(),
                })
            }
            super::session::InboundEvent::UserDefineOutcome {
                description,
                rubric,
                max_iterations,
            } => Ok(SessionEventInput::DefineOutcome {
                description: description.clone(),
                rubric: match rubric {
                    super::session::OutcomeRubric::Text { content } => SessionOutcomeRubric::Text {
                        content: content.clone(),
                    },
                    super::session::OutcomeRubric::File { file_id } => SessionOutcomeRubric::File {
                        file_id: file_id.clone(),
                    },
                },
                max_iterations: *max_iterations,
            }),
            other => Err(format!(
                "initial_events contains unsupported event type `{}`",
                other.type_str()
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    awaken_session_contract::SessionInitialEventPlan::compile(
        session_id,
        format!("initial:{session_id}"),
        inputs,
    )
    .map(Some)
    .map_err(|error| error.to_string())
}

/// Defensive admission for a Deployment batch after its stored public union was
/// lowered to the shared inbound DTO. The one generic validator remains the
/// rule owner; this wrapper adds the existing content/file bound.
pub(crate) fn validate_deployment_inbound_initial_events(
    events: &[super::session::InboundEvent],
) -> Result<(), String> {
    validate_deployment_initial_events(events)?;
    let file_documents = events
        .iter()
        .map(super::session::InboundEvent::validate_content)
        .try_fold(0usize, |total, count| {
            count.and_then(|count| {
                total
                    .checked_add(count)
                    .ok_or_else(|| "too many file-sourced documents".to_string())
            })
        })?;
    if file_documents > 100 {
        return Err("initial_events supports at most 100 file-sourced document blocks".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use awaken_agent_contract::agent::content::ContentBlock;

    use super::*;
    use crate::types::deployment::DeploymentInitialEvent;
    use crate::types::{InboundEvent, OutcomeRubric};

    fn user(text: &str) -> DeploymentInitialEvent {
        DeploymentInitialEvent::UserMessage {
            content: vec![ContentBlock::text(text)],
        }
    }

    fn system(text: &str) -> DeploymentInitialEvent {
        DeploymentInitialEvent::SystemMessage {
            content: vec![ContentBlock::text(text)],
        }
    }

    fn outcome(description: &str, rubric: OutcomeRubric) -> DeploymentInitialEvent {
        DeploymentInitialEvent::UserDefineOutcome {
            description: description.into(),
            rubric,
            max_iterations: Some(3),
        }
    }

    fn lowered(events: &[DeploymentInitialEvent]) -> Vec<InboundEvent> {
        events.iter().cloned().map(Into::into).collect()
    }

    #[test]
    fn deployment_system_placement_rejects_the_whole_batch() {
        // Causes: the fixtures below establish `deployment system placement` with the concrete
        // inputs, state, dependencies, and failure triggers used by this case.
        // Effects: the observable result `rejects the whole batch` and every asserted state
        // transition or side effect must hold.
        // Constraints/invariants: the Managed edge owns wire validation/projection only;
        // Session/Run stores and committed facts remain the single behavior authority.
        // Decision rule: evaluate every labeled cause partition in this test; each matching rule
        // selects only its stated effect and preserves the authority constraint.
        // Cause/effect graph: C1 System count is zero/one/multiple; C2 its
        // ordinal is final/non-final; C3 its immediate predecessor is User or
        // another Event. E1 admit the whole ordered batch; E2 reject before a
        // Session identity or root can exist. The official constraint is the
        // conjunction C1<=1 + C2=final + C3=User.
        //
        // | Rule | System count | Final | Predecessor | Effect |
        // | D1 | 1 | yes | User | E1 |
        // | D2 | 1 | yes | none | E2 |
        // | D3 | 2 | any | any | E2 |
        // | D4 | 1 | no | User | E2 |
        // | D5 | 1 | yes | Outcome | E2 |
        let cases = [
            (
                "D1/E1 user+system",
                vec![user("one"), system("context")],
                true,
            ),
            (
                "D1/E1 multiple preceding Users",
                vec![user("one"), user("two"), system("context")],
                true,
            ),
            ("D2/E2 system-only", vec![system("context")], false),
            (
                "D3/E2 multiple System Events",
                vec![user("one"), system("a"), system("b")],
                false,
            ),
            (
                "D4/E2 non-final System",
                vec![user("one"), system("context"), user("later")],
                false,
            ),
            (
                "D5/E2 Outcome predecessor",
                vec![
                    outcome(
                        "ship",
                        OutcomeRubric::Text {
                            content: "ok".into(),
                        },
                    ),
                    system("context"),
                ],
                false,
            ),
        ];

        for (rule, events, accepted) in cases {
            assert_eq!(
                validate_deployment_initial_events(&events).is_ok(),
                accepted,
                "{rule}: public Deployment union"
            );
            assert_eq!(
                validate_deployment_inbound_initial_events(&lowered(&events)).is_ok(),
                accepted,
                "{rule}: lowered Session input"
            );
        }
    }

    #[test]
    fn outcome_blank_fields_fail_before_plan_compilation() {
        // Causes: the fixtures below establish `outcome blank fields fail before plan compilation`
        // with the concrete inputs, state, dependencies, and failure triggers used by this case.
        // Effects: the observable result `all output, state, side-effect, error, and terminal
        // assertions below hold together` and every asserted state transition or side effect must
        // hold.
        // Constraints/invariants: the Managed edge owns wire validation/projection only;
        // Session/Run stores and committed facts remain the single behavior authority.
        // Coverage rationale: `outcome blank fields fail before plan compilation` is one
        // independent branch selecting `all output, state, side-effect, error, and terminal
        // assertions below hold together`; a multi-row decision table is not applicable, and
        // sibling tests own alternate causes.
        // Causes C1 description blank, C2 text rubric blank, C3 file id blank;
        // effect E1 is whole-batch rejection at the wire validator, before the
        // neutral plan can be installed. Rule O1=!C1&&!C2&&!C3=>accept;
        // O2=C1|C2|C3=>E1. Text and File cover both rubric union variants.
        let cases = [
            (
                "O1",
                outcome(
                    "ship",
                    OutcomeRubric::Text {
                        content: "rubric".into(),
                    },
                ),
                true,
            ),
            (
                "O2/C1/E1",
                outcome(
                    "  ",
                    OutcomeRubric::Text {
                        content: "rubric".into(),
                    },
                ),
                false,
            ),
            (
                "O2/C2/E1",
                outcome(
                    "ship",
                    OutcomeRubric::Text {
                        content: " \n".into(),
                    },
                ),
                false,
            ),
            (
                "O2/C3/E1",
                outcome(
                    "ship",
                    OutcomeRubric::File {
                        file_id: "\t".into(),
                    },
                ),
                false,
            ),
        ];

        for (rule, event, accepted) in cases {
            let events = [event];
            assert_eq!(
                validate_deployment_initial_events(&events).is_ok(),
                accepted,
                "{rule}: public Deployment union"
            );
            assert_eq!(
                validate_deployment_inbound_initial_events(&lowered(&events)).is_ok(),
                accepted,
                "{rule}: lowered Session input"
            );
        }
    }

    #[test]
    fn outcome_provenance_preserves_rubric_kind_and_optional_iterations() {
        // Constraints/invariants: the Managed edge owns wire validation/projection only;
        // Session/Run stores and committed facts remain the single behavior authority.
        // Decision rule: evaluate every labeled cause partition in this test; each matching rule
        // selects only its stated effect and preserves the authority constraint.
        // Wire/provenance decision table. Causes: C1 rubric is Text/File; C2
        // max_iterations is omitted/explicit null/value. Effects: E1 the
        // neutral root preserves the rubric variant and exact payload; E2 a
        // value remains Some(value); E3 omitted and explicit null both become
        // None. The official DTO permits both omitted and null, but current
        // serde Option semantics make those two inputs observably equivalent.
        //
        // | Rule | Rubric | max_iterations | Root provenance |
        // | P1 | Text | omitted | Text + None |
        // | P2 | Text | null | Text + None |
        // | P3 | File | 7 | File + Some(7) |
        let omitted: InboundEvent = serde_json::from_value(serde_json::json!({
            "type": "user.define_outcome",
            "description": "ship",
            "rubric": { "type": "text", "content": "correct" }
        }))
        .expect("P1 wire");
        let explicit_null: InboundEvent = serde_json::from_value(serde_json::json!({
            "type": "user.define_outcome",
            "description": "ship",
            "rubric": { "type": "text", "content": "correct" },
            "max_iterations": null
        }))
        .expect("P2 wire");
        match (&omitted, &explicit_null) {
            (
                InboundEvent::UserDefineOutcome {
                    description: omitted_description,
                    rubric:
                        OutcomeRubric::Text {
                            content: omitted_content,
                        },
                    max_iterations: omitted_iterations,
                },
                InboundEvent::UserDefineOutcome {
                    description: null_description,
                    rubric:
                        OutcomeRubric::Text {
                            content: null_content,
                        },
                    max_iterations: null_iterations,
                },
            ) => {
                assert_eq!(omitted_description, null_description, "P1+P2/E3");
                assert_eq!(omitted_content, null_content, "P1+P2/E3");
                assert!(
                    omitted_iterations.is_none() && null_iterations.is_none(),
                    "P1+P2/E3"
                );
            }
            _ => panic!("P1+P2 expected equivalent Text Outcome inputs"),
        }

        let text = compile_session_initial_event_plan("text", &[omitted])
            .unwrap()
            .unwrap();
        let awaken_session_contract::SessionEventCommand::DefineOutcome {
            rubric,
            max_iterations,
            ..
        } = &text.batch.events[0].event
        else {
            panic!("P1 Outcome command")
        };
        assert!(
            matches!(
                rubric,
                awaken_session_contract::SessionOutcomeRubric::Text { content }
                    if content == "correct"
            ),
            "P1/E1"
        );
        assert_eq!(*max_iterations, None, "P1/E3");

        let file = InboundEvent::UserDefineOutcome {
            description: "ship".into(),
            rubric: OutcomeRubric::File {
                file_id: "file_rubric".into(),
            },
            max_iterations: Some(7),
        };
        let file = compile_session_initial_event_plan("file", &[file])
            .unwrap()
            .unwrap();
        let awaken_session_contract::SessionEventCommand::DefineOutcome {
            rubric,
            max_iterations,
            ..
        } = &file.batch.events[0].event
        else {
            panic!("P3 Outcome command")
        };
        assert!(
            matches!(
                rubric,
                awaken_session_contract::SessionOutcomeRubric::File { file_id }
                    if file_id == "file_rubric"
            ),
            "P3/E1"
        );
        assert_eq!(*max_iterations, Some(7), "P3/E2");
    }
}
