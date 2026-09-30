//! Shared executable decision-table oracles for the production checkpoint owner.
use super::{ErasureAction, ErasureProgress, ErasureTarget};

fn checkpoint(
    completed: [Option<ErasureTarget>; 2],
    revision: u64,
    count: usize,
    flags: [bool; 2],
) -> ErasureProgress {
    ErasureProgress {
        revision,
        completed_targets: completed.into_iter().flatten().collect(),
        records_removed: count,
        accountability_stamped: flags[0],
        complete: flags[1],
    }
}

fn check_selection(
    completed: [Option<ErasureTarget>; 2],
    enabled: [bool; 2],
    revision: u64,
    count: usize,
    flags: [bool; 2],
) {
    use ErasureTarget::{Coordinator, Resources};
    // Causes: C1/C2 adapter bound; C3/C4 target occurs in the checkpoint.
    // Decision table: R1=C1&&!C3 -> Coordinator; R2=!(C1&&!C3)&&C2&&!C4
    // -> Resources; R3=neither -> None. Adapter identity and the full checkpoint
    // are preserved. All 0..2-row lists include reversed and duplicate rows;
    // revision/count/terminal flags are arbitrary frame, not selection inputs.
    let progress = checkpoint(completed, revision, count, flags);
    let before = progress.clone();
    let adapters = [11_u8, 29_u8];
    let actual = progress.pending_target([
        (Coordinator, enabled[0].then_some(&adapters[0])),
        (Resources, enabled[1].then_some(&adapters[1])),
    ]);
    let expected = match (
        enabled[0] && completed[0] != Some(Coordinator) && completed[1] != Some(Coordinator),
        enabled[1] && completed[0] != Some(Resources) && completed[1] != Some(Resources),
    ) {
        (true, _) => Some((Coordinator, Some(&adapters[0]))),
        (false, true) => Some((Resources, Some(&adapters[1]))),
        (false, false) => None,
    };
    assert_eq!(actual, expected);
    assert_eq!(progress, before);
}

fn check_record_effect(
    progress: &mut ErasureProgress,
    before: &ErasureProgress,
    target: ErasureTarget,
    removed: usize,
) -> Result<(), String> {
    // Causes: C1 a bound, uncompleted target is selected; C2 the mathematical
    // count+receipt fits usize. R2=C1&&C2 -> append exactly that target once,
    // exact sum and preserve every other field. R3=C1&&!C2 -> exact error and
    // full stutter. Selection and later reselection remain the caller's oracle.
    // Receipts/counts span usize, including zero/MAX; u128 is the independent
    // arithmetic oracle on the supported <=64-bit usize targets. These are local
    // transitions after a successful effect, not async/CAS/reload refinement.
    let mathematical_sum = (before.records_removed as u128) + (removed as u128);
    let result = progress.record_target(target, removed);
    if mathematical_sum <= usize::MAX as u128 {
        assert!(result.is_ok());
        let mut expected = before.clone();
        expected.records_removed = usize::try_from(mathematical_sum).unwrap();
        expected.completed_targets.push(target);
        assert_eq!(*progress, expected);
        Ok(())
    } else {
        let error = result.unwrap_err().0;
        assert_eq!(*progress, *before);
        Err(error)
    }
}

fn check_selected_count(
    completed: [Option<ErasureTarget>; 2],
    enabled: [bool; 2],
    revision: u64,
    count: usize,
    removed: usize,
    flags: [bool; 2],
) {
    use ErasureTarget::{Coordinator, Resources};
    // R1 no pending target => exact stutter; R2/R3 are owned by the shared
    // record oracle above. On R2 the selected target occurs exactly once and
    // can never be selected a second time. The Rust cross product composes
    // selection and recording; Kani partitions those two independent causes.
    let mut progress = checkpoint(completed, revision, count, flags);
    let before = progress.clone();
    let bindings = [
        (Coordinator, enabled[0].then_some(())),
        (Resources, enabled[1].then_some(())),
    ];
    if let Some((target, Some(()))) = progress.pending_target(bindings) {
        match check_record_effect(&mut progress, &before, target, removed) {
            Ok(()) => {
                assert_eq!(
                    progress
                        .completed_targets
                        .iter()
                        .filter(|item| **item == target)
                        .count(),
                    1
                );
                assert!(
                    progress
                        .pending_target(bindings)
                        .is_none_or(|(next, _)| next != target)
                );
            }
            Err(error) => assert_eq!(error, "erasure receipt overflow"),
        }
    } else {
        assert_eq!(progress, before);
    }
}

