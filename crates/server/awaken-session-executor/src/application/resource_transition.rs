//! Finite admission kernel for the Runtime copy of one aggregate-authored
//! Resource transition.
//!
//! The Session aggregate remains the durable generation and amendment
//! authority. The parent application installer owns projection state and
//! effects; this module only classifies whether its current disposable cache
//! may stage, replace, or must reject one complete incoming transition.

/// Resource behavior for the one complete frozen-projection installer.
/// Keeping this typed prevents terminal recovery from entering ordinary
/// materialization while preserving one publication/baseline/lease owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FrozenResourceProjectionMode {
    Dispatch,
    Synchronize,
    ValidateInstalled,
    Terminal,
}

impl FrozenResourceProjectionMode {
    pub(super) const fn synchronizes(self) -> bool {
        matches!(self, Self::Dispatch | Self::Synchronize)
    }

    pub(super) const fn transition_use(
        self,
    ) -> awaken_session_realization_contract::FrozenResourceTransitionUse {
        if self.synchronizes() {
            awaken_session_realization_contract::FrozenResourceTransitionUse::ApplyEffects
        } else {
            awaken_session_realization_contract::FrozenResourceTransitionUse::ValidateInstalled
        }
    }

    pub(super) const fn allows_unattempted_amendment(self) -> bool {
        matches!(self, Self::Dispatch)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FrozenResourceTransitionInstallDecision {
    Stage,
    Replace,
    Reject,
}

/// Decide whether one complete aggregate projection may replace the Runtime's
/// disposable Resource projection. Durable amendment eligibility remains owned
/// by the Session aggregate; this kernel only prevents a claimed realization
/// from projecting different content into the same observed generation.
const fn frozen_resource_generation_install_decision(
    previous_exists: bool,
    exact_replay: bool,
    workspace_matches: bool,
    previous_revision: u64,
    incoming_revision: u64,
    authority_amends_unattempted: bool,
) -> FrozenResourceTransitionInstallDecision {
    use FrozenResourceTransitionInstallDecision::{Reject, Replace, Stage};

    if !previous_exists || exact_replay {
        return Stage;
    }
    if workspace_matches
        && (incoming_revision > previous_revision
            || (authority_amends_unattempted && incoming_revision == previous_revision))
    {
        Replace
    } else {
        Reject
    }
}

/// Project aggregate-owned transition values into the one generation kernel.
/// This adapter contains no independent policy and is the only place where
/// Runtime cache identity is translated into the kernel's complete fact set.
pub(super) fn frozen_resource_transition_install_decision(
    existing: Option<&awaken_session_resource_contract::SessionResourceTransition>,
    incoming: &awaken_session_resource_contract::SessionResourceTransition,
    authority_amends_unattempted: bool,
) -> FrozenResourceTransitionInstallDecision {
    match existing {
        Some(existing) => frozen_resource_generation_install_decision(
            true,
            existing.desired() == incoming.desired(),
            existing.desired().workspace_id == incoming.desired().workspace_id,
            existing.desired().revision,
            incoming.desired().revision,
            authority_amends_unattempted,
        ),
        None => frozen_resource_generation_install_decision(
            false,
            false,
            false,
            0,
            incoming.desired().revision,
            authority_amends_unattempted,
        ),
    }
}

#[cfg(kani)]
/// @obligation session_resources.install_replacement_requires_newer_or_unattempted_authority_amendment
#[kani::proof]
fn session_resource_replacement_requires_newer_or_authority_amended_generation() {
    use FrozenResourceTransitionInstallDecision::{Reject, Replace, Stage};

    let previous_exists: bool = kani::any();
    let exact_replay: bool = kani::any();
    let workspace_matches: bool = kani::any();
    let previous_revision: u64 = kani::any();
    let incoming_revision: u64 = kani::any();
    let mode = match kani::any::<u8>() % 4 {
        0 => FrozenResourceProjectionMode::Dispatch,
        1 => FrozenResourceProjectionMode::Synchronize,
        2 => FrozenResourceProjectionMode::ValidateInstalled,
        _ => FrozenResourceProjectionMode::Terminal,
    };
    let authority_amends_unattempted = mode.allows_unattempted_amendment();
    assert_eq!(
        authority_amends_unattempted,
        mode == FrozenResourceProjectionMode::Dispatch
    );

    let decision = frozen_resource_generation_install_decision(
        previous_exists,
        exact_replay,
        workspace_matches,
        previous_revision,
        incoming_revision,
        authority_amends_unattempted,
    );
    let replacement_authorized = previous_exists
        && !exact_replay
        && workspace_matches
        && (incoming_revision > previous_revision
            || (authority_amends_unattempted && incoming_revision == previous_revision));
    assert_eq!(decision == Replace, replacement_authorized);
    assert_eq!(decision == Stage, !previous_exists || exact_replay);
    assert_eq!(
        decision == Reject,
        !replacement_authorized && previous_exists && !exact_replay
    );
}

#[cfg(test)]
mod tests {
    use super::{
        FrozenResourceTransitionInstallDecision, frozen_resource_transition_install_decision,
    };

    fn transition(
        workspace: &str,
        revision: u64,
        skill: Option<&str>,
    ) -> awaken_session_resource_contract::SessionResourceTransition {
        let resources = awaken_session_resource_contract::ResolvedSessionResources::try_new(
            Vec::new(),
            skill
                .map(
                    |skill_id| awaken_session_resource_contract::ResolvedSkillBinding {
                        kind: awaken_agent_contract::AgentSkillKind::Custom,
                        skill_id: skill_id.into(),
                        version: 1,
                        bundle_sha256: format!("sha256-{skill_id}"),
                    },
                )
                .into_iter()
                .collect(),
        )
        .expect("test Resource projection is valid");
        let manifest = awaken_session_resource_contract::SessionResourceManifest::at_revision(
            workspace, revision, resources,
        );
        awaken_session_resource_contract::SessionResourceTransition::new(manifest.clone(), manifest)
            .expect("test transition belongs to one Workspace")
    }

    fn transition_from(
        previous: awaken_session_resource_contract::SessionResourceManifest,
        desired: awaken_session_resource_contract::SessionResourceManifest,
    ) -> awaken_session_resource_contract::SessionResourceTransition {
        awaken_session_resource_contract::SessionResourceTransition::new(previous, desired)
            .expect("test transition belongs to one Workspace")
    }

    #[test]
    fn complete_projection_install_decision_has_one_authority_table() {
        use FrozenResourceTransitionInstallDecision::{Reject, Replace, Stage};

        // Cause/effect graph: C1 a staged transition exists; C2 incoming desired
        // generation is an exact replay (its prior endpoint may already have
        // advanced after physical completion); C3 Workspace matches; C4 desired revision is newer,
        // equal, or older; C5 the typed caller is Coordinator Dispatch and may
        // project the aggregate's unattempted amendment. Effects are Stage,
        // Replace, or Reject before any Resource read or slot mutation.
        //
        // | Rule | C1 | C2 | C3 | C4 | C5 | Effect |
        // | I1 | F | any | any | any | any | Stage |
        // | I2 | T | T | any | any | any | Stage |
        // | I3 | T | F | T | newer | any | Replace |
        // | I4 | T | F | T | equal | T | Replace |
        // | I5 | T | F | T | equal | F | Reject |
        // | I6 | T | F | T | older | any | Reject |
        // | I7 | T | F | F | any | any | Reject |
        let old = transition_from(
            transition("workspace-a", 8, Some("base")).desired().clone(),
            transition("workspace-a", 9, Some("old")).desired().clone(),
        );
        let exact = old.clone();
        let exact_after_completion =
            transition_from(exact.desired().clone(), exact.desired().clone());
        let newer = transition("workspace-a", 10, Some("new"));
        let amended = transition("workspace-a", 9, Some("amended"));
        let older = transition("workspace-a", 8, Some("older"));
        let foreign = transition("workspace-b", 10, Some("foreign"));
        let rules = [
            (None, &old, false, Stage),
            (Some(&old), &exact, false, Stage),
            (Some(&old), &exact_after_completion, false, Stage),
            (Some(&old), &newer, false, Replace),
            (Some(&old), &amended, true, Replace),
            (Some(&old), &amended, false, Reject),
            (Some(&old), &older, true, Reject),
            (Some(&old), &foreign, true, Reject),
        ];
        for (existing, incoming, authority, expected) in rules {
            assert_eq!(
                frozen_resource_transition_install_decision(existing, incoming, authority),
                expected
            );
        }
    }

    #[test]
    fn terminal_install_retains_pending_generation_without_applying_live_effects() {
        // T1 warm Worker already staged A -> B: complete cleanup projection is
        // an exact replay. T2 cold Worker stages the same transition. T3 old
        // active-only A -> A is rejected. None may enter live Resource effects.
        use FrozenResourceTransitionInstallDecision::{Reject, Stage};
        let active = transition("workspace", 0, None);
        let pending = transition("workspace", 1, Some("pending"));
        let complete = transition_from(active.desired().clone(), pending.desired().clone());
        let mode = super::FrozenResourceProjectionMode::Terminal;
        assert!(!mode.synchronizes());
        assert!(!mode.allows_unattempted_amendment());
        for (installed, incoming, expected) in [
            (Some(&complete), &complete, Stage),
            (None, &complete, Stage),
            (Some(&complete), &active, Reject),
        ] {
            assert_eq!(
                frozen_resource_transition_install_decision(
                    installed,
                    incoming,
                    mode.allows_unattempted_amendment(),
                ),
                expected,
            );
        }
    }
}
