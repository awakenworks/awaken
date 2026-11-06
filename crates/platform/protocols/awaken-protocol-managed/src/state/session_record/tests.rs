// @formal evidence session.runtime_projection_equal_source_and_content_is_a_read_without_cache_write_or_broadcast
// @formal evidence session.runtime_projection_stale_writer_never_rolls_back_visible_result
// @formal evidence session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
//! Closed cause/effect table for the actual publication decision and owned maps.
use super::*;
use std::cmp::Ordering::{self, Equal, Greater, Less};

// (current, candidate, independently declared ordering), including unsigned
// boundaries. A numeric version never stands in for child identity/presence.
const SCALARS: [(u64, u64, Ordering); 6] = [
    (0, 0, Equal),
    (0, u64::MAX, Greater),
    (u64::MAX, 0, Less),
    (u64::MAX, u64::MAX, Equal),
    (1, 0, Less),
    (1, u64::MAX, Greater),
];
const CHILDREN: [Option<u64>; 3] = [None, Some(0), Some(u64::MAX)];
const CHILD_ORDER: [[Ordering; 3]; 3] = [
    [Equal, Greater, Greater],
    [Less, Equal, Greater],
    [Less, Less, Equal],
];

fn children(code: usize) -> HashMap<String, u64> {
    [("child-a", code % 3), ("child-b", code / 3)]
        .into_iter()
        .filter_map(|(id, index)| CHILDREN[index].map(|version| (id.into(), version)))
        .collect()
}

#[test]
fn projection_publish_decision_covers_cache_and_source_fences() {
    // Causes: C1 exact/stale cache CAS; C2 each of four independent scalar
    // clocks regresses/equal/advances at 0/MAX; C3 each of two child identities
    // is absent/zero/MAX in each map; C4 rendered content unchanged/changed.
    // R1 !C1 -> RetryStaleCache regardless of C2-C4 (precedence matters).
    // R2 C1 + any regression (including removal of a zero-version child) ->
    // RejectSourceRegression. R3 C1 + exact source + !C4 -> AlreadyCurrent.
    // R4 C1 + no regression + (source differs or C4) -> Apply.
    // The full Cartesian table includes mixed clock directions, renamed child
    // identities, zero-valued addition/removal, and content-only changes. Its
    // declared ordering tables own the expected result, not a second reducer.
    // These 419904 real HashMap/String calls replace the old six examples and
    // the redundant two-call simulated writer order. Actual publisher races,
    // cache mutation, failure and broadcast remain in events/tests.rs.
    let mut counts = [0_usize; 4];
    for mut scalar_code in 0..6_usize.pow(4) {
        let mut current = ManagedProjectionSourceVersion::default();
        let mut candidate = ManagedProjectionSourceVersion::default();
        let mut orders = [Equal; 6];
        let mut old = [0; 4];
        let mut new = [0; 4];
        for axis in 0..4 {
            (old[axis], new[axis], orders[axis]) = SCALARS[scalar_code % 6];
            scalar_code /= 6;
        }
        current.session_revision = awaken_session_types::SessionRevision(old[0]);
        candidate.session_revision = awaken_session_types::SessionRevision(new[0]);
        current.root_thread_version = old[1];
        candidate.root_thread_version = new[1];
        current.lifecycle_cursor = awaken_agent_contract::RunLifecycleCursor(old[2]);
        candidate.lifecycle_cursor = awaken_agent_contract::RunLifecycleCursor(new[2]);
        current.budget_reach_generation = old[3];
        candidate.budget_reach_generation = new[3];
        for old_children in 0..9 {
            current.child_thread_versions = children(old_children);
            for new_children in 0..9 {
                candidate.child_thread_versions = children(new_children);
                orders[4] = CHILD_ORDER[old_children % 3][new_children % 3];
                orders[5] = CHILD_ORDER[old_children / 3][new_children / 3];
                for cache_matches in [false, true] {
                    for changed in [false, true] {
                        let expected = if !cache_matches {
                            ProjectionPublishDecision::RetryStaleCache
                        } else if orders.contains(&Less) {
                            ProjectionPublishDecision::RejectSourceRegression
                        } else if orders.iter().all(|order| *order == Equal) && !changed {
                            ProjectionPublishDecision::AlreadyCurrent
                        } else {
                            ProjectionPublishDecision::Apply
                        };
                        let actual = decide_projection_publish(
                            if cache_matches { u64::MAX } else { 0 },
                            u64::MAX,
                            &current,
                            &candidate,
                            changed,
                        );
                        assert_eq!(
                            actual, expected,
                            "CAS={cache_matches}, orders={orders:?}, content={changed}"
                        );
                        counts[match actual {
                            ProjectionPublishDecision::RetryStaleCache => 0,
                            ProjectionPublishDecision::RejectSourceRegression => 1,
                            ProjectionPublishDecision::AlreadyCurrent => 2,
                            ProjectionPublishDecision::Apply => 3,
                        }] += 1;
                    }
                }
            }
        }
    }
    assert_eq!(counts.iter().sum::<usize>(), 419_904);
    assert!(
        counts.iter().all(|count| *count > 0),
        "all four decisions are reached"
    );
}

