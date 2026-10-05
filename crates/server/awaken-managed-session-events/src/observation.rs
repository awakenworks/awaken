//! Committed Runtime observations of each Run: model-request spans and
//! context-compaction markers.
//!
//! Consumer: the protocol adapter's root and child projectors. This is the
//! sole source of these facts; live Step results carry neither.

use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use awaken_runtime_contract::compaction::RunCompactionMarker;
use awaken_runtime_contract::llm::ModelRequestObservation;

use crate::{
    DerivedSessionEvent, EventIdentity, FactProvenance, ManagedEventType, ProjectionError,
    SessionEventKind,
};

/// The observation facts of one committed Thread prefix. Publication progress
/// does not enter this domain decision; the Managed adapter deduplicates the
/// resulting stable fact keys at its existing event boundary.
pub fn run_observation_facts(
    thread_id: &str,
    snapshot: &RunRecoverySnapshot,
) -> Result<Vec<DerivedSessionEvent>, ProjectionError> {
    let at = |role, provenance| EventIdentity::new(thread_id, role, provenance);
    let mut facts = Vec::new();
    // A repeated audit row in the same committed prefix emits one fact.
    let mut emitted = std::collections::HashSet::new();
    for run in &snapshot.runs {
        if RunCompactionMarker::is_recorded(&snapshot.state, &run.id.0) {
            let key = at(
                ManagedEventType::RunContextCompacted,
                FactProvenance::RunState {
                    run_id: run.id.0.clone(),
                },
            );
            if emitted.insert(key.clone()) {
                facts.push(DerivedSessionEvent::new(
                    key,
                    SessionEventKind::ContextCompacted,
                ));
            }
        }
        for audit in snapshot
            .events
            .iter()
            .filter(|audit| audit.run_id == run.id)
        {
            let Some(observation) =
                ModelRequestObservation::from_record(audit).map_err(|error| {
                    ProjectionError::corrupt(format!(
                        "decode committed model request observation: {error}"
                    ))
                })?
            else {
                continue;
            };
            let provenance = || FactProvenance::Audit {
                run_id: run.id.0.clone(),
                sequence: audit.sequence,
            };
            let start = at(ManagedEventType::ModelRequestStart, provenance());
            let end = at(ManagedEventType::ModelRequestEnd, provenance());
            if !emitted.insert(end.clone()) {
                continue;
            }
            facts.push(DerivedSessionEvent::new(
                start.clone(),
                SessionEventKind::ModelRequestStart,
            ));
            facts.push(DerivedSessionEvent::new(
                end,
                SessionEventKind::ModelRequestEnd {
                    start,
                    is_error: observation.is_error,
                    usage: observation.usage,
                },
            ));
        }
    }
    Ok(facts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::run::{Id as RunId, Record, RunState};
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_agent_contract::audit::model_request::TokenUsage;

    fn snapshot(audits: Vec<(u64, bool)>) -> RunRecoverySnapshot {
        let run = RunId("run".into());
        RunRecoverySnapshot {
            thread_id: ThreadId("thread".into()),
            claimed_run_id: run.clone(),
            runs: vec![Record {
                id: run.clone(),
                thread_id: ThreadId("thread".into()),
                state: RunState::Running,
            }],
            latest_run_id: Some(run.clone()),
            messages: vec![],
            message_commit_cursors: vec![],
            commit_timestamps: Default::default(),
            state: vec![RunCompactionMarker::command("run")],
            state_commit_cursors: vec![1],
            events: audits
                .into_iter()
                .map(
                    |(sequence, is_error)| awaken_agent_contract::audit::record::Record {
                        sequence,
                        run_id: run.clone(),
                        kind: awaken_agent_contract::audit::kind::Kind::ModelRequestCompleted,
                        payload: serde_json::to_value(ModelRequestObservation {
                            is_error,
                            usage: TokenUsage::default(),
                            retry_count: 0,
                        })
                        .unwrap(),
                    },
                )
                .collect(),
            resume_tickets: vec![],
            thread_version: 1,
            store_cursor: 1,
            next_commit_ordinal: 0,
        }
    }

    #[test]
    fn observation_facts_decision_table() {
        // Causes: C1 a recorded compaction marker; C2 model-request audit rows,
        // one repeated; C3 the function is called again over the same prefix.
        // Effects: E1 one compaction fact; E2 one start/end pair per request;
        // E3 repeated audit rows emit once; E4 C3 returns the same facts,
        // independent of any process-local publication state. Rules:
        // R1=C1+C2=>E1+E2+E3; R2=C1+C2+C3=>E1+E2+E3+E4.
        let prefix = snapshot(vec![(2, false), (3, true), (3, true)]);
        let roles = || {
            run_observation_facts("thread", &prefix)
                .unwrap()
                .into_iter()
                .map(|fact| (fact.key.role, fact.key.provenance))
                .collect::<Vec<_>>()
        };
        let audit = |sequence| FactProvenance::Audit {
            run_id: "run".into(),
            sequence,
        };
        let compaction = FactProvenance::RunState {
            run_id: "run".into(),
        };
        let first = roles();
        assert_eq!(
            first,
            vec![
                (ManagedEventType::RunContextCompacted, compaction.clone()),
                (ManagedEventType::ModelRequestStart, audit(2)),
                (ManagedEventType::ModelRequestEnd, audit(2)),
                (ManagedEventType::ModelRequestStart, audit(3)),
                (ManagedEventType::ModelRequestEnd, audit(3)),
            ],
            "E1+E2+E3"
        );
        assert_eq!(roles(), first, "R2/E4 stable from committed prefix alone");
    }
}