#[test]
fn erasure_progress_selection_and_count_decision_table() {
    // Cross product executes R1-R3 above with all target-list/binding/flag
    // combinations; boundary pairs distinguish exact MAX, zero and overflow.
    let targets = [
        None,
        Some(ErasureTarget::Coordinator),
        Some(ErasureTarget::Resources),
    ];
    for first in targets {
        for second in targets {
            for bits in 0_u8..16 {
                let enabled = [bits & 1 != 0, bits & 2 != 0];
                let flags = [bits & 4 != 0, bits & 8 != 0];
                for revision in [0, u64::MAX] {
                    for (count, removed) in [
                        (0, 0),
                        (0, usize::MAX),
                        (usize::MAX, 0),
                        (usize::MAX - 1, 1),
                        (usize::MAX, 1),
                        (1, usize::MAX),
                    ] {
                        check_selection([first, second], enabled, revision, count, flags);
                        check_action([first, second], enabled, revision, count, flags);
                        check_selected_count(
                            [first, second],
                            enabled,
                            revision,
                            count,
                            removed,
                            flags,
                        );
                    }
                }
            }
        }
    }
}

#[cfg(kani)]
fn arbitrary_target() -> Option<ErasureTarget> {
    match kani::any::<u8>() % 3 {
        0 => None,
        1 => Some(ErasureTarget::Coordinator),
        _ => Some(ErasureTarget::Resources),
    }
}

#[cfg(kani)]
fn selected_target_with_other_rows() -> (ErasureTarget, [Option<ErasureTarget>; 2]) {
    use ErasureTarget::{Coordinator, Resources};
    let (target, other) = if kani::any::<bool>() {
        (Coordinator, Resources)
    } else {
        (Resources, Coordinator)
    };
    (
        target,
        [
            kani::any::<bool>().then_some(other),
            kani::any::<bool>().then_some(other),
        ],
    )
}

#[cfg(kani)]
/// @obligation erasure.each_target_is_counted_once
/// @kani profile=erasure-progress-selection-v1 tier=nightly
#[kani::proof]
#[kani::unwind(8)]
fn erasure_progress_selects_exact_first_pending_target() {
    check_selection(
        [arbitrary_target(), arbitrary_target()],
        kani::any(),
        kani::any(),
        kani::any(),
        kani::any(),
    );
}

#[cfg(kani)]
/// @obligation erasure.each_target_is_counted_once
/// @kani tier=nightly
#[kani::proof]
#[kani::unwind(8)]
fn erasure_progress_bounded_count_adds_selected_target_once() {
    // C1 selected Coordinator with empty predecessor; C2 independent u16
    // count/receipt; C3 arbitrary revision and flags. E1 exact sum, append
    // and frame preservation. Rule C1+C2+C3=>E1; overflow is impossible in
    // this row and belongs to the near-MAX proof below.
    let count = kani::any::<u16>() as usize;
    let removed = kani::any::<u16>() as usize;
    let mut progress = checkpoint([None, None], kani::any(), count, kani::any());
    let before = progress.clone();
    assert!(
        check_record_effect(&mut progress, &before, ErasureTarget::Coordinator, removed,).is_ok()
    );
}

#[cfg(kani)]
/// @obligation erasure.each_target_is_counted_once
/// @kani tier=nightly
#[kani::proof]
#[kani::unwind(8)]
fn erasure_progress_near_max_count_preserves_exact_overflow_boundary() {
    // C1 selected Coordinator with empty predecessor; C2 independent u16
    // deficit below usize::MAX and u16 receipt; C3 arbitrary frame. E1 append
    // iff receipt <= deficit; E2 otherwise exact overflow error/stutter.
    // Rule C1+C2+C3+fits=>E1; !fits=>E2. Rust's cross table also exercises
    // the zero/MAX extremes through the same production method.
    let deficit = kani::any::<u16>() as usize;
    let removed = kani::any::<u16>() as usize;
    let mut progress = checkpoint([None, None], kani::any(), usize::MAX - deficit, kani::any());
    let before = progress.clone();
    assert_eq!(
        check_record_effect(&mut progress, &before, ErasureTarget::Coordinator, removed).is_ok(),
        removed <= deficit,
    );
}

#[cfg(kani)]
/// @obligation erasure.each_target_is_counted_once
/// @kani tier=nightly
#[kani::proof]
#[kani::unwind(8)]
fn erasure_progress_record_preserves_other_target_rows_and_flags() {
    // C1 either selected target with zero, one or two other-target rows; C2
    // fitting zero receipt or overflowing MAX+1; C3 arbitrary revision/flags.
    // E1 success appends only the selected target and preserves all frame
    // fields; E2 overflow leaves every byte unchanged. Selection for all
    // binding/list combinations is the separate existing Kani proof.
    let (target, completed) = selected_target_with_other_rows();
    let (count, removed, expected_success) = if kani::any::<bool>() {
        (0, 0, true)
    } else {
        (usize::MAX, 1, false)
    };
    let mut progress = checkpoint(completed, kani::any(), count, kani::any());
    let before = progress.clone();
    assert_eq!(
        check_record_effect(&mut progress, &before, target, removed).is_ok(),
        expected_success,
    );
}

