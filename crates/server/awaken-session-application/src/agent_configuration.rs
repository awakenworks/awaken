// @formal entrypoint session.missing_publication_is_available_only_to_noninteractive_frozen_projection
// The named entrypoint is the sole source-ledger link. Its Kani harness proves only
// the pure missing-publication decision; profile/roster compilation keeps its native
// regressions and is not claimed to prove catalog, execution or receipt behavior.
//! Effective Agent configuration compiled from the original accepted inputs.

use awaken_session_types::RunError;

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

pub(crate) fn resolve_session_multiagent(
    profile: Option<&awaken_executable_agent_contract::ExecutableAgentSessionProfile>,
    caps: &awaken_session_configuration_contract::AgentCapabilities,
    captured: Option<
        &[(
            String,
            awaken_executable_agent_contract::ExecutableAgentSessionProfile,
        )],
    >,
    mut resolve: impl FnMut(
        &str,
        Option<u64>,
    )
        -> Option<awaken_executable_agent_contract::ExecutableAgentSessionProfile>,
) -> Result<Option<awaken_session_configuration_contract::SessionAgentRoster>, RunError> {
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
    let advisor_model = profile.and_then(|profile| profile.advisor_model.clone());
    if delegates.is_empty() && advisor_model.is_none() {
        return Ok(None);
    }
    let mut agents = Vec::with_capacity(delegates.len() + usize::from(advisor_model.is_some()));
    for delegate in delegates {
        let resolved = if let Some(captured) = captured {
            captured
                .iter()
                .find(|(id, value)| {
                    id == &delegate.agent_id
                        && delegate
                            .source_revision
                            .is_none_or(|revision| value.source_revision == revision)
                })
                .map(|(_, value)| value.clone())
        } else {
            resolve(
                &delegate.agent_id,
                if profile.is_some() {
                    delegate.source_revision
                } else {
                    None
                },
            )
        };
        let child = match resolved {
            Some(profile) => agent_configuration_from_profile(&delegate.agent_id, profile),
            None if profile.is_none() => {
                agent_configuration_from_profile(&delegate.agent_id, Default::default())
            }
            None => {
                return Err(RunError::bad_request(format!(
                    "multiagent_unavailable: Agent `{}` publication revision {:?} is unavailable",
                    delegate.agent_id, delegate.source_revision
                )));
            }
        };
        agents.push(
            awaken_session_configuration_contract::SessionAgentRosterMember::Agent(Box::new(child)),
        );
    }
    if let Some(model) = advisor_model {
        agents
            .push(awaken_session_configuration_contract::SessionAgentRosterMember::Advisor(model));
    }
    Ok(Some(
        awaken_session_configuration_contract::SessionAgentRoster { members: agents },
    ))
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
