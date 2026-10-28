// @formal entrypoint session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
//! Usage of Managed Session events over one committed evidence prefix.

use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use awaken_agent_contract::thread::usage::ThreadUsage;
use awaken_session_budget_contract::SessionUsage;
use awaken_session_execution_contract::ProjectionThreadSnapshots;
use awaken_session_state_contract::SessionRuntimeInterval;

use crate::ProjectionError;

/// The usage one logical Thread committed, read from the same recovery prefix
/// as its transcript and Run state rather than a second accounting snapshot.
/// A missing Thread prefix is zero usage.
///
/// Consumer: the protocol adapter, which prices it with the Session-frozen
/// price authority for its per-Thread usage projection.
#[must_use]
pub fn committed_thread_usage(snapshot: Option<&RunRecoverySnapshot>) -> SessionUsage {
    snapshot.map_or_else(SessionUsage::default, |snapshot| {
        awaken_session_application_contract::session_usage_from_thread_usage(
            ThreadUsage::from_committed_state(&snapshot.state),
        )
    })
}

/// Usage carried by a Managed aggregate close at its exact Runtime commit.
/// Token accounting reuses the Runtime's neutral usage cell and merge rule;
/// this Managed event rule selects their historical prefix and only the
/// Session intervals already closed at that boundary. The adapter alone
/// prices and encodes the result. No independent read or observing clock is
/// used, so advancing recovery cannot change a previously published close.
pub fn session_usage_as_of(
    closed_intervals: &[SessionRuntimeInterval],
    session_id: &str,
    root: Option<&RunRecoverySnapshot>,
    children: &ProjectionThreadSnapshots,
    source_commit_cursor: u64,
) -> Result<SessionUsage, ProjectionError> {
    let mut attributed = ThreadUsage::default();
    for snapshot in root.into_iter().chain(
        children
            .iter()
            .filter(|(id, _)| id.as_str() != session_id)
            .map(|(_, snapshot)| snapshot),
    ) {
        attributed.merge(&thread_usage_as_of(snapshot, source_commit_cursor)?);
    }
    let mut usage =
        awaken_session_application_contract::session_usage_from_thread_usage(attributed);
    let active_millis = closed_intervals
        .iter()
        .filter(|interval| {
            crate::interval_close_cursor(interval)
                .is_some_and(|close| close <= source_commit_cursor)
        })
        .map(|interval| {
            interval
                .ended_at_unix_ms
                .saturating_sub(interval.started_at_unix_ms)
        })
        .sum::<u64>();
    usage.active_seconds = active_millis / 1_000;
    Ok(usage)
}

/// A missing or misaligned command coordinate fails the candidate closed:
/// it cannot be assigned before or after the historical event boundary.
fn thread_usage_as_of(
    snapshot: &RunRecoverySnapshot,
    source_commit_cursor: u64,
) -> Result<ThreadUsage, ProjectionError> {
    if snapshot.state_commit_cursors.len() != snapshot.state.len() {
        return Err(ProjectionError::corrupt(format!(
            "Thread {:?} state recovery omitted commit coordinates; its usage cannot be placed at a commit",
            snapshot.thread_id.0
        )));
    }
    let state = snapshot
        .state
        .iter()
        .zip(&snapshot.state_commit_cursors)
        .filter(|(_, cursor)| **cursor <= source_commit_cursor)
        .map(|(command, _)| command.clone())
        .collect::<Vec<_>>();
    Ok(ThreadUsage::from_committed_state(&state))
}

#[cfg(test)]
mod usage_as_of_tests {
    use super::*;
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::agent::state::{Command, MergePolicy, Scope};
    use awaken_agent_contract::agent::thread::Id as ThreadId;

    fn usage_command(model: &str, input_tokens: u64) -> Command {
        let mut usage = ThreadUsage::default();
        usage.record(
            model,
            awaken_agent_contract::audit::model_request::TokenUsage {
                prompt_tokens: input_tokens,
                ..Default::default()
            },
        );
        Command::set(
            Scope::Thread,
            MergePolicy::Commutative,
            awaken_agent_contract::thread::usage::THREAD_USAGE_STATE_KEY,
            serde_json::to_value(usage).unwrap(),
        )
    }

    fn snapshot(state: Vec<Command>, state_commit_cursors: Vec<u64>) -> RunRecoverySnapshot {
        RunRecoverySnapshot {
            commit_timestamps: Default::default(),
            thread_id: ThreadId("usage-thread".into()),
            claimed_run_id: RunId("usage-run".into()),
            runs: Vec::new(),
            latest_run_id: None,
            messages: Vec::new(),
            message_commit_cursors: Vec::new(),
            state,
            state_commit_cursors,
            events: Vec::new(),
            resume_tickets: Vec::new(),
            thread_version: 1,
            store_cursor: 30,
            next_commit_ordinal: 0,
        }
    }