#[cfg(kani)]
/// @obligation erasure.each_target_is_counted_once
/// @kani tier=nightly
#[kani::proof]
#[kani::unwind(8)]
fn erasure_progress_recorded_target_is_not_selected_again() {
    // C1 one selected target and zero, one or two other-target rows; C2 its
    // effect succeeds with a zero receipt. E1 the target is selected before
    // recording; E2 the same binding cannot select it after the append, even
    // when the completed vector now has three rows. Rule C1+C2=>E1+E2.
    // All binding/list selection cases remain in the original selection proof.
    use ErasureTarget::{Coordinator, Resources};
    let (target, completed) = selected_target_with_other_rows();
    let bindings = [
        (Coordinator, (target == Coordinator).then_some(())),
        (Resources, (target == Resources).then_some(())),
    ];
    let mut progress = checkpoint(completed, 0, 0, [false, false]);
    assert_eq!(
        progress.pending_target(bindings).map(|(item, _)| item),
        Some(target)
    );
    let before = progress.clone();
    assert!(check_record_effect(&mut progress, &before, target, 0).is_ok());
    assert!(progress.pending_target(bindings).is_none());
}

fn check_action(
    completed: [Option<ErasureTarget>; 2],
    enabled: [bool; 2],
    revision: u64,
    count: usize,
    flags: [bool; 2],
) {
    use ErasureTarget::{Coordinator, Resources};
    // Causes: C1 complete, C2 stamped, C3 any currently bound target missing.
    // Decision table: R1=C1&&!C2 OR (C1||C2)&&C3 -> invalid/stutter before any
    // replay/effect. R2=valid&&C1 -> Replay; R3=valid&&!C1&&C3 -> first missing
    // target with exact adapter; R4=valid&&!C1&&!C3&&!C2 -> Stamp;
    // R5=valid&&!C1&&!C3&&C2 -> Complete. No assumptions remove corrupt inputs:
    // all flags, duplicate/reversed/empty 0..2-row lists and bindings are covered.
    // Every decision preserves the entire checkpoint. Adapter tokens are borrowed
    // bytes; async effects, CAS and persisted/physical truth remain separate.
    let progress = checkpoint(completed, revision, count, flags);
    let before = progress.clone();
    let adapters = [11_u8, 29_u8];
    let actual = progress.next_action([
        (Coordinator, enabled[0].then_some(&adapters[0])),
        (Resources, enabled[1].then_some(&adapters[1])),
    ]);
    let missing = [
        enabled[0] && completed[0] != Some(Coordinator) && completed[1] != Some(Coordinator),
        enabled[1] && completed[0] != Some(Resources) && completed[1] != Some(Resources),
    ];
    let stamped = flags[0];
    let complete = flags[1];
    let any_missing = missing[0] || missing[1];
    let expected = match (complete, stamped, any_missing) {
        (true, false, _) | (_, true, true) => ErasureAction::InvalidCheckpoint,
        (true, true, false) => ErasureAction::Replay,
        (false, false, true) if missing[0] => ErasureAction::Erase(Coordinator, &adapters[0]),
        (false, false, true) => ErasureAction::Erase(Resources, &adapters[1]),
        (false, false, false) => ErasureAction::Stamp,
        (false, true, false) => ErasureAction::Complete,
    };
    assert_eq!(actual, expected);
    assert_eq!(progress, before);
    if matches!(
        actual,
        ErasureAction::Stamp | ErasureAction::Complete | ErasureAction::Replay
    ) {
        assert!(!any_missing);
    }
    if matches!(actual, ErasureAction::Complete | ErasureAction::Replay) {
        assert!(stamped);
    }
}

#[cfg(kani)]
/// @obligation erasure.accountability_stamp_follows_all_targets
/// @obligation erasure.completion_requires_accountability_stamp
/// @kani profile=erasure-progress-selection-v1 tier=nightly
#[kani::proof]
#[kani::unwind(8)]
fn erasure_checkpoint_action_requires_all_targets_and_accountability() {
    check_action(
        [arbitrary_target(), arbitrary_target()],
        kani::any(),
        kani::any(),
        kani::any(),
        kani::any(),
    );
}

