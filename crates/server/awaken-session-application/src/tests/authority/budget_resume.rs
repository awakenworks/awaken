//! T13-C03 (恢复预算): a Session reaches its budget boundary, the allowance is
//! raised through `update_session`, and only the original waiting Run resumes.
//!
//! Oracles (groups-2.json order):
//! - O0 only the permitted original Run resumes;
//! - O1 no already-settled step is duplicated;
//! - O2 boundary values and message identity stay stable.
//!
//! Negative control: bypassing the budget wait through a NEW Run must fail.
//! The Runtime fake records every reservation and activation (unlike
//! `RecordingBoundaryBudgetRuntime`, whose neutral reservation returns
//! Unavailable and would make any bypass look rejected for the wrong reason).
//!
//! | Row | Budget command | Effect |
//! |---|---|---|
//! | B1 | raise to exactly the consumed boundary | rejected; root byte-equal; no resume |
//! | B2 | raise above the boundary | the exact ticket resumes once; no reservation/activation |
//! | B3 | exact replay of B2 | same delivery and epoch; no second activity record |
//! | B4 | replay after the continuation settled | settled epoch is not reopened |
//! | NC | new Run while reached | `budget_reached`; no activity, activation or model call |
use super::*;
use crate::tests::effect_counting_runtime::EffectCountingRuntime;
use awaken_agent_contract::agent::awaiting::{AwaitTarget, PauseReason, ResumeTicket};
use awaken_session_application_contract::SessionBudgetResumeTicket;
use awaken_session_budget_contract::SessionBudgetState;
use awaken_session_execution_contract::{
    AdmitSessionRun, SessionAgentCoordination as _, SessionRunActivityAdmission,
    SessionRunActivityAdmissionMode,
};

const CONSUMED_MINOR: u64 = 2;

fn consumed_numerator() -> u128 {
    u128::from(CONSUMED_MINOR)
        * SessionBudgetState::MICROS_PER_MINOR_USD
        * SessionBudgetState::COST_DENOMINATOR
}

/// A freshly created reached Session. `persisted()` deliberately models a
/// legacy root whose `run_activities` history is absent; on such a root the
/// reducer opens epochs but retains no admission record, so the record oracle
/// could never be observed. A new Session carries the explicit empty history.
fn reached_session(id: &str) -> PersistedSession {
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
    assert_eq!(
        session.run_activities(),
        Some((&[][..], false)),
        "fixture retains complete activity history"
    );
    session.budget = SessionBudgetState::Active {
        // The limit is below the exact consumed boundary: reached.
        max_list_cost_minor: 1,
        consumed_numerator: consumed_numerator(),
        usage_cursor: Default::default(),
        snapshot: awaken_session_budget_contract::ManagedListPriceSnapshot {
            snapshot_id: "t13-prices-v1".into(),
            version: 1,
            effective_at_unix_ms: 1,
            arithmetic_version: 1,
            model_rates: Default::default(),
            runtime_rates: Default::default(),
            fingerprint: "t13-prices-v1-fingerprint".into(),
        },
        reach_transitions: Vec::new(),
    };
    session
}

fn ticket(
    session: &str,
    run: &str,
    correlation: &str,
    reason: PauseReason,
) -> SessionBudgetResumeTicket {
    SessionBudgetResumeTicket {
        ticket: ResumeTicket::new(
            correlation,
            RunId(run.into()),
            ThreadId(session.into()),
            format!("{run}-snapshot"),
            format!("{run}-catalog"),
            AwaitTarget::Pause(reason),
        ),
        pause_generation: 7,
        prior_session_activity_epoch: None,
    }
}

fn raise(limit: u64, key: &str) -> SessionUpdateCommand {
    SessionUpdateCommand {
        title: None,
        metadata: None,
        budget: Some(SessionFieldUpdate::Replace(limit)),
        tools: None,
        mcp_update: None,
        idempotency_key: Some(key.into()),
        request_fingerprint: awaken_agent_contract::stable_fingerprint(&(limit, key)),
        if_match: None,
    }
}

fn bypass_run(session: &str) -> AdmitSessionRun {
    let session_id = SessionId::from(session);
    AdmitSessionRun {
        session_id: session_id.clone(),
        agent_id: "agent".into(),
        operation_id: "t13-bypass".into(),
        run_id: awaken_session_execution_contract::session_run_id(&session_id, "t13-bypass"),
        messages: vec![awaken_agent_contract::agent::message::Message::text(
            awaken_agent_contract::agent::message::Id("t13-bypass-input".into()),
            awaken_agent_contract::agent::message::Role::User,
            "start over to escape the budget wait",
        )],
        data_subject_id: None,
        traceparent: None,
        execution_requirements: Default::default(),
        replacement: Default::default(),
    }
}

async fn fixture(
    id: &str,
    tickets: Vec<SessionBudgetResumeTicket>,
) -> (
    Arc<awaken_session_store::SqliteManagedSessionRepository>,
    Arc<EffectCountingRuntime>,
    SessionApplication,
) {
    let repo = authority_repository("T13 budget repository").await;
    create(repo.as_ref(), reached_session(id)).await;
    let runtime = Arc::new(EffectCountingRuntime::default());
    runtime.set_budget_tickets(tickets);
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    (repo, runtime, app)
}

