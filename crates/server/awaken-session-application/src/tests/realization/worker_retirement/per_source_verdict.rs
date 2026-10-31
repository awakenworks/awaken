//! T18-C03 (全部来源独立证明): Worker retirement qualifies only when EVERY
//! exact source independently proves retention.
//!
//! Oracles (groups JSON order):
//! - O0 each exact source satisfies the rule on its own;
//! - O1 a failure is never replaced by a successful prefix of the set;
//! - O2 an unknown answer keeps the Worker.
//!
//! Negative control: "checking only the last source, or only counting the
//! sources" must fail. It is executed: each row gives every subject its OWN
//! reported fact, and two mutant verdicts (last subject only; number of
//! reported facts equals the number of subjects) are computed from the same
//! report and shown to disagree with the production decision.
//!
//! | Row | Facts for subjects a, m, z | Production |
//! |---|---|---|
//! | V1 | missing, retained, retained | false |
//! | V2 | retained, retained, missing | false |
//! | V3 | retained, stale digest, retained | false |
//! | V4 | retained, retained and not retained, retained | false |
//! | V5 | retained, retained, retained | true |
use super::*;

static SOURCES: [&str; 3] = ["retire-each-a", "retire-each-m", "retire-each-z"];

/// One subject's part of the Worker report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fact {
    Retained,
    /// No fact for this subject: retention is unknown.
    Missing,
    /// A retained fact for an older digest of the same Session.
    Stale,
    /// Two facts for the exact subject that disagree.
    Contradicted,
}
use Fact::{Contradicted, Missing, Retained, Stale};

fn fact(subject: &SourceRetentionSubject, digest: &str, retained: bool) -> WorkerSourceRetention {
    WorkerSourceRetention {
        session_id: subject.session_id.to_string(),
        subject: digest.to_string(),
        retained,
    }
}

fn reported(
    subjects: &[SourceRetentionSubject],
    facts: &[Fact],
) -> BTreeSet<WorkerSourceRetention> {
    let mut report = BTreeSet::new();
    for subject in subjects {
        let index = SOURCES
            .iter()
            .position(|id| *id == subject.session_id.as_str())
            .expect("a fixture source");
        match &facts[index] {
            Retained => {
                report.insert(fact(subject, &subject.digest, true));
            }
            Missing => {}
            Stale => {
                report.insert(fact(subject, &format!("{}-stale", subject.digest), true));
            }
            Contradicted => {
                report.insert(fact(subject, &subject.digest, true));
                report.insert(fact(subject, &subject.digest, false));
            }
        }
    }
    report
}

/// NC mutant 1: trust whatever was reported for the last subject (z).
fn last_source_only(
    subjects: &[SourceRetentionSubject],
    report: &BTreeSet<WorkerSourceRetention>,
) -> bool {
    let last = subjects
        .iter()
        .max_by_key(|subject| subject.session_id.to_string())
        .unwrap();
    report
        .iter()
        .any(|fact| fact.session_id == last.session_id.as_str() && fact.retained)
}

/// NC mutant 2: qualify once as many facts were reported as there are subjects.
fn source_count_only(
    subjects: &[SourceRetentionSubject],
    report: &BTreeSet<WorkerSourceRetention>,
) -> bool {
    report.len() >= subjects.len()
}

#[tokio::test]
async fn worker_retirement_requires_each_exact_source_to_prove_retention_itself() {
    let rows: [(&str, [Fact; 3], bool); 5] = [
        ("V1", [Missing, Retained, Retained], false),
        ("V2", [Retained, Retained, Missing], false),
        ("V3", [Retained, Stale, Retained], false),
        ("V4", [Retained, Contradicted, Retained], false),
        ("V5", [Retained, Retained, Retained], true),
    ];
    let mut mutant_detected = (false, false);
    for (row, facts, expected) in rows {
        let identity = awaken_worker_contract::WorkerIdentity::new("each-source-worker", "boot", 1);
        let repo = sqlite_session_repository("per-source retirement").await;
        let mut roots = Vec::new();
        for id in SOURCES {
            create(repo.as_ref(), retained_root(id, &identity)).await;
            roots.push(repo.get(&SessionId::from(id)).await.unwrap());
        }
        let runtime = Arc::new(RecordingCleanupRuntime::default());
        *runtime.source_observation.lock().unwrap() =
            awaken_session_execution_contract::SessionSourceDispatchObservation::Settled;
        let app = configured_application(
            runtime.clone(),
            crate::SessionPersistence::new(repo.clone(), awaken_time::system_clock()),
            Arc::new(RecordingEnvironmentSource::default()),
            SessionApplicationConfiguration {
                execution_placement: SessionExecutionPlacement::RegisteredWorker,
                ..Default::default()
            },
            None,
        );
        let mut report = BTreeSet::new();
        let (result, subjects) = qualify_retirement(&app, &identity, |subjects| {
            report = reported(subjects, &facts);
            report.clone()
        })
        .await;
        let mut subject_ids = subjects
            .iter()
            .map(|subject| subject.session_id.to_string())
            .collect::<Vec<_>>();
        subject_ids.sort();
        assert_eq!(
            subject_ids, SOURCES,
            "{row} every exact source is a subject"
        );
        let observed = result.expect("retirement read");
        assert!(observed.occupied, "{row} occupancy is retained");
        assert_eq!(observed.sources_retained, expected, "{row}/O0-O2");
        if last_source_only(&subjects, &report) != expected {
            mutant_detected.0 = true;
        }
        if source_count_only(&subjects, &report) != expected {
            mutant_detected.1 = true;
        }
        for root in roots {
            assert_eq!(
                repo.get(&root.session_id).await.unwrap(),
                root,
                "{row} roots are unchanged"
            );
        }
        assert!(
            runtime.acknowledged_completed.lock().unwrap().is_empty(),
            "{row} no ACK"
        );
        assert!(
            runtime.intents.lock().unwrap().is_empty(),
            "{row} no preparation"
        );
    }
    assert_eq!(
        mutant_detected,
        (true, true),
        "NC both the last-source-only and the count-only verdicts are refuted"
    );
}