fn check_successor(
    old: ([Option<ErasureTarget>; 2], u64, usize, [bool; 2]),
    new: ([Option<ErasureTarget>; 2], u64, usize, [bool; 2]),
    present: bool,
    expected_revision: Option<u64>,
) {
    use ErasureTarget::{Coordinator, Resources};
    // Causes: C1 old row present; C2 caller expected present/equal/stale/future;
    // C3 candidate revision zero/exact/equal/jump/wrap; C4 each old target occurs
    // in the candidate. Rules S1 absent+None+zero -> accept; S2 present+exact
    // expected+representable successor+target superset -> accept; S3 every other
    // combination -> reject. Duplicate/reordered rows use set membership, not
    // vector equality or multiplicity. Counts and both flags are arbitrary
    // independent frame, not additional admission rules. No assumptions exclude
    // regressive candidates or exhaustion. Admission never mutates either row.
    let current = present.then(|| checkpoint(old.0, old.1, old.2, old.3));
    let candidate = checkpoint(new.0, new.1, new.2, new.3);
    // The inputs contain at most two rows. Capture every field and ordered row
    // without cloning heap allocations solely for the frame assertion. This
    // retains exact duplicate/order checks, and real input allocation/Drop.
    let frame = |row: &ErasureProgress| {
        assert!(row.completed_targets.len() <= 2);
        (
            row.revision,
            row.records_removed,
            row.accountability_stamped,
            row.complete,
            row.completed_targets.len(),
            row.completed_targets.first().copied(),
            row.completed_targets.get(1).copied(),
        )
    };
    let before = (current.as_ref().map(&frame), frame(&candidate));
    let old_members = [
        old.0[0] == Some(Coordinator) || old.0[1] == Some(Coordinator),
        old.0[0] == Some(Resources) || old.0[1] == Some(Resources),
    ];
    let new_members = [
        new.0[0] == Some(Coordinator) || new.0[1] == Some(Coordinator),
        new.0[0] == Some(Resources) || new.0[1] == Some(Resources),
    ];
    let preserves = (!old_members[0] || new_members[0]) && (!old_members[1] || new_members[1]);
    let expected = match (present, expected_revision) {
        (false, None) => new.1 == 0,
        (true, Some(revision)) => {
            revision == old.1 && u128::from(new.1) == u128::from(revision) + 1 && preserves
        }
        _ => false,
    };
    let actual = candidate.follows(expected_revision, current.as_ref());
    assert_eq!(actual, expected);
    if actual && present {
        assert!(preserves);
        assert!(new.1 > old.1);
    }
    assert_eq!((current.as_ref().map(&frame), frame(&candidate)), before);
}

#[test]
fn erasure_checkpoint_successor_decision_table() {
    // Execute S1-S3 across every 0..2-row list pair (duplicates/reversal), all
    // old/expected presence and flag combinations. The revision table includes
    // zero/create, exact, stale/future, equal/jump, MAX-1->MAX and MAX exhaustion.
    let values = [
        None,
        Some(ErasureTarget::Coordinator),
        Some(ErasureTarget::Resources),
    ];
    for a in values {
        for b in values {
            for c in values {
                for d in values {
                    for bits in 0_u8..64 {
                        for (old, expected, new) in [
                            (0, 0, 0),
                            (0, 0, 1),
                            (0, 0, 2),
                            (1, 0, 1),
                            (0, 1, 2),
                            (1, 1, 2),
                            (u64::MAX - 1, u64::MAX - 1, u64::MAX),
                            (u64::MAX, u64::MAX, 0),
                            (u64::MAX, u64::MAX, u64::MAX),
                        ] {
                            check_successor(
                                ([a, b], old, usize::MAX, [bits & 4 != 0, bits & 8 != 0]),
                                ([c, d], new, 0, [bits & 16 != 0, bits & 32 != 0]),
                                bits & 1 != 0,
                                (bits & 2 != 0).then_some(expected),
                            );
                        }
                    }
                }
            }
        }
    }
}

// S1-S3 retain every symbolic list/revision/presence/frame input. At most
// two target rows and two flags need three loop steps including exit. Keep
// unwinding assertions and real allocation/Drop enabled: insufficient bounds
// must fail, not silently remove paths. Full-frame capture above avoids copies
// made only for the oracle; production follows is unchanged.
#[cfg(kani)]
/// @obligation erasure.checkpoint_never_forgets_completed_targets
/// @kani profile=erasure-checkpoint-successor-v1
#[kani::proof]
#[kani::unwind(3)]
fn erasure_checkpoint_successor_preserves_all_completed_targets() {
    check_successor(
        (
            [arbitrary_target(), arbitrary_target()],
            kani::any(),
            kani::any(),
            kani::any(),
        ),
        (
            [arbitrary_target(), arbitrary_target()],
            kani::any(),
            kani::any(),
            kani::any(),
        ),
        kani::any(),
        kani::any(),
    );
}
