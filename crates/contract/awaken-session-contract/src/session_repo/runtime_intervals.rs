//! Root-CAS reducers for one customer-visible Session Running interval.

use super::{PersistedSession, SessionExecutionState, SessionRevision};

impl PersistedSession {
    /// Open the one continuous Running interval after the execution transition
    /// has committed its logical owner. Replays and overlapping activities join
    /// the existing interval.
    pub fn begin_runtime_interval(&mut self, started_at_unix_ms: u64) -> bool {
        if !runtime_interval_may_open(
            self.execution == SessionExecutionState::Running,
            self.running_interval.is_some(),
        ) {
            return false;
        }
        let Some(opened_revision) = self.revision.0.checked_add(1).map(SessionRevision) else {
            return false;
        };
        self.running_interval = Some(crate::SessionRuntimeIntervalStart {
            interval_id: crate::stable_fingerprint(&(
                "session-runtime-interval-v1",
                self.session_id.as_str(),
                self.activity_epoch,
            )),
            activity_epoch: self.activity_epoch,
            started_at_unix_ms,
            opened_revision,
            observations: Vec::new(),
        });
        true
    }

    /// Retain one exact Runtime commit boundary under the activity epoch that
    /// admitted it. Exact replay is a no-op; a conflicting cursor identity or an
    /// observation outside the current interval is rejected.
    pub fn observe_runtime_interval(
        &mut self,
        observation: crate::SessionRuntimeIntervalObservation,
    ) -> bool {
        let legacy_singleton = self.active_activity_epochs.is_empty()
            && self.execution == SessionExecutionState::Running
            && observation.activity_epoch == self.activity_epoch;
        if !legacy_singleton
            && !self
                .active_activity_epochs
                .contains(&observation.activity_epoch)
        {
            return false;
        }
        let Some(interval) = self.running_interval.as_mut() else {
            return false;
        };
        if let Some(existing) = interval.observations.iter().find(|existing| {
            existing.lifecycle_cursor == observation.lifecycle_cursor
                || (existing.activity_epoch == observation.activity_epoch
                    && existing.source_commit_cursor == observation.source_commit_cursor)
        }) {
            return existing == &observation;
        }
        interval.observations.push(observation);
        interval.observations.sort_by_key(|observation| {
            (
                observation.source_commit_cursor,
                observation.lifecycle_cursor,
                observation.activity_epoch,
            )
        });
        true
    }

    /// Cumulative active time at an observation instant, including the one
    /// currently open interval exactly once. The durable closed total and open
    /// interval are the sole clock ledger; request admission must use this view
    /// rather than inventing a gate-local timer.
    #[must_use]
    pub fn effective_runtime_active_millis(&self, now_unix_ms: u64) -> u64 {
        self.runtime_active_millis.saturating_add(
            self.running_interval
                .as_ref()
                .map(|start| {
                    normalized_runtime_interval_end(start.started_at_unix_ms, now_unix_ms)
                        .saturating_sub(start.started_at_unix_ms)
                })
                .unwrap_or_default(),
        )
    }

    /// Close and remove the current interval. The returned value is committed
    /// through the same root mutation's lifecycle outbox.
    pub fn close_runtime_interval(
        &mut self,
        ended_at_unix_ms: u64,
    ) -> Option<crate::SessionRuntimeInterval> {
        let closed_revision = self.revision.0.checked_add(1).map(SessionRevision)?;
        self.running_interval.take().map(|start| {
            let ended_at_unix_ms =
                normalized_runtime_interval_end(start.started_at_unix_ms, ended_at_unix_ms);
            self.runtime_active_millis = self
                .runtime_active_millis
                .saturating_add(ended_at_unix_ms.saturating_sub(start.started_at_unix_ms));
            self.usage_cursor.active_seconds = self.runtime_active_millis / 1_000;
            let interval = crate::SessionRuntimeInterval {
                interval_id: start.interval_id,
                activity_epoch: start.activity_epoch,
                started_at_unix_ms: start.started_at_unix_ms,
                ended_at_unix_ms,
                opened_revision: start.opened_revision,
                closed_revision,
                observations: start.observations,
                usage: self.usage_cursor.clone(),
                max_list_cost_minor: self.budget.max_list_cost_minor(),
            };
            self.closed_runtime_intervals.push(interval.clone());
            interval
        })
    }
}

