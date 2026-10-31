//! T33-C02 (重试原身份): fail the Run admission before its activity commit,
//! lose the response of a committed admission, and leave the effect unknown,
//! then retry through the production `admit_session_run` entrypoint over a
//! file SQLite Session store.
//!
//! Oracles (groups-5.json order):
//! - O0 a determinately unadmitted request may be admitted again;
//! - O1 a committed request returns its original receipt (epoch);
//! - O2 an unknown effect first observes the original operation.
//!
//! | Row | Fault at the activity CAS | Caller sees | Retry (same operation) | Expected root |
//! |---|---|---|---|---|
//! | Q1 | outage before commit (store proves nothing applied) | `Err` `session_run_not_admitted` | admitted | exactly one record, the retry's epoch |
//! | Q2 | commit, response reported as conflict | `Ok` (the call observes its own commit) | same epoch | exactly one record |
//! | Q3 | commit, response lost as `Unavailable` | `Err` `session_run_admission_pending` (unknown) | the committed epoch | exactly one record, committed before the retry |
//! | NC | Q3, but the client mints a NEW operation on `Unavailable` | - | - | two epochs: detected by the same checker |
//!
//! The Q1/Q3 caller errors carry distinct classes (ADR-0094 D3 as amended):
//! "not admitted" and "unknown"; both retry only the same operation.
use super::*;
use crate::tests::effect_counting_runtime::EffectCountingRuntime;
use crate::tests::run_admission::response_loss_store::ResponseLossStore;

const ACTIVITY_CAS: &str = "managed:session-activity:";

fn fresh_session(id: &str) -> PersistedSession {
    let legacy = persisted(id, false, "idle");
    let mut session = PersistedSession::frozen_with_budget(
        id,
        legacy.frozen_baseline().unwrap().clone(),
        legacy.resources,
        legacy.mcp,
        legacy.title,
        legacy.metadata,
        legacy.tools,
        legacy.budget,
    );
    session.execution = legacy.execution;
    session
}

fn admit_command(session: &str, operation: &str) -> AdmitSessionRun {
    let session_id = SessionId::from(session);
    AdmitSessionRun {
        session_id: session_id.clone(),
        agent_id: "agent".into(),
        operation_id: operation.into(),
        run_id: awaken_session_execution_contract::session_run_id(&session_id, operation),
        messages: vec![awaken_agent_contract::agent::message::Message::text(
            awaken_agent_contract::agent::message::Id(format!("{operation}-input")),
            awaken_agent_contract::agent::message::Role::User,
            "retry me with my original identity",
        )],
        data_subject_id: None,
        traceparent: None,
        execution_requirements: Default::default(),
        replacement: Default::default(),
        private_task_input: None,
    }
}

fn delivery_epoch(admitted: &AdmittedSessionRun) -> u64 {
    match admitted {
        AdmittedSessionRun::Reserved(delivery)
        | AdmittedSessionRun::AlreadyReserved(delivery)
        | AdmittedSessionRun::AlreadyActivated(delivery) => delivery.session_activity_epoch,
        other => panic!("admission without a delivery: {other:?}"),
    }
}

