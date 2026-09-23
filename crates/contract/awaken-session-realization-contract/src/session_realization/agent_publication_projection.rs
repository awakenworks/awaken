//! Canonical projection and validation of the Agent publication frozen into a Session.

use crate::RunError;

/// Project every Session-owned Agent override onto one immutable publication.
///
/// This is the only lowering rule for Session-local model, inference, and
/// system-prompt selection. Coordinator publication, co-located execution, and
/// cold Worker recovery must reuse the returned snapshot; Runtime adapters may
/// validate its frozen coordinates but must not reconstruct it field by field.
pub fn project_effective_agent_publication(
    model_override: Option<&crate::SessionModelOverride>,
    system_prompt: &crate::SessionSystemPromptSelection,
    workspace_id: &str,
    mut snapshot: awaken_runtime_contract::ExecutableAgentSnapshot,
) -> Result<awaken_runtime_contract::ExecutableAgentSnapshot, RunError> {
    if let Some(model_override) = model_override {
        if let Some(publication) = &model_override.publication {
            publication
                .validate_for_workspace(workspace_id)
                .map_err(|error| {
                    RunError::internal(format!(
                        "frozen Session model override publication is invalid: {error}"
                    ))
                })?;
            snapshot.resolved_spec.model_binding = publication.primary.clone();
            snapshot.resolved_spec.model_candidates = publication.candidates.clone();
        }
        snapshot.resolved_spec.plugin_config.inference = model_override.inference.clone();
    }
    if !system_prompt.is_inherit() {
        snapshot.resolved_spec.instructions = system_prompt
            .resolve(Some(snapshot.resolved_spec.instructions.clone()))
            .unwrap_or_default();
    }
    if model_override.is_some() || !system_prompt.is_inherit() {
        snapshot.recompute_fingerprint().map_err(|error| {
            RunError::internal(format!(
                "Session-local Agent publication is invalid: {error}"
            ))
        })?;
    }
    Ok(snapshot)
}

/// Total decision for binding an immutable Agent publication to one frozen
/// Session baseline. A pinned Worker projection is never allowed to continue
/// without the exact publication; local compatibility projections may omit it,
/// but any publication that is present must match every frozen coordinate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrozenAgentPublicationDecision {
    Unpinned,
    OptionalMissing,
    Exact,
    MissingRequired,
    Mismatch,
}

#[must_use]
const fn frozen_agent_publication_facts(
    has_frozen_revision: bool,
    worker_placement: bool,
    publication_present: bool,
    agent_id_matches: bool,
    source_revision_matches: bool,
    frozen_runtime_is_resolved: bool,
    model_candidate_present: bool,
) -> FrozenAgentPublicationDecision {
    if publication_present {
        if agent_id_matches && source_revision_matches && frozen_runtime_is_resolved {
            FrozenAgentPublicationDecision::Exact
        } else {
            FrozenAgentPublicationDecision::Mismatch
        }
    } else if has_frozen_revision {
        if worker_placement {
            FrozenAgentPublicationDecision::MissingRequired
        } else if frozen_runtime_is_resolved {
            FrozenAgentPublicationDecision::OptionalMissing
        } else {
            FrozenAgentPublicationDecision::Mismatch
        }
    } else if !frozen_runtime_is_resolved {
        FrozenAgentPublicationDecision::Mismatch
    } else if model_candidate_present {
        // A complete Session-local override is a frozen model publication even
        // when an optional Agent snapshot is unavailable. It must not be
        // collapsed into the genuinely publication-free legacy Host case.
        FrozenAgentPublicationDecision::OptionalMissing
    } else {
        FrozenAgentPublicationDecision::Unpinned
    }
}