#[must_use]
const fn runtime_interval_may_open(execution_is_running: bool, interval_is_open: bool) -> bool {
    execution_is_running && !interval_is_open
}

#[must_use]
const fn normalized_runtime_interval_end(started_at_unix_ms: u64, ended_at_unix_ms: u64) -> u64 {
    if ended_at_unix_ms < started_at_unix_ms {
        started_at_unix_ms
    } else {
        ended_at_unix_ms
    }
}

#[cfg(kani)]
#[kani::proof]
fn runtime_intervals_open_once_and_never_close_before_start() {
    let execution_is_running = kani::any();
    let interval_is_open = kani::any();
    assert_eq!(
        runtime_interval_may_open(execution_is_running, interval_is_open),
        execution_is_running && !interval_is_open
    );

    let started_at_unix_ms = kani::any();
    let ended_at_unix_ms = kani::any();
    let normalized = normalized_runtime_interval_end(started_at_unix_ms, ended_at_unix_ms);
    assert!(normalized >= started_at_unix_ms);
    assert_eq!(
        normalized,
        if ended_at_unix_ms < started_at_unix_ms {
            started_at_unix_ms
        } else {
            ended_at_unix_ms
        }
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_interval_retains_all_runtime_boundaries_and_exact_usage() {
        // Causes: C1 two overlapping admitted epochs share one Running interval;
        // C2 each settles from a different committed Thread/Run cursor; C3 only
        // the second settlement removes the last epoch; C4 cumulative usage was
        // observed while the interval was open; C5 close crosses a whole-second
        // boundary. Effects: E1 one interval is retained, not two; E2 both exact
        // observations survive in commit order; E3 opened/closed root revisions
        // fence the interval; E4 historical usage retains token counters and its
        // active seconds is corrected by the close CAS; C6 the same Run resumes
        // in a later interval; C7 a later interval is child-only. Effects E5
        // later intervals retain distinct identities and exact ownership without
        // using Run id as an interval key. Decision rules:
        // R1=C1+C2+!C3=>retain open+E2; R2=C1+C2+C3=>E1+E3;
        // R3=C4+C5=>E4; R4=C6/C7=>E5. Constraints: Runtime remains transcript/counter truth;
        // this vector is the Session root's unbounded replay projection only.
        let mut value =
            super::super::mutation_tests::session("interval-history", SessionRevision(7));
        value.execution = SessionExecutionState::Running;
        assert_eq!(value.begin_activity_epoch(), Some(1));
        assert!(value.begin_runtime_interval(1_000));
        value.revision = SessionRevision(8);
        assert_eq!(value.begin_activity_epoch(), Some(2));
        assert!(!value.begin_runtime_interval(1_100), "R1 one interval");
        value.usage_cursor.by_model.insert(
            "model-a".into(),
            crate::ManagedModelUsageCursor {
                input_tokens: 5,
                output_tokens: 3,
                cache_read_tokens: 2,
                cache_creation_tokens: 1,
            },
        );
        for (epoch, thread, run, lifecycle, source) in [
            (1, "interval-history", "run-root", 2_000, 2),
            (2, "child", "run-child", 4_000, 4),
        ] {
            assert!(
                value.observe_runtime_interval(crate::SessionRuntimeIntervalObservation {
                    activity_epoch: epoch,
                    thread_id: awaken_agent_contract::agent::thread::Id(thread.into()),
                    run_id: awaken_agent_contract::agent::run::Id(run.into()),
                    lifecycle_cursor: awaken_agent_contract::RunLifecycleCursor(lifecycle),
                    source_commit_cursor: source,
                })
            );
        }
        assert_eq!(value.settle_activity_epoch(1), Some(false), "R1");
        assert_eq!(value.settle_activity_epoch(2), Some(true), "R2");
        value.revision = SessionRevision(11);
        let closed = value.close_runtime_interval(3_500).expect("R2 close");
        assert_eq!(value.closed_runtime_intervals, vec![closed.clone()], "E1");
        assert_eq!(closed.opened_revision, SessionRevision(8), "E3");
        assert_eq!(closed.closed_revision, SessionRevision(12), "E3");
        assert_eq!(closed.observations.len(), 2, "E2");
        assert_eq!(closed.observations[0].source_commit_cursor, 2, "E2");
        assert_eq!(closed.observations[1].source_commit_cursor, 4, "E2");
        assert_eq!(closed.usage.active_seconds, 2, "E4");
        assert_eq!(closed.usage.by_model["model-a"].input_tokens, 5, "E4");

        for (epoch, thread, lifecycle, source) in [
            (3, "interval-history", 6_000, 6),
            (4, "child-only", 8_000, 8),
        ] {
            assert_eq!(value.begin_activity_epoch(), Some(epoch), "R4 setup");
            assert!(value.begin_runtime_interval(u64::from(epoch) * 1_000));
            value.revision = SessionRevision(value.revision.0 + 1);
            assert!(
                value.observe_runtime_interval(crate::SessionRuntimeIntervalObservation {
                    activity_epoch: epoch,
                    thread_id: awaken_agent_contract::agent::thread::Id(thread.into()),
                    run_id: awaken_agent_contract::agent::run::Id("run-root".into()),
                    lifecycle_cursor: awaken_agent_contract::RunLifecycleCursor(lifecycle),
                    source_commit_cursor: source,
                },)
            );
            assert_eq!(value.settle_activity_epoch(epoch), Some(true));
            let next = value
                .close_runtime_interval(u64::from(epoch) * 1_000 + 500)
                .expect("R4 close");
            assert_ne!(next.interval_id, closed.interval_id, "R4/E5");
            assert_eq!(next.observations[0].thread_id.0, thread, "R4/E5");
            value.revision = SessionRevision(value.revision.0 + 1);
        }
        assert_eq!(value.closed_runtime_intervals.len(), 3, "R4/E5");
    }

    #[test]
    fn revision_exhaustion_preserves_the_open_interval_for_fail_closed_retry() {
        // Cause/effect graph: C1 revision MAX-1 may open an interval at the
        // prospective MAX revision; C2 the aggregate reaches revision MAX before
        // close; C3 another aggregate is already at MAX before open. Effects: E1
        // C1 opens exactly; E2 C2 returns None without taking/mutating the open
        // interval, usage, or closed history; E3 C3 refuses to open. Decision
        // rules X1=C1=>E1; X2=C1+C2=>E2; X3=C3=>E3. The repository's adjacent
        // mutation test owns the matching RevisionExhausted CAS error.
        let mut value = super::super::mutation_tests::session(
            "interval-revision-exhausted",
            SessionRevision(u64::MAX - 1),
        );
        value.execution = SessionExecutionState::Running;
        assert_eq!(value.begin_activity_epoch(), Some(1));
        assert!(value.begin_runtime_interval(1_000), "X1/E1");
        let open = value.running_interval.clone();
        value.revision = SessionRevision(u64::MAX);
        assert!(value.close_runtime_interval(2_000).is_none(), "X2/E2");
        assert_eq!(value.running_interval, open, "X2/E2");
        assert!(value.closed_runtime_intervals.is_empty(), "X2/E2");
        assert_eq!(value.runtime_active_millis, 0, "X2/E2");

        let mut never_opened = super::super::mutation_tests::session(
            "interval-open-revision-exhausted",
            SessionRevision(u64::MAX),
        );
        never_opened.execution = SessionExecutionState::Running;
        assert_eq!(never_opened.begin_activity_epoch(), Some(1));
        assert!(!never_opened.begin_runtime_interval(1_000), "X3/E3");
        assert!(never_opened.running_interval.is_none(), "X3/E3");
    }
}