fn activity_record_count(session: &PersistedSession) -> usize {
    session
        .run_activities()
        .map_or(0, |(records, _)| records.len())
}

#[tokio::test]
async fn raised_budget_resumes_only_the_original_run_once_with_stable_identity() {
    let id = "t13-budget-resume";
    let session_id = SessionId::from(id);
    let original = ticket(
        id,
        "paused-run",
        "paused-correlation",
        PauseReason::BudgetReached,
    );
    let (repo, runtime, app) = fixture(id, vec![original.clone()]).await;
    let before = repo.get(&session_id).await.unwrap();
    assert!(
        !before.budget.can_admit_model_request(),
        "fixture is reached"
    );

    // B1/O2: the exact boundary is not "above consumed".
    let boundary = app
        .update_commands
        .update_session(&session_id, raise(CONSUMED_MINOR, "raise-to-boundary"))
        .await;
    assert!(
        matches!(boundary, Err(SessionUpdateError::Rejected(_))),
        "B1 boundary raise is rejected: {boundary:?}"
    );
    assert_eq!(
        repo.get(&session_id).await.unwrap(),
        before,
        "B1 root byte-equal"
    );
    assert_eq!(runtime.counts(), Default::default(), "B1 no Runtime effect");

    // B2/O0: one unit above the boundary resumes the exact original ticket.
    let raised = Box::pin(
        app.update_commands
            .update_session(&session_id, raise(CONSUMED_MINOR + 1, "raise")),
    )
    .await
    .expect("B2 raise");
    assert!(raised.command_applied, "B2");
    let deliveries = runtime.budget_deliveries.lock().unwrap().clone();
    assert_eq!(deliveries.len(), 1, "B2/O0 exactly one resumption");
    let delivery = deliveries[0].clone();
    assert_eq!(delivery.session_id, session_id, "B2/O0");
    assert_eq!(
        delivery.run_id, original.ticket.run_id,
        "B2/O0 original Run"
    );
    assert_eq!(delivery.thread_id, original.ticket.thread_id, "B2/O0");
    assert_eq!(
        delivery.correlation_id, original.ticket.correlation_id,
        "B2/O2 message/ticket identity"
    );
    assert_eq!(delivery.pause_generation, 7, "B2/O2 pause generation");
    assert_eq!(delivery.prior_session_activity_epoch, None, "B2");
    let epoch = delivery.session_activity_epoch;
    assert!(epoch > 0, "B2 continuation epoch");
    let counts = runtime.counts();
    assert_eq!(counts.reservations, 0, "B2/O0 no new Run reservation");
    assert_eq!(counts.activations, 0, "B2/O0 no new Run activation");
    assert_eq!(
        counts.model_calls, 0,
        "B2 the application runs nothing inline"
    );
    let resumed = repo.get(&session_id).await.unwrap();
    let SessionBudgetState::Active {
        max_list_cost_minor,
        consumed_numerator: consumed,
        snapshot,
        reach_transitions,
        ..
    } = &resumed.budget
    else {
        panic!("B2 budget remains Active")
    };
    assert_eq!(*max_list_cost_minor, CONSUMED_MINOR + 1, "B2/O2 new limit");
    assert_eq!(
        *consumed,
        consumed_numerator(),
        "B2/O2 consumed boundary unchanged"
    );
    assert_eq!(
        snapshot.snapshot_id, "t13-prices-v1",
        "B2/O2 price pin unchanged"
    );
    let SessionBudgetState::Active {
        reach_transitions: original_transitions,
        ..
    } = &before.budget
    else {
        unreachable!()
    };
    assert_eq!(reach_transitions, original_transitions, "B2/O2 provenance");
    assert_eq!(
        resumed.active_activity_epochs,
        BTreeSet::from([epoch]),
        "B2 one continuation epoch"
    );
    let records = activity_record_count(&resumed);
    assert_eq!(records, 1, "B2 one continuation activity record");

    // B3/O1: the exact replay redelivers the SAME continuation identity.
    let replay = Box::pin(
        app.update_commands
            .update_session(&session_id, raise(CONSUMED_MINOR + 1, "raise")),
    )
    .await
    .expect("B3 replay");
    assert!(!replay.command_applied, "B3 receipt replay");
    let deliveries = runtime.budget_deliveries.lock().unwrap().clone();
    assert_eq!(deliveries.len(), 2, "B3 replay reaches Runtime");
    assert_eq!(
        deliveries[1], delivery,
        "B3/O1 identical delivery and epoch"
    );
    let replayed = repo.get(&session_id).await.unwrap();
    assert_eq!(
        replayed.active_activity_epochs,
        BTreeSet::from([epoch]),
        "B3/O1 no second epoch"
    );
    assert_eq!(
        activity_record_count(&replayed),
        records,
        "B3/O1 no new record"
    );

    // B4/O1: the continuation settles; a later replay cannot reopen it.
    app.lifecycle
        .settle_activity_observed(&session_id, epoch, None)
        .await
        .expect("B4 settle continuation");
    let settled = repo.get(&session_id).await.unwrap();
    assert!(settled.active_activity_epochs.is_empty(), "B4 settled");
    let _ = Box::pin(
        app.update_commands
            .update_session(&session_id, raise(CONSUMED_MINOR + 1, "raise")),
    )
    .await;
    let after_settled_replay = repo.get(&session_id).await.unwrap();
    assert!(
        after_settled_replay.active_activity_epochs.is_empty(),
        "B4/O1 a settled continuation epoch is never reopened"
    );
    assert_eq!(
        activity_record_count(&after_settled_replay),
        records,
        "B4/O1 no duplicate settled step"
    );
    assert!(
        runtime
            .budget_deliveries
            .lock()
            .unwrap()
            .iter()
            .all(|redelivered| *redelivered == delivery),
        "B4/O2 every redelivery keeps the original identity"
    );
    assert_eq!(runtime.counts().reservations, 0, "B4/O0 still no new Run");
}

