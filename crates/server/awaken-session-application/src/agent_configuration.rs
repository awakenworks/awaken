// @formal entrypoint session.missing_publication_is_available_only_to_noninteractive_frozen_projection
// The named entrypoint is the sole source-ledger link. Its Kani harness proves only
// the pure missing-publication decision; profile/roster compilation keeps its native
// regressions and is not claimed to prove catalog, execution or receipt behavior.
//! Effective Agent configuration compiled from the original accepted inputs.

use awaken_executable_agent_contract::{ExecutableAgentDelegate, ExecutableAgentSessionProfile};
use awaken_runtime_contract::runs::RunError;
use awaken_session_configuration_contract::{AgentCapabilities, SessionAgentRoster};

/// The original ordered publication captures shared by creation and roster compilation.
pub(crate) type CapturedDelegates = Vec<(String, ExecutableAgentSessionProfile)>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionRecoveryPurpose {
    Interactive,
    CollectionRead,
    FrozenControl,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RehydrationPublicationDecision {
    Available,
    Unpinned,
    NonInteractiveFrozenProjection,
    RejectMissingExact,
}

/// Decide whether projection recovery may proceed without a catalog profile.
/// Interactive recovery remains fail-closed. Collection reads and controls over
/// already-frozen work may project only the Session's durable baseline; neither
/// path restores a Runtime or makes the missing publication executable.
#[must_use]
pub(crate) const fn rehydration_publication_decision(
    purpose: SessionRecoveryPurpose,
    has_frozen_revision: bool,
    profile_available: bool,
) -> RehydrationPublicationDecision {
    match (profile_available, has_frozen_revision, purpose) {
        (true, _, _) => RehydrationPublicationDecision::Available,
        (false, false, _) => RehydrationPublicationDecision::Unpinned,
        (false, true, SessionRecoveryPurpose::Interactive) => {
            RehydrationPublicationDecision::RejectMissingExact
        }
        (false, true, _) => RehydrationPublicationDecision::NonInteractiveFrozenProjection,
    }
}

#[cfg(test)]
#[test]
fn rehydration_publication_decision_table_is_exact() {
    // The three causes form all twelve rows: profile availability dominates;
    // otherwise only a pinned interactive recovery rejects. This executes the
    // existing Kani decision surface on native platforms as well.
    for purpose in [
        SessionRecoveryPurpose::Interactive,
        SessionRecoveryPurpose::CollectionRead,
        SessionRecoveryPurpose::FrozenControl,
    ] {
        for pinned in [false, true] {
            for available in [false, true] {
                let expected = if available {
                    RehydrationPublicationDecision::Available
                } else if !pinned {
                    RehydrationPublicationDecision::Unpinned
                } else if purpose == SessionRecoveryPurpose::Interactive {
                    RehydrationPublicationDecision::RejectMissingExact
                } else {
                    RehydrationPublicationDecision::NonInteractiveFrozenProjection
                };
                assert_eq!(
                    rehydration_publication_decision(purpose, pinned, available),
                    expected
                );
            }
        }
    }
}

#[cfg(kani)]
/// @obligation session.missing_publication_is_available_only_to_noninteractive_frozen_projection
/// @kani profile=solver-kissat-v1
/// @proof-input-debt free_symbol rehydration_publication_decision has_frozen_revision
/// @proof-input-debt free_symbol rehydration_publication_decision profile_available
#[kani::proof]
fn missing_agent_publication_is_available_only_to_noninteractive_projection() {
    let purpose_discriminant: u8 = kani::any();
    kani::assume(purpose_discriminant < 3);
    let has_frozen_revision: bool = kani::any();
    let profile_available: bool = kani::any();
    let purpose = match purpose_discriminant {
        0 => SessionRecoveryPurpose::Interactive,
        1 => SessionRecoveryPurpose::CollectionRead,
        _ => SessionRecoveryPurpose::FrozenControl,
    };
    let decision =
        rehydration_publication_decision(purpose, has_frozen_revision, profile_available);

    assert_eq!(
        decision == RehydrationPublicationDecision::NonInteractiveFrozenProjection,
        purpose != SessionRecoveryPurpose::Interactive && has_frozen_revision && !profile_available
    );
    assert_eq!(
        decision == RehydrationPublicationDecision::RejectMissingExact,
        purpose == SessionRecoveryPurpose::Interactive && has_frozen_revision && !profile_available
    );
}

/// Capture direct publications once in declaration order. The caller retains
/// its exact/current selection and admission policy; consumers borrow these
/// same values for the accepted roster and transitive Skill admission.
pub(crate) fn capture_accepted_delegates(
    delegates: &[ExecutableAgentDelegate],
    mut resolve: impl FnMut(&ExecutableAgentDelegate) -> Result<ExecutableAgentSessionProfile, RunError>,
) -> Result<CapturedDelegates, RunError> {
    delegates
        .iter()
        .map(|reference| Ok((reference.agent_id.clone(), resolve(reference)?)))
        .collect()
}

/// Bare Runtime capabilities name current profiles, with the original default
/// for an unavailable child. Capture each occurrence once, in declared order.
pub(crate) fn capture_capability_delegates(
    caps: &AgentCapabilities,
    mut resolve: impl FnMut(&str, Option<u64>) -> Option<ExecutableAgentSessionProfile>,
) -> CapturedDelegates {
    caps.delegates
        .iter()
        .map(|id| (id.clone(), resolve(id, None).unwrap_or_default()))
        .collect()
}

pub(crate) fn resolve_session_multiagent(
    profile: Option<&ExecutableAgentSessionProfile>,
    caps: &AgentCapabilities,
    captured: Option<CapturedDelegates>,
    mut resolve: impl FnMut(&str, Option<u64>) -> Option<ExecutableAgentSessionProfile>,
) -> Result<(Option<SessionAgentRoster>, CapturedDelegates), RunError> {
    let delegates = profile.map_or_else(
        || {
            caps.delegates
                .iter()
                .cloned()
                .map(
                    |agent_id| awaken_executable_agent_contract::ExecutableAgentDelegate {
                        agent_id,
                        source_revision: None,
                    },
                )
                .collect()
        },
        |profile| profile.delegates.clone(),
    );
    let unavailable = |delegate: &ExecutableAgentDelegate| {
        RunError::bad_request(format!(
            "multiagent_unavailable: Agent `{}` publication revision {:?} is unavailable",
            delegate.agent_id, delegate.source_revision
        ))
    };
    let captured = match captured {
        Some(captured) => captured,
        None if profile.is_none() => capture_capability_delegates(caps, &mut resolve),
        None => capture_accepted_delegates(&delegates, |delegate| {
            resolve(&delegate.agent_id, delegate.source_revision)
                .ok_or_else(|| unavailable(delegate))
        })?,
    };
    let advisor_model = profile.and_then(|profile| profile.advisor_model.clone());
    if delegates.is_empty() && advisor_model.is_none() {
        return Ok((None, captured));
    }
    let mut agents = Vec::with_capacity(delegates.len() + usize::from(advisor_model.is_some()));
    // Captures follow declaration order, including repeated ids. A first-id
    // lookup would let a pinned edge replace a later current edge's value.
    for (position, delegate) in delegates.into_iter().enumerate() {
        let resolved = captured
            .get(position)
            .filter(|(id, value)| {
                id == &delegate.agent_id
                    && delegate
                        .source_revision
                        .is_none_or(|revision| value.source_revision == revision)
            })
            .map(|(_, value)| value.clone());
        let child = match resolved {
            Some(profile) => agent_configuration_from_profile(&delegate.agent_id, profile),
            None if profile.is_none() => {
                agent_configuration_from_profile(&delegate.agent_id, Default::default())
            }
            None => return Err(unavailable(&delegate)),
        };
        agents.push(
            awaken_session_configuration_contract::SessionAgentRosterMember::Agent(Box::new(child)),
        );
    }
    if let Some(model) = advisor_model {
        agents
            .push(awaken_session_configuration_contract::SessionAgentRosterMember::Advisor(model));
    }
    Ok((Some(SessionAgentRoster { members: agents }), captured))
}

#[cfg(test)]
#[test]
fn capability_capture_keeps_current_defaults_and_every_declared_occurrence() {
    // R1 table: caps empty / present / missing / repeated [x,missing,x].
    // Each occurrence queries current (None) once, preserves declaration order,
    // and freezes an absent child's original default. The later Some compiler
    // performs no lookup and returns the same allocation; default revision0
    // still presents as version1. A duplicate id cannot alias the first value.
    for (ids, selected, expected) in [
        (vec![], vec![], vec![]),
        (vec!["x"], vec![Some(7)], vec![7]),
        (vec!["missing"], vec![None], vec![1]),
        (
            vec!["x", "missing", "x"],
            vec![Some(7), None, Some(8)],
            vec![7, 1, 8],
        ),
    ] {
        let caps = AgentCapabilities {
            delegates: ids.iter().map(|id| (*id).to_owned()).collect(),
            ..Default::default()
        };
        let mut values = selected.into_iter();
        let mut calls = Vec::new();
        let captured = capture_capability_delegates(&caps, |id, revision| {
            calls.push((id.to_owned(), revision));
            values
                .next()
                .unwrap()
                .map(|source_revision| ExecutableAgentSessionProfile {
                    source_revision,
                    ..Default::default()
                })
        });
        assert!(values.next().is_none());
        assert_eq!(
            calls,
            ids.iter()
                .map(|id| ((*id).to_owned(), None))
                .collect::<Vec<_>>()
        );
        let allocation = (captured.as_ptr(), captured.capacity());
        let (roster, returned) = resolve_session_multiagent(None, &caps, Some(captured), |_, _| {
            panic!("R1 late compilation must consume the original capability capture")
        })
        .unwrap();
        assert_eq!((returned.as_ptr(), returned.capacity()), allocation);
        let versions = roster
            .into_iter()
            .flat_map(|roster| roster.members)
            .map(|member| match member {
                awaken_session_configuration_contract::SessionAgentRosterMember::Agent(agent) => {
                    agent.version
                }
                _ => panic!("R1 capabilities declare ordinary children"),
            })
            .collect::<Vec<_>>();
        assert_eq!(versions, expected);
    }
}

#[cfg(test)]
#[test]
fn captured_roster_preserves_each_declared_edge_and_refuses_misaligned_values() {
    // Causes: ordered exact/current edges, repeated ids, and corresponding
    // captures that are complete, absent, misordered or at the wrong revision.
    // R1-4 complete => each edge's own selected revision, original multiplicity
    // and final Advisor; R5-7 invalid => original refusal, no catalog fallback.
    // The former first-id match changed [(x,7),(x,current8)] into [7,7].
    use awaken_executable_agent_contract::{
        ExecutableAgentDelegate, ExecutableAgentSessionProfile,
    };
    for (edges, values, expected) in [
        (
            vec![Some(7), None],
            vec![("x", 7), ("x", 8)],
            Some(vec![7, 8]),
        ),
        (
            vec![None, Some(7)],
            vec![("x", 8), ("x", 7)],
            Some(vec![8, 7]),
        ),
        (vec![None, None], vec![("x", 7), ("x", 8)], Some(vec![7, 8])),
        (
            vec![Some(7), Some(8)],
            vec![("x", 7), ("x", 8)],
            Some(vec![7, 8]),
        ),
        (vec![Some(7), None], vec![("x", 7)], None),
        (vec![Some(7), None], vec![("other", 7), ("x", 8)], None),
        (vec![Some(7), None], vec![("x", 8), ("x", 7)], None),
    ] {
        let profile = ExecutableAgentSessionProfile {
            delegates: edges
                .into_iter()
                .map(|source_revision| ExecutableAgentDelegate {
                    agent_id: "x".into(),
                    source_revision,
                })
                .collect(),
            advisor_model: Some("advisor".into()),
            ..Default::default()
        };
        let captured = if expected.is_some() {
            let mut selected = values.into_iter();
            let captured = capture_accepted_delegates(&profile.delegates, |reference| {
                let (id, source_revision) = selected.next().unwrap();
                assert_eq!(id, reference.agent_id);
                Ok(ExecutableAgentSessionProfile {
                    source_revision,
                    ..Default::default()
                })
            })
            .unwrap();
            assert!(
                selected.next().is_none(),
                "R1-4 one capture per declared occurrence"
            );
            captured
        } else {
            values
                .into_iter()
                .map(|(id, source_revision)| {
                    (
                        id.to_owned(),
                        ExecutableAgentSessionProfile {
                            source_revision,
                            ..Default::default()
                        },
                    )
                })
                .collect::<Vec<_>>()
        };
        let allocation = (captured.as_ptr(), captured.capacity());
        let result = resolve_session_multiagent(
            Some(&profile),
            &Default::default(),
            Some(captured),
            |_, _| panic!("R1-7 captured admission cannot reselect a publication"),
        );
        if let Some(expected) = expected {
            let (roster, selected) = result.unwrap();
            assert_eq!(
                (selected.as_ptr(), selected.capacity()),
                allocation,
                "R1-4 move the original capture, without cloning its Vec"
            );
            let roster = roster.unwrap();
            let versions = roster
                .members
                .iter()
                .filter_map(|member| match member {
                    awaken_session_configuration_contract::SessionAgentRosterMember::Agent(
                        agent,
                    ) => Some(agent.version),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(versions, expected);
            assert_eq!(
                selected.len(),
                expected.len(),
                "R1-4 original capture returned"
            );
            assert!(matches!(roster.members.last(), Some(
                awaken_session_configuration_contract::SessionAgentRosterMember::Advisor(model)
            ) if model == "advisor"));
        } else {
            assert_eq!(result.unwrap_err().code, "invalid_request");
        }
    }
}

/// Project one already selected profile into the existing Session configuration.
/// Catalog selection and durable acceptance remain with the caller.
pub fn agent_configuration_from_profile(
    agent_id: &str,
    profile: awaken_executable_agent_contract::ExecutableAgentSessionProfile,
) -> awaken_session_configuration_contract::SessionAgentConfiguration {
    awaken_session_configuration_contract::SessionAgentConfiguration {
        agent_id: agent_id.to_owned(),
        version: profile.source_revision.max(1),
        model: awaken_session_configuration_contract::SessionModelChoice {
            model_id: profile
                .model
                .or(profile.execution_model_ref)
                .unwrap_or_default(),
            inference: profile.inference,
            source: awaken_session_configuration_contract::SessionModelSource::Publication,
        },
        name: profile.name.unwrap_or_else(|| agent_id.to_owned()),
        description: profile.description,
        system: profile.system,
        tools: awaken_session_configuration_contract::SessionToolConfiguration {
            toolsets: profile.toolsets,
            client_tools: profile.client_tools,
        },
        mcp_servers: profile
            .mcp_servers
            .into_iter()
            .map(
                |server| awaken_session_configuration_contract::VisibleMcpServer {
                    name: server.name,
                    target: server.target,
                    prompts_as_skills: server.prompts_as_skills,
                },
            )
            .collect(),
        skills: profile.skills,
        multiagent: None,
    }
}

#[cfg(test)]
#[test]
fn missing_exact_publication_is_readable_but_not_interactively_recoverable() {
    // Cause/effect table: C1=exact publication available; C2=operation may
    // start/resume execution or projects/controls only frozen truth.
    // E1=available publication is used; E2=interactive recovery without it
    // rejects; E3=noninteractive projection rebuilds no executable state.
    // Rules P1 C1=>E1; P2 !C1+interactive=>E2;
    // P3 !C1+collection-or-frozen-control=>E3.
    assert_eq!(
        rehydration_publication_decision(SessionRecoveryPurpose::Interactive, true, false),
        RehydrationPublicationDecision::RejectMissingExact
    );
    assert_eq!(
        rehydration_publication_decision(SessionRecoveryPurpose::CollectionRead, true, false),
        RehydrationPublicationDecision::NonInteractiveFrozenProjection
    );
    assert_eq!(
        rehydration_publication_decision(SessionRecoveryPurpose::FrozenControl, true, false),
        RehydrationPublicationDecision::NonInteractiveFrozenProjection
    );
    for purpose in [
        SessionRecoveryPurpose::Interactive,
        SessionRecoveryPurpose::CollectionRead,
        SessionRecoveryPurpose::FrozenControl,
    ] {
        assert_eq!(
            rehydration_publication_decision(purpose, true, true),
            RehydrationPublicationDecision::Available
        );
    }
}
