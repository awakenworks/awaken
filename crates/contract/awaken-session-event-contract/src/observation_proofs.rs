use super::*;

/// @obligation session_event_batch.processed_requires_commit_observation_activity_and_dispatch_settlement
/// @obligation session_time.processed_event_retains_first_time_and_rejects_atomically
#[kani::proof]
fn processed_event_time_is_first_observation_or_atomic_rejection() {
    // E1 pending+unset/equal anchor -> stamp; E2 processed+exact anchor -> inert,
    // including legacy unknown time; E3 changed established anchor, including
    // pending committed input -> unchanged rejection. All u64 clocks,
    // cursors and optional markers are symbolic. Batch lookup/operation identity
    // is tested separately; this calls the production shared metadata reducer.
    let before_processed: bool = kani::any();
    let before_time = kani::any::<bool>().then(kani::any::<u64>);
    let before_anchor = kani::any::<bool>().then(|| SessionEventProjectionAnchor {
        source_commit_cursor: kani::any(),
    });
    let anchor = kani::any::<bool>().then(|| SessionEventProjectionAnchor {
        source_commit_cursor: kani::any(),
    });
    let now: u64 = kani::any();
    // The same symbolic before-state also covers the non-completing input
    // observation: no process-local clock participates and a legacy processed
    // receipt can never gain a late anchor. Any existing anchor is immutable.
    let incoming = SessionEventProjectionAnchor {
        source_commit_cursor: kani::any(),
    };
    let mut input_anchor = before_anchor;
    let anchored = anchor_committed_observation(before_processed, &mut input_anchor, incoming);
    if before_anchor == Some(incoming) {
        assert_eq!(anchored, Ok(false));
        assert_eq!(input_anchor, before_anchor);
    } else if before_anchor.is_some() || before_processed {
        assert!(anchored.is_err());
        assert_eq!(input_anchor, before_anchor);
    } else {
        assert_eq!(anchored, Ok(true));
        assert_eq!(input_anchor, Some(incoming));
    }
    let (mut processed, mut time, mut recorded) = (before_processed, before_time, before_anchor);
    let result = mark_processed_observation(&mut processed, &mut recorded, &mut time, anchor, now);
    if before_processed || (before_anchor.is_some() && before_anchor != anchor) {
        assert_eq!(processed, before_processed);
        assert_eq!(time, before_time);
        assert_eq!(recorded, before_anchor);
        if before_processed && before_anchor == anchor {
            assert_eq!(result, Ok(false));
        } else {
            assert!(result.is_err());
        }
    } else {
        assert_eq!(result, Ok(true));
        assert!(processed);
        assert_eq!(time, Some(now));
        assert_eq!(recorded, anchor);
    }
}
