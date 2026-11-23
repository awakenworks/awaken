use super::*;
use crate::test_support::registration;

fn large_registration(revision: u64) -> ExecutableAgentRegistration {
    let mut value = registration(revision, &format!("fp-{revision}"));
    value.session_profile.system = Some("retained immutable publication ".repeat(2048));
    value
}

fn catalog_with_history(count: u64) -> ExecutableAgentCatalog {
    let mut state = CatalogState::default();
    for revision in 1..=count {
        ExecutableAgentCatalog::register_locked(&mut state, large_registration(revision)).unwrap();
    }
    let catalog = ExecutableAgentCatalog::new();
    *catalog.write_state().unwrap() = state;
    catalog
}

// Differential oracle: the complete durable-projection transaction snapshot.
// Point previews select fewer keys but use the same transition owner as writes.
fn full_transition_state(state: &CatalogState) -> CatalogState {
    state.clone_for_projection_transaction()
}

#[test]
fn registration_preview_is_bounded_and_equivalent_with_unrelated_history() {
    // Cause graph: C1 history absent/present; C2 command is new/exact replay.
    // Effects: E1 unchanged registration outcome; E2 at most one entry per
    // touched index, independent of history volume; E3 no catalog mutation.
    // Decision rules: R1 !C1+new -> RegisteredCurrent; R2 C1+replay ->
    // AlreadyRegistered; R3 C1+new -> RegisteredCurrent. An exact replay
    // requires existing history, so !C1+replay is infeasible. All feasible
    // rules require E2+E3, including large payloads.
    for (count, revision, expected_outcome) in [
        (0, 1, ExecutableAgentRegistrationOutcome::RegisteredCurrent),
        (64, 1, ExecutableAgentRegistrationOutcome::AlreadyRegistered),
        (
            64,
            65,
            ExecutableAgentRegistrationOutcome::RegisteredCurrent,
        ),
    ] {
        let catalog = catalog_with_history(count);
        let command = large_registration(revision);
        let before = catalog.current("workspace-a", "agent-a");
        let mut full = {
            let state = catalog.read_state().unwrap();
            let view = state.registration_preview_state(&command);
            assert!(view.current.len() <= 1);
            assert!(view.revisions.len() <= 1);
            assert!(view.fingerprints.len() <= 1);
            assert!(view.time_amendments.len() <= 1);
            full_transition_state(&state)
        };
        let expected = ExecutableAgentCatalog::register_locked(&mut full, command.clone());
        assert_eq!(catalog.preview_registration(command), expected);
        assert_eq!(expected.unwrap(), expected_outcome);
        assert_eq!(catalog.current("workspace-a", "agent-a"), before);
        assert_eq!(
            catalog.read_state().unwrap().revisions.len(),
            count as usize
        );
    }
}

#[test]
fn point_preview_preserves_conflicts_scope_and_legacy_amendment() {
    // C1 same revision with conflicting profile; C2 a new revision collides
    // with an existing fingerprint; C3 exact legacy publication gains its
    // paired time/sequence witness; C4 otherwise identical identity is in a
    // different Workspace. R1/R2 -> Conflict, R3 -> exact replay/amendment,
    // R4 -> independent current. Every preview equals the full transition and
    // must leave the original history/time witness unchanged.
    let catalog = catalog_with_history(64);
    let mut profile_conflict = large_registration(1);
    profile_conflict.session_profile.system = Some("conflicting profile".into());
    let fingerprint_conflict = registration(65, "fp-1");
    let mut amended = large_registration(1);
    amended.published_at_unix_ms = Some(100);
    amended.publication_sequence = Some(1);
    amended.publication_order_verified = true;
    let mut foreign_scope = large_registration(1);
    foreign_scope.workspace_id = "workspace-b".into();
    let commands = [
        profile_conflict,
        fingerprint_conflict,
        amended,
        foreign_scope,
    ];
    for (index, command) in commands.into_iter().enumerate() {
        let mut full = full_transition_state(&catalog.read_state().unwrap());
        let expected = ExecutableAgentCatalog::register_locked(&mut full, command.clone());
        assert_eq!(catalog.preview_registration(command), expected);
        match index {
            0 | 1 => assert!(matches!(
                expected,
                Err(ExecutableAgentRegistrationError::Conflict(_))
            )),
            2 => assert_eq!(
                expected.unwrap(),
                ExecutableAgentRegistrationOutcome::AlreadyRegistered
            ),
            3 => assert_eq!(
                expected.unwrap(),
                ExecutableAgentRegistrationOutcome::RegisteredCurrent
            ),
            _ => unreachable!(),
        }
    }
    assert_eq!(
        catalog
            .at_revision("workspace-a", "agent-a", 1)
            .unwrap()
            .publication_sequence,
        None
    );
    assert!(catalog.current("workspace-b", "agent-a").is_none());
    assert_eq!(
        catalog
            .current("workspace-a", "agent-a")
            .unwrap()
            .source_revision,
        64
    );
}

#[test]
fn withdrawal_preview_uses_only_the_current_lifecycle_entry() {
    // Causes: C1 current available/tombstoned; C2 withdrawal lower/equal/higher
    // than lifecycle; C3 invalid zero revision. All combinations preserve the
    // existing monotonic transition/error, retain exact history, and copy zero
    // revision/fingerprint payloads. No preview publishes a tombstone.
    for already_withdrawn in [false, true] {
        let catalog = catalog_with_history(64);
        if already_withdrawn {
            ExecutableAgentCatalog::withdraw_locked(
                &mut catalog.write_state().unwrap(),
                ExecutableAgentWithdrawal {
                    workspace_id: "workspace-a".into(),
                    agent_id: "agent-a".into(),
                    lifecycle_revision: 65,
                },
            )
            .unwrap();
        }
        let before = catalog.current("workspace-a", "agent-a");
        for revision in [0, 63, 64, 65, 66] {
            let command = ExecutableAgentWithdrawal {
                workspace_id: "workspace-a".into(),
                agent_id: "agent-a".into(),
                lifecycle_revision: revision,
            };
            let view = catalog
                .read_state()
                .unwrap()
                .withdrawal_preview_state(&command);
            assert!(view.current.len() <= 1);
            assert!(view.revisions.is_empty());
            assert!(view.fingerprints.is_empty());
            let mut full = full_transition_state(&catalog.read_state().unwrap());
            let expected = ExecutableAgentCatalog::withdraw_locked(&mut full, command.clone());
            assert_eq!(catalog.preview_withdrawal(command), expected);
            assert_eq!(catalog.current("workspace-a", "agent-a"), before);
            assert_eq!(catalog.read_state().unwrap().revisions.len(), 64);
        }
    }
}