/// The shared checker: one logical request owns exactly one durable activity
/// admission. Returns its (run id, epoch).
fn single_admission(session: &PersistedSession) -> Result<(String, u64), String> {
    let records = session
        .run_activities()
        .map(|(records, _)| {
            records
                .iter()
                .map(|record| (record.run_id().0.clone(), record.activity_epoch().get()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    match records.as_slice() {
        [single] => Ok(single.clone()),
        other => Err(format!(
            "one request admitted {} times: {other:?}",
            other.len()
        )),
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    durable: Arc<awaken_session_store::SqliteManagedSessionRepository>,
    loss: Arc<ResponseLossStore>,
    faulting: Arc<FaultingSessionRepository>,
    runtime: Arc<EffectCountingRuntime>,
    app: SessionApplication,
}

async fn fixture(sessions: &[&str]) -> Fixture {
    let directory = tempfile::tempdir().expect("T33 SQLite directory");
    let durable = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open(
            directory.path().join("t33-retry.db").to_str().unwrap(),
        )
        .await
        .expect("T33 file SQLite"),
    );
    for session in sessions {
        create(durable.as_ref(), fresh_session(session)).await;
    }
    let loss = Arc::new(ResponseLossStore::new(durable.clone()));
    let faulting = Arc::new(FaultingSessionRepository::new(loss.clone()));
    let runtime = Arc::new(EffectCountingRuntime::default());
    let app = application_with_runtime(
        runtime.clone(),
        faulting.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    Fixture {
        _directory: directory,
        durable,
        loss,
        faulting,
        runtime,
        app,
    }
}

impl Fixture {
    async fn root(&self, session: &str) -> PersistedSession {
        self.durable.get(&SessionId::from(session)).await.unwrap()
    }

    async fn admit(&self, session: &str, operation: &str) -> Result<AdmittedSessionRun, RunError> {
        self.app
            .run_admissions
            .admit_session_run(admit_command(session, operation))
            .await
    }

    /// Retry the SAME operation; its durable reservation now answers
    /// `AlreadyReserved`, as the production Runtime does for a known Run.
    async fn retry(&self, session: &str, operation: &str) -> Result<AdmittedSessionRun, RunError> {
        self.runtime.already_reserved.store(true, Ordering::SeqCst);
        let result = self.admit(session, operation).await;
        self.runtime.already_reserved.store(false, Ordering::SeqCst);
        result
    }
}

#[tokio::test]
async fn retries_keep_the_original_admission_identity() {
    let fixture = fixture(&["t33-q1", "t33-q2", "t33-q3"]).await;

    // Q1/O0: the activity CAS never ran; the retry admits once.
    fixture.loss.commit_unavailable_once(ACTIVITY_CAS);
    let failed = fixture.admit("t33-q1", "q1").await;
    assert!(failed.is_err(), "Q1 the outage is reported");
    assert!(
        fixture
            .root("t33-q1")
            .await
            .run_activities()
            .is_some_and(|(records, _)| records.is_empty()),
        "Q1/O0 determinately not admitted"
    );
    let admitted = fixture
        .retry("t33-q1", "q1")
        .await
        .expect("Q1/O0 re-admission");
    let (run, epoch) = single_admission(&fixture.root("t33-q1").await).expect("Q1/O0");
    assert_eq!(
        epoch,
        delivery_epoch(&admitted),
        "Q1/O0 the retry owns the one epoch"
    );
    assert_eq!(
        run,
        admit_command("t33-q1", "q1").run_id.0,
        "Q1 original Run identity"
    );

    // Q2/O1: the commit's response is reported as a conflict; the call
    // observes its own committed receipt instead of opening a second one.
    fixture.faulting.commit_then_conflict_once(ACTIVITY_CAS);
    let first = fixture
        .admit("t33-q2", "q2")
        .await
        .expect("Q2/O1 the call recovers its committed receipt");
    let (_, epoch) = single_admission(&fixture.root("t33-q2").await).expect("Q2/O1");
    assert_eq!(delivery_epoch(&first), epoch, "Q2/O1 original epoch");
    let replay = fixture.retry("t33-q2", "q2").await.expect("Q2 exact retry");
    assert_eq!(
        delivery_epoch(&replay),
        epoch,
        "Q2/O1 retry returns the original epoch"
    );
    assert_eq!(
        single_admission(&fixture.root("t33-q2").await),
        Ok((admit_command("t33-q2", "q2").run_id.0, epoch)),
        "Q2/O1 still one record"
    );

    // Q3/O2: the commit happened but the caller only sees Unavailable. The
    // durable record exists BEFORE any retry; the retry observes it.
    fixture.loss.commit_then_unavailable_once(ACTIVITY_CAS);
    let unknown = fixture.admit("t33-q3", "q3").await;
    assert!(unknown.is_err(), "Q3 the effect is unknown to the caller");
    let (_, committed) = single_admission(&fixture.root("t33-q3").await)
        .expect("Q3/O2 the original operation is observable before retry");
    let observed = fixture.retry("t33-q3", "q3").await.expect("Q3 retry");
    assert_eq!(
        delivery_epoch(&observed),
        committed,
        "Q3/O2 the retry adopts the committed epoch"
    );
    assert_eq!(
        single_admission(&fixture.root("t33-q3").await),
        Ok((admit_command("t33-q3", "q3").run_id.0, committed)),
        "Q3/O2 no second admission"
    );
    assert_eq!(
        fixture.runtime.counts().activations,
        0,
        "admission activates nothing"
    );
}

#[tokio::test]
async fn minting_a_new_operation_on_unavailable_is_detected() {
    // NC: the same Q3 fault, but a client that treats every Unavailable as
    // "not admitted" and retries with a NEW operation identity.
    let fixture = fixture(&["t33-nc"]).await;
    fixture.loss.commit_then_unavailable_once(ACTIVITY_CAS);
    assert!(fixture.admit("t33-nc", "nc-original").await.is_err());
    let mutant = fixture
        .admit("t33-nc", "nc-minted-after-5xx")
        .await
        .expect("the mutant's fresh operation is admitted");
    let root = fixture.root("t33-nc").await;
    let detected = single_admission(&root).expect_err("NC the checker detects two epochs");
    assert!(detected.contains("2 times"), "NC {detected}");
    assert_eq!(
        root.run_activities().map(|(records, _)| records.len()),
        Some(2),
        "NC two durable admissions for one logical request"
    );
    assert!(delivery_epoch(&mutant) > 0);
}

#[tokio::test]
async fn unadmitted_and_unknown_failures_are_classified_apart() {
    let fixture = fixture(&["t33-gap-q1", "t33-gap-q3"]).await;
    fixture.loss.commit_unavailable_once(ACTIVITY_CAS);
    let not_admitted = fixture
        .admit("t33-gap-q1", "gap-q1")
        .await
        .expect_err("pre-commit outage");
    fixture.loss.commit_then_unavailable_once(ACTIVITY_CAS);
    let unknown = fixture
        .admit("t33-gap-q3", "gap-q3")
        .await
        .expect_err("lost committed response");
    assert_ne!(
        (not_admitted.kind, not_admitted.code.clone()),
        (unknown.kind, unknown.code.clone()),
        "a caller must be able to tell 'retry may admit' from 'observe the original first'"
    );
    assert_eq!(
        not_admitted.code, "session_run_not_admitted",
        "Q1 the store proved nothing was applied"
    );
    assert_eq!(
        unknown.code, "session_run_admission_pending",
        "Q3 the outcome is unknown until observed"
    );
}