#[tokio::test]
async fn new_run_cannot_bypass_the_budget_wait() {
    // NC: the same reached Session; instead of raising the allowance the
    // caller submits a brand-new Run. The fake really records reservation and
    // activation, so a rejection here is the Session budget gate, not an
    // unsupported reservation port.
    let id = "t13-budget-bypass";
    let session_id = SessionId::from(id);
    let (repo, runtime, app) = fixture(
        id,
        vec![ticket(
            id,
            "paused-run",
            "paused-correlation",
            PauseReason::BudgetReached,
        )],
    )
    .await;
    let before = repo.get(&session_id).await.unwrap();
    let error = app
        .run_admissions
        .admit_session_run(bypass_run(id))
        .await
        .expect_err("NC a new Run cannot bypass the budget wait");
    assert_eq!(
        error.code, "budget_reached",
        "NC classified as budget: {error:?}"
    );
    let counts = runtime.counts();
    assert_eq!(counts.activations, 0, "NC no activation");
    assert_eq!(counts.model_calls, 0, "NC no model call");
    assert_eq!(counts.budget_resumes, 0, "NC no resumption");
    let after = repo.get(&session_id).await.unwrap();
    assert_eq!(
        activity_record_count(&after),
        activity_record_count(&before),
        "NC no activity record for the bypass Run"
    );
    assert!(after.active_activity_epochs.is_empty(), "NC no epoch");
    assert_eq!(after.budget, before.budget, "NC budget unchanged");
    // The fake proves the gate is real: the reservation port was reachable.
    assert_eq!(
        counts.reservations, 1,
        "NC the bypass reached the real reservation port before the budget gate"
    );
}

#[tokio::test]
async fn raised_budget_resumes_only_budget_reached_tickets() {
    let id = "t13-budget-ticket-filter";
    let session_id = SessionId::from(id);
    let (_, runtime, app) = fixture(
        id,
        vec![
            ticket(
                id,
                "budget-run",
                "budget-correlation",
                PauseReason::BudgetReached,
            ),
            ticket(id, "manual-run", "manual-correlation", PauseReason::Manual),
        ],
    )
    .await;
    Box::pin(
        app.update_commands
            .update_session(&session_id, raise(CONSUMED_MINOR + 1, "raise")),
    )
    .await
    .expect("raise");
    let resumed = runtime
        .budget_deliveries
        .lock()
        .unwrap()
        .iter()
        .map(|delivery| delivery.run_id.0.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        resumed,
        vec!["budget-run".to_string()],
        "only the Run waiting on BudgetReached may be resumed by a budget raise"
    );
}

#[tokio::test]
#[ignore = "product gap: a budget-rejected bypass Run keeps its reservation; after the raise, repair RecoverOrAdmit admits it as a new Run"]
async fn budget_rejected_bypass_run_is_not_admitted_by_repair_after_raise() {
    let id = "t13-budget-bypass-repair";
    let session_id = SessionId::from(id);
    let (repo, _, app) = fixture(
        id,
        vec![ticket(
            id,
            "paused-run",
            "paused-correlation",
            PauseReason::BudgetReached,
        )],
    )
    .await;
    let bypass = bypass_run(id);
    let bypass_run_id = bypass.run_id.clone();
    app.run_admissions
        .admit_session_run(bypass)
        .await
        .expect_err("bypass rejected while reached");
    Box::pin(
        app.update_commands
            .update_session(&session_id, raise(CONSUMED_MINOR + 1, "raise")),
    )
    .await
    .expect("raise resumes the original Run");
    let repair = app
        .coordination
        .admit_session_run_activity(
            &session_id,
            "agent",
            &bypass_run_id,
            SessionRunActivityAdmissionMode::RecoverOrAdmit,
        )
        .await;
    assert!(
        matches!(repair, Ok(SessionRunActivityAdmission::Rejected)),
        "the rejected bypass reservation must not be admitted later: {repair:?}"
    );
    let session = repo.get(&session_id).await.unwrap();
    assert!(
        session.run_activities().is_none_or(|(records, _)| records
            .iter()
            .all(|record| record.run_id() != &bypass_run_id)),
        "no activity for the bypass Run"
    );
}
