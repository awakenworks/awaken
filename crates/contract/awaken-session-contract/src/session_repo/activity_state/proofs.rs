use super::*;

// A0/A1 partition: any historical u64 scalar, one real Next admission into the
// empty persisted activity set. Exhaustion stutters; otherwise exactly the
// mathematical successor becomes the sole member, with execution unchanged.
// BTreeSet insertion, lookup and Drop are real and all unwinding checks remain.
// This bounded base case does not establish two live receipts, collection
// removal, terminal clearing, SQL durability or application exactly-once close.
#[kani::proof]
#[kani::unwind(4)]
fn activity_next_epoch_inserts_exact_checked_successor() {
    let seed: u64 = kani::any();
    let mut epoch = seed;
    let mut active = BTreeSet::new();
    let mut execution = SessionExecutionState::Idle;
    let actual = ActivityState {
        epoch: &mut epoch,
        active: &mut active,
        execution: &mut execution,
    }
    .begin_activity_epoch();
    if seed == u64::MAX {
        assert!(
            actual.is_none() && epoch == seed && active.is_empty(),
            "N1 exhausted activity is an exact stutter"
        );
    } else {
        let successor = seed + 1;
        assert!(
            actual == Some(successor) && epoch == successor,
            "N2 admission returns and stores the successor"
        );
        assert!(
            active.len() == 1 && active.contains(&successor),
            "N3 admission inserts exactly its successor"
        );
    }
    assert!(
        execution == SessionExecutionState::Idle,
        "N4 admission preserves execution"
    );
}

// E0/E1 partition: arbitrary historical and requested u64 epochs with an empty
// real set. A request is admitted iff strictly above the high-water (therefore
// positive); admission stores/inserts that exact value, rejection stutters.
// This complements the Next base case without assuming uniqueness or replacing
// the real BTreeSet. Multi-member admission/settlement remain separate gaps.
#[kani::proof]
#[kani::unwind(4)]
fn activity_exact_epoch_inserts_only_fresh_positive_member() {
    let seed: u64 = kani::any();
    let requested: u64 = kani::any();
    let mut epoch = seed;
    let mut active = BTreeSet::new();
    let mut execution = SessionExecutionState::Idle;
    let actual = ActivityState {
        epoch: &mut epoch,
        active: &mut active,
        execution: &mut execution,
    }
    .begin_activity_epoch_at(requested);
    assert!(
        actual == (requested > seed),
        "E1 exact admission requires a fresh positive epoch"
    );
    if requested > seed {
        assert!(
            epoch == requested && active.len() == 1 && active.contains(&requested),
            "E2 exact admission stores only its requested member"
        );
    } else {
        assert!(
            epoch == seed && active.is_empty(),
            "E3 rejected exact admission stutters"
        );
    }
    assert!(
        execution == SessionExecutionState::Idle,
        "E4 exact admission preserves execution"
    );
}