/// The frame includes the CAS token and every visible/continuation field.
/// Exhaustive destructuring makes adding a SessionRecord field require an
/// oracle decision. This deliberately does not call the publisher's content
/// comparator: that comparator excludes the CAS token and is itself subject
/// to the independent frame assertion.
pub(in crate::state) fn assert_record_frame_eq(actual: &SessionRecord, expected: &SessionRecord) {
    let SessionRecord {
        cache_revision,
        committed_projection_ready,
        owner_scope,
        private_binding,
        agent_id,
        session,
        root_time,
        event_times,
        child_times,
        resource_state,
        events,
        event_page_index,
        checkpoint,
        event_thread_owners,
        child_threads,
        primary_thread_usage,
        permission_decisions,
    } = actual;
    assert_eq!(*cache_revision, expected.cache_revision, "frame: cache CAS");
    assert_eq!(owner_scope, &expected.owner_scope, "frame: Session owner");
    assert_eq!(
        private_binding, &expected.private_binding,
        "frame: private Session owner"
    );
    assert!(
        Arc::ptr_eq(event_page_index, &expected.event_page_index),
        "frame: published event index"
    );
    // The recorded gate decisions change what the wire reports for a tool call,
    // so they are projected content and belong in the frame oracle.
    assert_eq!(
        permission_decisions, &expected.permission_decisions,
        "frame: recorded permission decisions"
    );
    assert_eq!(
        *committed_projection_ready, expected.committed_projection_ready,
        "frame: complete committed-prefix availability"
    );
    assert_eq!(agent_id, &expected.agent_id, "frame: agent identity");
    assert_eq!(root_time, &expected.root_time, "frame: root time evidence");
    assert_eq!(
        event_times, &expected.event_times,
        "frame: event time evidence"
    );
    assert_eq!(
        child_times, &expected.child_times,
        "frame: child time evidence"
    );
    assert!(
        resource_state == &expected.resource_state,
        "frame: resources"
    );
    assert!(
        checkpoint == &expected.checkpoint,
        "frame: source/progress pair"
    );
    assert_eq!(
        event_thread_owners, &expected.event_thread_owners,
        "frame: event owners"
    );
    for (label, actual, expected) in [
        (
            "session",
            serde_json::to_value(session),
            serde_json::to_value(&expected.session),
        ),
        (
            "events",
            serde_json::to_value(events),
            serde_json::to_value(&expected.events),
        ),
        (
            "child threads",
            serde_json::to_value(child_threads),
            serde_json::to_value(&expected.child_threads),
        ),
        (
            "primary usage",
            serde_json::to_value(primary_thread_usage),
            serde_json::to_value(&expected.primary_thread_usage),
        ),
    ] {
        assert_eq!(actual.unwrap(), expected.unwrap(), "frame: {label}");
    }
}

#[tokio::test]
async fn owner_is_part_of_projection_equality_without_changing_the_wire_dto() {
    let state = ManagedState::new(crate::state::test_support::RehydrateFake::default()).await;
    let created = state
        .create_session(
            serde_json::from_value(
                serde_json::json!({"agent":"coder", "environment_id":"env_local"}),
            )
            .unwrap(),
            "workspace-a",
        )
        .await
        .unwrap();
    let original = state
        .sessions
        .lock()
        .unwrap_or_else(|poisoned| panic!("{poisoned}"))
        .get(&created.id)
        .unwrap()
        .clone();
    let mut other = original.clone();
    assert!(original.same_projected_content(&other).unwrap());
    other.owner_scope = "workspace-b".into();
    assert!(!original.same_projected_content(&other).unwrap());
    assert!(!other.same_projected_content(&original).unwrap());
    assert_eq!(
        serde_json::to_value(original.session_projection()).unwrap(),
        serde_json::to_value(other.session_projection()).unwrap(),
        "the owner is a private cache projection, never another wire field"
    );
}