fn publication_decision_for_coordinates(
    agent_id: &str,
    agent_revision: Option<u64>,
    runtime_placement: crate::SessionRuntimePlacement,
    model_override: Option<&crate::SessionModelOverride>,
    runtime: Option<&str>,
    publication: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
) -> FrozenAgentPublicationDecision {
    let model_candidate = model_override
        .and_then(|model_override| model_override.publication.as_ref())
        .map(|publication| &publication.primary)
        .or_else(|| publication.map(|publication| &publication.resolved_spec.model_binding));
    let frozen_runtime_is_resolved = runtime.is_none_or(|runtime| {
        model_candidate.is_some_and(|candidate| candidate.binding().backend_ref == runtime)
    });
    frozen_agent_publication_facts(
        agent_revision.is_some(),
        runtime_placement == crate::SessionRuntimePlacement::Worker,
        publication.is_some(),
        publication.is_some_and(|publication| publication.root_agent_id.0 == agent_id),
        publication.is_some_and(|publication| {
            agent_revision.is_none_or(|revision| publication.metadata.source.revision == revision)
        }),
        frozen_runtime_is_resolved,
        model_candidate.is_some(),
    )
}

/// Compare a delivered executable publication with the immutable coordinates
/// in the Session baseline. Both Coordinator and Worker call this kernel, so a
/// transport cannot weaken the validation performed before dispatch.
#[must_use]
pub fn frozen_agent_publication_decision(
    baseline: &crate::SessionBaseline,
    publication: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
) -> FrozenAgentPublicationDecision {
    publication_decision_for_coordinates(
        &baseline.agent_id,
        baseline.agent_revision,
        baseline.runtime_placement,
        baseline.model_override.as_ref(),
        baseline.runtime.as_deref(),
        publication,
    )
}

/// Prospective-layout adapter for [`frozen_agent_publication_decision`]. Both
/// current Session baselines and pre-root layouts consume the same fact kernel;
/// protocol/application callers never select a provider independently.
#[must_use]
pub fn frozen_sandbox_layout_publication_decision(
    layout: &crate::SessionSandboxLayout,
    publication: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
) -> FrozenAgentPublicationDecision {
    publication_decision_for_coordinates(
        &layout.agent_id,
        layout.agent_revision,
        layout.runtime_placement,
        layout.model_override.as_ref(),
        layout.runtime.as_deref(),
        publication,
    )
}

/// Compatibility adapter for a pre-baseline local Session. It deliberately has
/// no frozen runtime or model override, so only an exact current Agent
/// publication or the genuinely publication-free Host path is admissible.
#[must_use]
pub fn legacy_agent_publication_decision(
    agent_id: &str,
    publication: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
) -> FrozenAgentPublicationDecision {
    publication_decision_for_coordinates(
        agent_id,
        None,
        crate::SessionRuntimePlacement::Local,
        None,
        None,
        publication,
    )
}