    #[test]
    fn historical_usage_without_state_commit_cursors_fails_closed() {
        // Causes: C1 every state command carries its commit cursor; C2 the
        // recovered cursors are missing or misaligned. Effects: E1 usage as of
        // a commit counts only commands at or before it; E2 a prefix that
        // cannot place its commands fails the candidate instead of counting
        // every command, later ones included, into an earlier boundary.
        // Rules: R1 C1 => E1 (an empty prefix before the first command is the
        // default usage); R2 C2 => E2.
        let command = Command::set(
            Scope::Thread,
            MergePolicy::Disjoint,
            "usage/probe",
            serde_json::json!({}),
        );
        assert_eq!(
            thread_usage_as_of(&snapshot(vec![command.clone()], vec![20]), 10).unwrap(),
            ThreadUsage::default(),
            "R1 a command after the boundary is not counted"
        );
        for cursors in [Vec::new(), vec![20, 21]] {
            let damaged = snapshot(vec![command.clone()], cursors.clone());
            assert!(
                thread_usage_as_of(&damaged, 10).is_err(),
                "R2 misaligned cursors {cursors:?} fail closed"
            );
            for (root, children) in [
                (Some(&damaged), ProjectionThreadSnapshots::new()),
                (None, [("child".into(), damaged.clone())].into()),
            ] {
                let error = session_usage_as_of(&[], "root", root, &children, 10).unwrap_err();
                assert!(matches!(error, ProjectionError::Corrupt(_)), "R2");
                assert_eq!(
                    error.to_string(),
                    "Thread \"usage-thread\" state recovery omitted commit coordinates; its usage cannot be placed at a commit",
                    "R2 root and child retain the exact internal fault"
                );
            }
        }
    }

    #[test]
    fn historical_session_usage_counts_distinct_threads_at_the_exact_commit_prefix() {
        // Causes: C1 a usage cell commits before/at/after the boundary; C2 a
        // root is also present in the child map; C3 recovery advances beyond
        // the boundary; C4 distinct Threads use the same or different model.
        // Effects: E1 only the latest C1 cell <= boundary contributes; E2 C2
        // counts root once; E3 C3 keeps historical usage unchanged; E4 C4
        // preserves per-model attribution through the neutral merge rule.
        // Rules: P1 before every cell => zero; P2 at/between cells => E1-E4;
        // P3 after every cell => latest cumulative cells, never sum of Sets.
        let root = snapshot(
            vec![usage_command("main", 5), usage_command("main", 15)],
            vec![20, 30],
        );
        let children = ProjectionThreadSnapshots::from([
            ("root".into(), root.clone()),
            (
                "child".into(),
                snapshot(vec![usage_command("other", 7)], vec![25]),
            ),
        ]);
        for (boundary, expected) in [(19, 0), (20, 5), (25, 12), (29, 12), (30, 22)] {
            let usage = session_usage_as_of(&[], "root", Some(&root), &children, boundary).unwrap();
            assert_eq!(usage.input_tokens, expected, "P1-P3 boundary={boundary}");
            assert_eq!(
                usage.by_model.get("other").map(|usage| usage.input_tokens),
                (boundary >= 25).then_some(7),
                "P2/P3 attribution"
            );
        }
        let earlier = snapshot(vec![usage_command("main", 5)], vec![20]);
        assert_eq!(
            session_usage_as_of(&[], "root", Some(&earlier), &children, 25).unwrap(),
            session_usage_as_of(&[], "root", Some(&root), &children, 25).unwrap(),
            "P2 growing recovery retains the same historical prefix"
        );
        let saturated = ProjectionThreadSnapshots::from([
            (
                "a".into(),
                snapshot(vec![usage_command("main", u64::MAX)], vec![20]),
            ),
            (
                "b".into(),
                snapshot(vec![usage_command("main", 1)], vec![20]),
            ),
        ]);
        assert_eq!(
            session_usage_as_of(&[], "root", None, &saturated, 20)
                .unwrap()
                .input_tokens,
            u64::MAX,
            "P2 neutral merge retains saturation"
        );
    }

    #[test]
    fn historical_session_active_time_uses_only_anchored_closed_intervals() {
        // Causes: C1 an interval's last close observation is <= boundary;
        // C2 close is later or has no observation; C3 recovery contains later
        // intervals. Effects: E1 C1 contributes its durable duration once;
        // E2 C2 contributes nothing; E3 C3 cannot change an earlier result.
        // Rules: I1 no admitted close => zero; I2 exact close => E1; I3 after
        // multiple closes => sum milliseconds before rounding to seconds.
        // There is no open interval/clock input: existing protocol tests own
        // that root-ledger/wire boundary rather than a duplicate time reducer.
        let interval = |id: &str, end, observations: &[u64]| SessionRuntimeInterval {
            interval_id: id.into(),
            activity_epoch: 1,
            started_at_unix_ms: 0,
            ended_at_unix_ms: end,
            opened_revision: awaken_session_types::SessionRevision(1),
            closed_revision: awaken_session_types::SessionRevision(2),
            admitted_after_commit_cursor: None,
            observations: observations
                .iter()
                .map(
                    |cursor| awaken_session_state_contract::SessionRuntimeIntervalObservation {
                        activity_epoch: 1,
                        thread_id: ThreadId("root".into()),
                        run_id: RunId(id.into()),
                        lifecycle_cursor: awaken_agent_contract::encode_run_lifecycle_cursor(
                            *cursor, 0,
                        )
                        .unwrap(),
                        source_commit_cursor: *cursor,
                    },
                )
                .collect(),
            usage: Default::default(),
            max_list_cost_minor: None,
        };
        let intervals = [
            interval("first", 1_999, &[20, 24]),
            interval("later", 1_501, &[30]),
            interval("unanchored", 999_999, &[]),
        ];
        for (boundary, expected) in [(19, 0), (20, 0), (24, 1), (29, 1), (30, 3)] {
            assert_eq!(
                session_usage_as_of(&intervals, "root", None, &Default::default(), boundary)
                    .unwrap()
                    .active_seconds,
                expected,
                "I1-I3 boundary={boundary}"
            );
        }
        assert_eq!(
            session_usage_as_of(&intervals[..1], "root", None, &Default::default(), 24).unwrap(),
            session_usage_as_of(&intervals, "root", None, &Default::default(), 24).unwrap(),
            "I2 later intervals cannot change the historical close"
        );
    }
}
