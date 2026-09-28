//! Which committed lifecycle facts one projection candidate may publish.

use std::collections::HashSet;

use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use awaken_agent_contract::{RunLifecycleCursor, RunLifecycleEvent, RunLifecycleEventKind};

/// The accepted lifecycle prefix of one candidate.
///
/// Consumer: the timeline projection, which projects only `accepted`, advances
/// its lifecycle checkpoint to `cursor`, and withholds the transcript of every
/// `blocked` Thread so a tool call is never classified before its ticket.
#[derive(Debug, Default)]
pub struct LifecycleAcceptance {
    pub accepted: Vec<RunLifecycleEvent>,
    pub cursor: RunLifecycleCursor,
    pub blocked_threads: HashSet<String>,
}

/// The committed snapshots and relationships acceptance is fenced by.
///
/// Consumer: [`accept_lifecycle`]; borrowed from the candidate's evidence.
pub struct AcceptanceFence<'a> {
    pub session_id: &'a str,
    pub root: Option<&'a RunRecoverySnapshot>,
    pub children: &'a std::collections::HashMap<String, RunRecoverySnapshot>,
    pub known_children: &'a HashSet<&'a str>,
}

impl AcceptanceFence<'_> {
    fn snapshot(&self, thread_id: &str) -> Option<&RunRecoverySnapshot> {
        if thread_id == self.session_id {
            self.root
        } else {
            self.children.get(thread_id)
        }
    }
}