#[cfg(kani)]
#[kani::proof]
fn frozen_worker_agent_publication_is_exact_or_fails_closed() {
    let has_frozen_revision: bool = kani::any();
    let worker_placement: bool = kani::any();
    let publication_present: bool = kani::any();
    let agent_id_matches: bool = kani::any();
    let source_revision_matches: bool = kani::any();
    let runtime_matches: bool = kani::any();
    let model_candidate_present: bool = kani::any();
    let decision = frozen_agent_publication_facts(
        has_frozen_revision,
        worker_placement,
        publication_present,
        agent_id_matches,
        source_revision_matches,
        runtime_matches,
        model_candidate_present,
    );

    if has_frozen_revision && worker_placement {
        assert_eq!(
            decision == FrozenAgentPublicationDecision::Exact,
            publication_present && agent_id_matches && source_revision_matches && runtime_matches
        );
        assert_ne!(decision, FrozenAgentPublicationDecision::OptionalMissing);
    }
    if decision == FrozenAgentPublicationDecision::Exact {
        assert!(publication_present);
        assert!(agent_id_matches && source_revision_matches && runtime_matches);
    }
    if decision == FrozenAgentPublicationDecision::MissingRequired {
        assert!(has_frozen_revision && worker_placement && !publication_present);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn worker_publication_decision_table_is_total_and_fail_closed() {
        // Cause/effect graph: C1 a frozen revision exists; C2 placement is a
        // Worker; C3 a publication is present; C4 its Agent id matches; C5 its
        // source revision matches; C6 its model resolves the frozen Runtime;
        // C7 a model candidate exists. Effects: E1 exact publication; E2 a
        // required publication is missing; E3 legacy Host is unpinned; every
        // remaining combination fails closed or is explicitly optional. The
        // exhaustive truth table below evaluates all 2^7 rules.
        for has_revision in [false, true] {
            for worker in [false, true] {
                for present in [false, true] {
                    for id_matches in [false, true] {
                        for revision_matches in [false, true] {
                            for runtime_matches in [false, true] {
                                for candidate_present in [false, true] {
                                    let decision = frozen_agent_publication_facts(
                                        has_revision,
                                        worker,
                                        present,
                                        id_matches,
                                        revision_matches,
                                        runtime_matches,
                                        candidate_present,
                                    );
                                    let exact = present
                                        && id_matches
                                        && revision_matches
                                        && runtime_matches;
                                    if has_revision && worker {
                                        assert_eq!(
                                            decision == FrozenAgentPublicationDecision::Exact,
                                            exact
                                        );
                                    }
                                    assert_eq!(
                                        decision == FrozenAgentPublicationDecision::MissingRequired,
                                        has_revision && worker && !present
                                    );
                                    assert_eq!(
                                        decision == FrozenAgentPublicationDecision::Unpinned,
                                        !has_revision
                                            && !present
                                            && runtime_matches
                                            && !candidate_present
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn effective_publication_system_prompt_decision_table_is_exact() {
        // Cause/effect graph: C1 selection is Inherit, Clear, or Replace; C2 an
        // Agent publication carries original instructions. Effects: E1 retain
        // them byte-for-byte; E2 emit an empty system instruction; E3 emit only
        // the Session replacement; E4 changed executable semantics recompute a
        // self-consistent fingerprint. Decision rules are the three rows below.
        let base = awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent")
            .model(awaken_runtime_contract::resolved::ModelBinding::new(
                "provider", "model", "genai",
            ))
            .instructions("agent instructions")
            .build();
        for (selection, expected, changed) in [
            (
                crate::SessionSystemPromptSelection::Inherit,
                "agent instructions",
                false,
            ),
            (crate::SessionSystemPromptSelection::Clear, "", true),
            (
                crate::SessionSystemPromptSelection::Replace("session instructions".into()),
                "session instructions",
                true,
            ),
        ] {
            let projected =
                project_effective_agent_publication(None, &selection, "workspace", base.clone())
                    .expect("valid effective publication");
            assert_eq!(projected.resolved_spec.instructions, expected);
            assert_eq!(projected.fingerprint != base.fingerprint, changed);
            assert_eq!(
                projected.fingerprint,
                projected.resolved_spec.catalog_fingerprint
            );
        }
    }

    proptest! {
        #[test]
        fn effective_publication_projection_is_idempotent(prompt in ".{0,128}") {
            // Property design: cause C1 is any valid replacement text, including
            // empty and Unicode; effect E1 is exact replacement and E2 is a
            // second projection producing the identical snapshot/fingerprint.
            // This covers the replay rule that Coordinator and Worker may both
            // observe the same already-effective immutable publication.
            let selection = crate::SessionSystemPromptSelection::Replace(prompt.clone());
            let base = awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent")
                .model(awaken_runtime_contract::resolved::ModelBinding::new(
                    "provider", "model", "genai",
                ))
                .instructions("agent instructions")
                .build();
            let once = project_effective_agent_publication(
                None,
                &selection,
                "workspace",
                base,
            ).expect("valid first projection");
            let twice = project_effective_agent_publication(
                None,
                &selection,
                "workspace",
                once.clone(),
            ).expect("valid replay projection");
            prop_assert_eq!(&once.resolved_spec.instructions, &prompt);
            prop_assert_eq!(twice, once);
        }
    }
}
