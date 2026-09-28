//! Committed Runtime observations of each Run: model-request spans and
//! context-compaction markers.
//!
//! Consumer: the protocol adapter's root and child projectors. This is the
//! sole source of these facts; live Step results carry neither.

use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use awaken_runtime_contract::compaction::RunCompactionMarker;
use awaken_runtime_contract::llm::ModelRequestObservation;

use crate::{FactKey, FactKind, FactProvenance, FactRole, ProjectionError, TimelineFact};

/// The observation facts of one Thread prefix not yet published. `published`
/// answers whether a fact's key already names a published event; a request
/// whose end is published is skipped whole.
pub fn run_observation_facts(
    thread_id: &str,
    snapshot: &RunRecoverySnapshot,
    published: impl Fn(&FactKey) -> bool,
) -> Result<Vec<TimelineFact>, ProjectionError> {
    let at = |role, provenance| FactKey::new(thread_id, role, provenance);
    let mut facts = Vec::new();
    // A fact emitted earlier in this same pass counts as published, exactly
    // as it would once appended.
    let mut emitted = std::collections::HashSet::new();
    for run in &snapshot.runs {
        if RunCompactionMarker::is_recorded(&snapshot.state, &run.id.0) {
            let key = at(
                FactRole::RunContextCompacted,
                FactProvenance::RunState {
                    run_id: run.id.0.clone(),
                },
            );
            if !published(&key) && emitted.insert(key.clone()) {
                facts.push(TimelineFact::new(key, FactKind::ContextCompacted));
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
            let start = at(FactRole::ModelRequestStart, provenance());
            let end = at(FactRole::ModelRequestEnd, provenance());
            if published(&end) || !emitted.insert(end.clone()) {
                continue;
            }
            facts.push(TimelineFact::new(
                start.clone(),
                FactKind::ModelRequestStart,
            ));
            facts.push(TimelineFact::new(
                end,
                FactKind::ModelRequestEnd {
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
        // one repeated; C3 which facts are already published. Effects: E1 one
        // compaction fact unless published; E2 one start/end pair per request
        // unless its end is published; E3 a repeated row emits once.
        let prefix = snapshot(vec![(2, false), (3, true), (3, true)]);
        let roles = |published: &dyn Fn(&FactKey) -> bool| {
            run_observation_facts("thread", &prefix, published)
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
        assert_eq!(
            roles(&|_| false),
            vec![
                (FactRole::RunContextCompacted, compaction.clone()),
                (FactRole::ModelRequestStart, audit(2)),
                (FactRole::ModelRequestEnd, audit(2)),
                (FactRole::ModelRequestStart, audit(3)),
                (FactRole::ModelRequestEnd, audit(3)),
            ],
            "E1+E2+E3"
        );
        assert_eq!(
            roles(&|key| key.provenance == compaction || key.provenance == audit(2)),
            vec![
                (FactRole::ModelRequestStart, audit(3)),
                (FactRole::ModelRequestEnd, audit(3)),
            ],
            "E1+E2 published facts are not repeated"
        );
    }
}