/// Accept the merged lifecycle in cursor order until the first commit this
/// candidate cannot publish.
///
/// Rules: an event of neither the root nor a known child is out of scope when
/// it lies inside the root fence (it advances the cursor and is dropped) and
/// stops acceptance otherwise (its link may not be visible yet); an event
/// beyond its own Thread's observed prefix stops acceptance; the current
/// Awaiting commit of a Thread without its answerable ticket (and not a budget
/// pause) stops acceptance and blocks that Thread's transcript.
pub fn accept_lifecycle(
    lifecycle: Vec<RunLifecycleEvent>,
    fence: &AcceptanceFence<'_>,
    initial_cursor: RunLifecycleCursor,
    has_pending_ticket: impl Fn(&str) -> bool,
) -> LifecycleAcceptance {
    let root_store_cursor = fence.root.map(|snapshot| snapshot.store_cursor);
    let latest_cursor_by_thread = lifecycle
        .iter()
        .filter(|event| {
            fence
                .snapshot(&event.thread_id.0)
                .is_some_and(|snapshot| event.source_commit_cursor <= snapshot.store_cursor)
        })
        .fold(std::collections::HashMap::new(), |mut latest, event| {
            latest.insert(event.thread_id.0.clone(), event.cursor);
            latest
        });
    let mut acceptance = LifecycleAcceptance {
        cursor: initial_cursor,
        ..LifecycleAcceptance::default()
    };
    for event in lifecycle {
        let thread_id = event.thread_id.0.as_str();
        let is_root = thread_id == fence.session_id;
        if !is_root && !fence.known_children.contains(thread_id) {
            // The relationship read happened after the root fence. Therefore
            // an event inside that prefix which still has no relationship is
            // out-of-scope lifecycle (another Postgres Session or invisible
            // auxiliary work) and can advance the feed cursor without creating
            // a public Thread. An event beyond the prefix may be a just-created
            // child whose link/snapshot is not visible yet, so it remains
            // replayable. Thread-id spelling is never used as provenance.
            if root_store_cursor.is_some_and(|cursor| event.source_commit_cursor <= cursor) {
                acceptance.cursor = event.cursor;
                continue;
            }
            break;
        }
        if fence
            .snapshot(thread_id)
            .is_none_or(|snapshot| event.source_commit_cursor > snapshot.store_cursor)
        {
            break;
        }
        let is_current_awaiting = event.kind == RunLifecycleEventKind::Awaiting
            && latest_cursor_by_thread.get(thread_id) == Some(&event.cursor);
        let is_budget_pause = event.await_reason.as_ref()
            == Some(&awaken_agent_contract::agent::awaiting::AwaitReason::BudgetReached);
        // A current Awaiting transition cannot be published without its exact
        // answerable ticket. Historical Awaiting transitions may be followed by
        // a committed resume and therefore need no live ticket.
        if is_current_awaiting && !has_pending_ticket(thread_id) && !is_budget_pause {
            // Transcript and lifecycle are one public classification. If the
            // message were marked projected before its ticket became visible,
            // a ToolUse would be irreversibly downgraded to allow and could
            // never become the answerable requires-action event.
            acceptance.blocked_threads.insert(thread_id.to_string());
            break;
        }
        acceptance.cursor = event.cursor;
        acceptance.accepted.push(event);
    }
    acceptance
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::run::{Id as RunId, RunState};
    use awaken_agent_contract::agent::thread::Id as ThreadId;

    fn snapshot(thread: &str, store_cursor: u64) -> RunRecoverySnapshot {
        RunRecoverySnapshot {
            thread_id: ThreadId(thread.into()),
            claimed_run_id: RunId("run".into()),
            runs: vec![],
            latest_run_id: None,
            messages: vec![],
            message_commit_cursors: vec![],
            commit_timestamps: Default::default(),
            state: vec![],
            state_commit_cursors: vec![],
            events: vec![],
            resume_tickets: vec![],
            thread_version: 1,
            store_cursor,
            next_commit_ordinal: 0,
        }
    }

    fn event(cursor: u64, thread: &str, kind: RunLifecycleEventKind) -> RunLifecycleEvent {
        RunLifecycleEvent {
            cursor: awaken_agent_contract::encode_run_lifecycle_cursor(cursor, 0).unwrap(),
            source_commit_cursor: cursor,
            thread_id: ThreadId(thread.into()),
            run_id: RunId(format!("run-{thread}")),
            kind,
            state: RunState::Running,
            await_reason: None,
        }
    }

    #[test]
    fn acceptance_decision_table() {
        // Causes: C1 root / known child / unknown Thread; C2 source inside or
        // beyond its Thread prefix (unknown: inside or beyond the root fence);
        // C3 current Awaiting with or without its ticket, or a budget pause.
        // Effects: A1 accept and advance; A2 skip and advance (out of scope);
        // A3 stop without advancing; A4 stop and block the Thread's transcript.
        use RunLifecycleEventKind::{Awaiting, Running};
        let root = snapshot("session", 5);
        let children =
            std::collections::HashMap::from([("child".to_string(), snapshot("child", 4))]);
        let known = HashSet::from(["child"]);
        let fence = AcceptanceFence {
            session_id: "session",
            root: Some(&root),
            children: &children,
            known_children: &known,
        };
        let cursor = |value| awaken_agent_contract::encode_run_lifecycle_cursor(value, 0).unwrap();
        let mut budget = event(4, "child", Awaiting);
        budget.await_reason =
            Some(awaken_agent_contract::agent::awaiting::AwaitReason::BudgetReached);
        for (rule, lifecycle, ticket, accepted, advanced, blocked) in [
            (
                "A1 root",
                vec![event(1, "session", Running)],
                true,
                1,
                1,
                None,
            ),
            (
                "A1 child",
                vec![event(2, "child", Running)],
                true,
                1,
                2,
                None,
            ),
            (
                "A2 unknown inside fence",
                vec![event(3, "stray", Running)],
                true,
                0,
                3,
                None,
            ),
            (
                "A3 unknown beyond fence",
                vec![event(6, "stray", Running)],
                true,
                0,
                0,
                None,
            ),
            (
                "A3 child beyond its prefix",
                vec![event(5, "child", Running)],
                true,
                0,
                0,
                None,
            ),
            (
                "A1 current Awaiting with ticket",
                vec![event(4, "child", Awaiting)],
                true,
                1,
                4,
                None,
            ),
            (
                "A4 current Awaiting without ticket",
                vec![event(4, "child", Awaiting)],
                false,
                0,
                0,
                Some("child"),
            ),
            (
                "A1 budget pause needs no ticket",
                vec![budget.clone()],
                false,
                1,
                4,
                None,
            ),
            (
                "A1 historical Awaiting needs no ticket",
                vec![event(1, "child", Awaiting), event(2, "child", Running)],
                false,
                2,
                2,
                None,
            ),
        ] {
            let result = accept_lifecycle(lifecycle, &fence, cursor(0), |_| ticket);
            assert_eq!(result.accepted.len(), accepted, "{rule}");
            assert_eq!(result.cursor, cursor(advanced), "{rule}");
            assert_eq!(
                result.blocked_threads,
                blocked
                    .into_iter()
                    .map(str::to_string)
                    .collect::<HashSet<_>>(),
                "{rule}"
            );
        }
    }
}
