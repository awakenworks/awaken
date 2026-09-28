// @formal entrypoint agent_profile.effective_publication_field_replacement_is_exact_and_noop_preserving
// @formal entrypoint agent_profile.worker_projection_requires_exact_frozen_publication
//! Canonical projection and validation of the Agent publication frozen into a Session.

use crate::RunError;

/// Report semantic changes while retaining the authoritative value on a no-op.
fn replace_if_changed<T: PartialEq>(current: &mut T, next: T) -> bool {
    if *current == next {
        false
    } else {
        *current = next;
        true
    }
}

#[cfg(kani)]
/// @obligation agent_profile.effective_publication_field_replacement_is_exact_and_noop_preserving
#[kani::proof]
fn effective_publication_field_replacement_is_exact_and_noop_preserving() {
    // The actual generic field updater, instantiated over all u64 payloads.
    // Container/String equality and hash/serialization remain their own TCB.
    let initial: u64 = kani::any();
    let incoming: u64 = kani::any();
    let mut current = initial;
    let changed = replace_if_changed(&mut current, incoming);
    assert_eq!(changed, initial != incoming);
    assert_eq!(current, incoming);
    assert!(!replace_if_changed(&mut current, incoming));
    assert_eq!(current, incoming);
}

/// Project every Session-owned Agent override onto one immutable publication.
///
/// This is the only lowering rule for Session-local model, inference, and
/// system-prompt selection. The Resource manifest owns the effective Skill
/// pins. Coordinator publication, co-located execution, and
/// cold Worker recovery must reuse the returned snapshot; Runtime adapters may
/// validate its frozen coordinates but must not reconstruct it field by field.
pub fn project_effective_agent_publication(
    model_override: Option<&crate::SessionModelOverride>,
    system_prompt: &crate::SessionSystemPromptSelection,
    skill_pins: Option<&[awaken_session_resource_contract::ResolvedSkillBinding]>,
    workspace_id: &str,
    mut snapshot: awaken_runtime_contract::ExecutableAgentSnapshot,
) -> Result<awaken_runtime_contract::ExecutableAgentSnapshot, RunError> {
    let mut changed = false;
    if let Some(model_override) = model_override {
        if let Some(publication) = &model_override.publication {
            publication
                .validate_for_workspace(workspace_id)
                .map_err(|error| {
                    RunError::internal(format!(
                        "frozen Session model override publication is invalid: {error}"
                    ))
                })?;
            changed |= replace_if_changed(
                &mut snapshot.resolved_spec.model_binding,
                publication.primary.clone(),
            );
            changed |= replace_if_changed(
                &mut snapshot.resolved_spec.model_candidates,
                publication.candidates.clone(),
            );
        }
        changed |= replace_if_changed(
            &mut snapshot.resolved_spec.plugin_config.inference,
            model_override.inference.clone(),
        );
    }
    if !system_prompt.is_inherit() {
        let instructions = system_prompt
            .resolve(Some(snapshot.resolved_spec.instructions.clone()))
            .unwrap_or_default();
        changed |= replace_if_changed(&mut snapshot.resolved_spec.instructions, instructions);
    }
    if let Some(pins) = skill_pins {
        let skills = pins
            .iter()
            .map(|pin| awaken_agent_contract::AgentSkillBinding {
                kind: pin.kind,
                skill_id: pin.skill_id.clone(),
                version: pin.version.to_string(),
            })
            .collect();
        changed |= replace_if_changed(
            &mut snapshot.resolved_spec.plugin_config.agent.skills,
            skills,
        );
    }
    if changed {
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
        model_candidate.is_some_and(|candidate| candidate.binding().identity().2 == runtime)
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
/// @obligation agent_profile.worker_projection_requires_exact_frozen_publication
/// @proof-input-debt free_symbol frozen_agent_publication_facts agent_id_matches
/// @proof-input-debt free_symbol frozen_agent_publication_facts has_frozen_revision
/// @proof-input-debt free_symbol frozen_agent_publication_facts model_candidate_present
/// @proof-input-debt free_symbol frozen_agent_publication_facts publication_present
/// @proof-input-debt free_symbol frozen_agent_publication_facts source_revision_matches
/// @proof-input-debt free_symbol frozen_agent_publication_facts worker_placement
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
    fn effective_publication_equal_overrides_preserve_the_complete_authoritative_source() {
        // Causes: P1 explicit equal model/inference roster; P2 replacing a
        // prompt with the same bytes (or clearing already-empty instructions);
        // P3 explicit empty Skills over an already-empty selection. N1-N5
        // independently set each no-op and N6 combines them => exact original
        // snapshot, including its authoritative consistency token/metadata.
        // Merely specifying an override cannot mint a new publication identity.
        let mut metadata = awaken_runtime_contract::AgentSnapshotMetadata::default();
        metadata.source.revision = 7;
        let base = awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent")
            .model(
                awaken_runtime_contract::resolved::ModelBinding::parse("test", "model", "genai")
                    .unwrap(),
            )
            .fingerprint("authoritative-publication-7")
            .metadata(metadata)
            .build();
        let equal_model = crate::SessionModelOverride {
            publication: Some(Box::new(
                awaken_session_configuration_contract::SessionModelPublication {
                    primary: base.resolved_spec.model_binding.clone(),
                    candidates: base.resolved_spec.model_candidates.clone(),
                },
            )),
            inference: base.resolved_spec.plugin_config.inference.clone(),
        };
        for (rule, model, prompt, skills) in [
            (
                "N1",
                None,
                crate::SessionSystemPromptSelection::Inherit,
                None,
            ),
            (
                "N2",
                Some(equal_model.clone()),
                crate::SessionSystemPromptSelection::Inherit,
                None,
            ),
            ("N3", None, crate::SessionSystemPromptSelection::Clear, None),
            (
                "N4",
                None,
                crate::SessionSystemPromptSelection::Replace(String::new()),
                None,
            ),
            (
                "N5",
                None,
                crate::SessionSystemPromptSelection::Inherit,
                Some(&[][..]),
            ),
            (
                "N6",
                Some(equal_model.clone()),
                crate::SessionSystemPromptSelection::Clear,
                Some(&[][..]),
            ),
        ] {
            let projected = project_effective_agent_publication(
                model.as_ref(),
                &prompt,
                skills,
                "workspace",
                base.clone(),
            )
            .unwrap();
            assert_eq!(projected, base, "{rule} complete no-op, no second identity");
        }
        // C1-C3 independently change only primary, fallback roster or inference
        // controls. Each must replace exactly that payload and recompute all
        // three existing fingerprint coordinates; adjacent tests own prompt
        // and Skill changes. These positive witnesses forbid an all-no-op fix.
        for axis in ["primary", "candidates", "inference"] {
            let mut model = equal_model.clone();
            let next = awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                awaken_runtime_contract::resolved::ModelBinding::parse(
                    "next-provider",
                    "next-model",
                    "genai",
                )
                .unwrap(),
            );
            match axis {
                "primary" => model.publication.as_mut().unwrap().primary = next,
                "candidates" => model.publication.as_mut().unwrap().candidates = vec![next],
                "inference" => {
                    model.inference.effort =
                        Some(awaken_runtime_contract::agent_bindings::ReasoningEffort::High)
                }
                _ => unreachable!(),
            }
            let projected = project_effective_agent_publication(
                Some(&model),
                &crate::SessionSystemPromptSelection::Inherit,
                Some(&[]),
                "workspace",
                base.clone(),
            )
            .unwrap();
            let publication = model.publication.as_ref().unwrap();
            assert_eq!(
                projected.resolved_spec.model_binding, publication.primary,
                "C/{axis} exact primary"
            );
            assert_eq!(
                projected.resolved_spec.model_candidates, publication.candidates,
                "C/{axis} exact roster"
            );
            assert_eq!(
                projected.resolved_spec.plugin_config.inference, model.inference,
                "C/{axis} exact inference"
            );
            assert_ne!(
                projected.fingerprint, base.fingerprint,
                "C/{axis} new content identity"
            );
            assert_eq!(
                projected.fingerprint, projected.resolved_spec.catalog_fingerprint,
                "C/{axis}"
            );
            assert_eq!(
                projected.fingerprint.0, projected.metadata.fingerprint.0,
                "C/{axis}"
            );
        }
    }

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
            let projected = project_effective_agent_publication(
                None,
                &selection,
                None,
                "workspace",
                base.clone(),
            )
            .expect("valid effective publication");
            assert_eq!(projected.resolved_spec.instructions, expected);
            assert_eq!(projected.fingerprint != base.fingerprint, changed);
            assert_eq!(
                projected.fingerprint,
                projected.resolved_spec.catalog_fingerprint
            );
        }
    }

    #[test]
    fn effective_publication_uses_only_the_frozen_resource_skill_pins() {
        // Causes: C1 a direct/legacy caller has no Resource projection; C2 a
        // Managed Session pins no Skills after `skills: []`; C3 it pins one
        // exact replacement version; C4 the published Agent selects a Skill.
        // Effects: E1 C1 inherits; E2 C2 clears the published selection; E3
        // C3 selects the pinned version; E4 replay preserves the fingerprint.
        // Rules K1=C1+C4, K2=C2+C4, K3=C3+C4.
        let published = awaken_agent_contract::AgentSkillBinding {
            kind: awaken_agent_contract::AgentSkillKind::Custom,
            skill_id: "published".into(),
            version: "1".into(),
        };
        let pin = awaken_session_resource_contract::ResolvedSkillBinding {
            kind: awaken_agent_contract::AgentSkillKind::Custom,
            skill_id: "replacement".into(),
            version: 2,
            bundle_sha256: "sha256:replacement".into(),
        };
        let mut base = awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent")
            .model(
                awaken_runtime_contract::resolved::ModelBinding::parse(
                    "provider", "model", "genai",
                )
                .expect("complete model binding"),
            )
            .build();
        base.resolved_spec.plugin_config.agent.skills = vec![published.clone()];
        base.recompute_fingerprint().expect("valid published Skill");
        let inherit = crate::SessionSystemPromptSelection::Inherit;
        for (pins, expected, changed) in [
            (None, vec![published], false),
            (Some(Vec::new()), vec![], true),
            (
                Some(vec![pin]),
                vec![awaken_agent_contract::AgentSkillBinding {
                    kind: awaken_agent_contract::AgentSkillKind::Custom,
                    skill_id: "replacement".into(),
                    version: "2".into(),
                }],
                true,
            ),
        ] {
            let projected = project_effective_agent_publication(
                None,
                &inherit,
                pins.as_deref(),
                "workspace",
                base.clone(),
            )
            .expect("valid Resource Skill selection");
            assert_eq!(projected.resolved_spec.plugin_config.agent.skills, expected);
            assert_eq!(projected.fingerprint != base.fingerprint, changed);
            assert_eq!(
                projected.fingerprint,
                projected.resolved_spec.catalog_fingerprint
            );
            let replayed = project_effective_agent_publication(
                None,
                &inherit,
                pins.as_deref(),
                "workspace",
                projected.clone(),
            )
            .expect("valid replay");
            assert_eq!(replayed, projected);
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
                None,
                "workspace",
                base,
            ).expect("valid first projection");
            let twice = project_effective_agent_publication(
                None,
                &selection,
                None,
                "workspace",
                once.clone(),
            ).expect("valid replay projection");
            prop_assert_eq!(&once.resolved_spec.instructions, &prompt);
            prop_assert_eq!(twice, once);
        }
    }
}
