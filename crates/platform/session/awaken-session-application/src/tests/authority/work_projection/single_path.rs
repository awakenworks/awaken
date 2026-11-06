//! T08-C01 (唯一投影路径): durable Session truth reaches the WorkQueue only
//! through `dispatch_session_work`, and only for an eligible Session.
//!
//! Oracles (groups JSON order):
//! - O0 only an eligible Session produces one stable Work identity;
//! - O1 a repeated projection adds nothing.
//!
//! Negative control: "another entrypoint quietly enqueues, or a terminal Session
//! is still delivered" must fail. It is executed: (a) a mutant enqueue of the
//! terminal Session into the same queue is detected by the same
//! `projection_violations` checker that accepts the production ledger; (b) a
//! source scan proves `.enqueue_session(` has exactly one production caller.
//!
//! Unlike `RecordingEnvironmentSource` (a `BTreeSet`), this queue records every
//! call, so a duplicate enqueue cannot be hidden by the fixture.
//!
//! | Row | Session | Path | Expected |
//! |---|---|---|---|
//! | P1 | self-hosted, idle | reconcile | one call, one identity |
//! | P2 | same | reconcile again + direct dispatch | same identity, queue size 1 |
//! | P3 | not self-hosted / terminal | reconcile + direct dispatch | no call, `Ok(false)` |
//! | NC1 | terminal | mutant enqueue | detected |
//! | NC2 | any | other production `.enqueue_session(` caller | none exists |
use super::*;
use awaken_session_application_contract::SessionWorkerAssignment;
use std::collections::BTreeMap;

/// An idempotent queue model: one Work identity per (environment, Session),
/// with every call kept in order.
#[derive(Default)]
struct LedgerQueue {
    calls: Mutex<Vec<(String, String, String)>>,
    identities: Mutex<BTreeMap<(String, String), String>>,
}

#[async_trait::async_trait]
impl SessionWorkerAssignment for LedgerQueue {
    async fn enqueue_session(
        &self,
        environment_id: &str,
        session_id: &SessionId,
    ) -> Result<String, RunError> {
        let identity = {
            let mut identities = self.identities.lock().unwrap();
            let next = format!("work-{}", identities.len() + 1);
            identities
                .entry((environment_id.to_string(), session_id.to_string()))
                .or_insert(next)
                .clone()
        };
        self.calls.lock().unwrap().push((
            environment_id.to_string(),
            session_id.to_string(),
            identity.clone(),
        ));
        Ok(identity)
    }

    async fn wake_session(
        &self,
        _environment_id: &str,
        _session_id: &SessionId,
    ) -> Result<String, RunError> {
        panic!("Work projection must not revive a Session")
    }

    async fn retire_session(
        &self,
        _environment_id: &str,
        _session_id: &SessionId,
    ) -> Result<(), RunError> {
        panic!("Work projection must not retire a Session")
    }
}

/// The O0/O1 checker shared by the production ledger and the mutant.
fn projection_violations(
    calls: &[(String, String, String)],
    eligible: &[(&str, &str)],
) -> Vec<String> {
    let mut violations = Vec::new();
    let mut identity_by_session = BTreeMap::new();
    for (environment, session, identity) in calls {
        if !eligible
            .iter()
            .any(|(env, id)| env == environment && id == session)
        {
            violations.push(format!("ineligible delivery {environment}/{session}"));
        }
        let first = identity_by_session
            .entry(session.clone())
            .or_insert_with(|| identity.clone());
        if first != identity {
            violations.push(format!("{session} received a second identity {identity}"));
        }
    }
    violations
}

#[tokio::test]
async fn work_projection_has_one_path_one_identity_and_skips_terminal_truth() {
    let repo = authority_repository("single-path repository").await;
    create(
        repo.as_ref(),
        persisted("single-path-eligible", true, "idle"),
    )
    .await;
    create(repo.as_ref(), persisted("single-path-local", false, "idle")).await;
    create(
        repo.as_ref(),
        persisted("single-path-terminal", true, "terminated"),
    )
    .await;
    let queue = LedgerQueue::default();
    let eligible = [("env-worker", "single-path-eligible")];

    // P1/O0.
    let first = reconcile_work_dispatches(repo.as_ref(), &queue, 0).await;
    assert!(first.failures.is_empty(), "P1 {first:?}");
    let after_first = queue.calls.lock().unwrap().clone();
    assert_eq!(
        after_first,
        vec![(
            "env-worker".to_string(),
            "single-path-eligible".to_string(),
            "work-1".to_string()
        )],
        "P1/O0 exactly one eligible delivery"
    );

    // P2/O1: a replay through both production paths keeps the identity.
    let replay = reconcile_work_dispatches(repo.as_ref(), &queue, 0).await;
    assert!(replay.failures.is_empty(), "P2 {replay:?}");
    let eligible_root = repo
        .get(&SessionId::from("single-path-eligible"))
        .await
        .unwrap();
    assert!(
        crate::work_assignments::dispatch_session_work(&queue, &eligible_root)
            .await
            .expect("P2 direct dispatch"),
        "P2 an eligible Session is projected"
    );
    let calls = queue.calls.lock().unwrap().clone();
    assert!(
        calls.len() >= 2,
        "P2 the replay really re-entered the idempotent port"
    );
    assert_eq!(
        projection_violations(&calls, &eligible),
        Vec::<String>::new(),
        "P2/O0/O1 production ledger"
    );
    assert_eq!(
        queue.identities.lock().unwrap().len(),
        1,
        "P2/O1 the repeat adds no Work"
    );

    // P3: ineligible truth never reaches the queue, through either path.
    for id in ["single-path-local", "single-path-terminal"] {
        let root = repo.get(&SessionId::from(id)).await.unwrap();
        assert!(
            !crate::work_assignments::dispatch_session_work(&queue, &root)
                .await
                .expect("P3 direct dispatch"),
            "P3 {id} is not projected"
        );
    }
    assert_eq!(queue.calls.lock().unwrap().len(), calls.len(), "P3 no call");

    // NC1: a second entrypoint that still delivers the terminal Session.
    queue
        .enqueue_session("env-worker", &SessionId::from("single-path-terminal"))
        .await
        .unwrap();
    let mutated = queue.calls.lock().unwrap().clone();
    assert_eq!(
        projection_violations(&mutated, &eligible),
        vec!["ineligible delivery env-worker/single-path-terminal".to_string()],
        "NC1 the checker detects a terminal delivery"
    );
}

fn production_sources(directory: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(directory).expect("read source directory") {
        let path = entry.expect("source entry").path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if path.is_dir() {
            if name != "tests" {
                production_sources(&path, found);
            }
        } else if name.ends_with(".rs") && name != "tests.rs" && !name.ends_with("_tests.rs") {
            found.push(path);
        }
    }
}

#[test]
fn only_the_projection_path_enqueues_session_work() {
    // NC2: no other production entrypoint calls the WorkQueue enqueue port.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut sources = Vec::new();
    production_sources(&root, &mut sources);
    assert!(sources.len() > 10, "the scan reached the crate sources");
    let callers = sources
        .iter()
        .filter(|path| {
            std::fs::read_to_string(path)
                .expect("read source")
                .contains(".enqueue_session(")
        })
        .map(|path| {
            path.strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        callers,
        vec!["work_assignments.rs".to_string()],
        "NC2 exactly one production enqueue caller"
    );
}
