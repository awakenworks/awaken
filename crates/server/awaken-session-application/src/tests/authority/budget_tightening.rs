//! T13-C02 (执行中收紧): between two model effects of a live Run the Session
//! budget is tightened, and the next controlled effect must obey the CURRENT
//! dynamic authority while the frozen model and price pins stay unchanged.
//!
//! Oracles (groups-2.json order):
//! - O0 the frozen execution model and price snapshot are preserved;
//! - O1 the next controlled effect is rejected (or waits) under the current
//!   dynamic authority.
//!
//! Negative control: a budget check made only once at creation must not pass.
//! It is executed, not described: a twin Session runs the same production gate
//! with the same usage but only its creation-time limit, and the O1 check is
//! shown to reject that decision.
//!
//! The gate under test is the production
//! `SessionAgentCoordination::admit_session_model_request`, which reconciles
//! cumulative usage into the root at one CAS and decides from the persisted
//! budget. Prices: 1 input token = 1 list micro-USD, so 10_000 tokens = 1 minor.
//!
//! | Row | Root | Command at the barrier | Usage at next gate | Effect |
//! |---|---|---|---|---|
//! | G1 | Idle, live Run boundary | none | 0 / 25_000 | admitted twice |
//! | G2 | Idle, live Run boundary | tighten 10 -> 3 minor | 25_000 | admitted; pins and consumed unchanged |
//! | G3 | Idle, live Run boundary | (after G2) | 30_000 | denied; one reach transition |
//! | NC | twin, creation limit only | none | 30_000 | admitted -> O1 check rejects it |
//! | R1 | Running root execution | tighten 10 -> 3 minor | 30_000 | admitted (ADR-0103); next request denied |
//!
//! Scope: component (L1) proof of the application gate with fake Runtime
//! boundaries; it does not sign off the L3 native-executor substrate, and no
//! dynamic egress limit is updatable through a Session command (egress is
//! bounded by lease revocation in the provisioning contract instead).
use super::*;
use awaken_session_contract::{
    ManagedListPriceSnapshot, ManagedTokenListRates, SessionBudgetState, SessionModelUsage,
    SessionUsage,
};

const CREATION_LIMIT_MINOR: u64 = 10;
const TIGHTENED_LIMIT_MINOR: u64 = 3;
/// 2.5 minor: below the tightened limit, so the tightening is admissible.
const FIRST_STEP_TOKENS: u64 = 25_000;
/// 3 minor: at the tightened cap, far below the creation-time cap.
const SECOND_STEP_TOKENS: u64 = 30_000;

fn price_snapshot() -> ManagedListPriceSnapshot {
    ManagedListPriceSnapshot {
        snapshot_id: "t13-c02-prices-v1".into(),
        version: 1,
        effective_at_unix_ms: 1,
        arithmetic_version: 1,
        model_rates: std::collections::BTreeMap::from([(
            "model".into(),
            ManagedTokenListRates {
                input_micros_per_million: 1_000_000,
                ..Default::default()
            },
        )]),
        runtime_rates: Default::default(),
        fingerprint: "t13-c02-prices-v1-fingerprint".into(),
    }
}

fn usage(tokens: u64) -> SessionUsage {
    SessionUsage {
        input_tokens: tokens,
        by_model: std::collections::BTreeMap::from([(
            "model".into(),
            SessionModelUsage {
                input_tokens: tokens,
                ..Default::default()
            },
        )]),
        ..Default::default()
    }
}

fn budgeted_session(id: &str, execution: &str) -> PersistedSession {
    let mut session = persisted(id, false, execution);
    session.budget = SessionBudgetState::active(CREATION_LIMIT_MINOR, price_snapshot());
    session
}

fn tighten(key: &str) -> SessionUpdateCommand {
    SessionUpdateCommand {
        title: None,
        metadata: None,
        budget: Some(SessionFieldUpdate::Replace(TIGHTENED_LIMIT_MINOR)),
        tools: None,
        mcp_update: None,
        idempotency_key: Some(key.into()),
        request_fingerprint: awaken_agent_contract::stable_fingerprint(&(
            TIGHTENED_LIMIT_MINOR,
            key,
        )),
        if_match: None,
    }
}

/// The frozen pins O0 protects: execution model and list-price snapshot.
fn frozen_pins(session: &PersistedSession) -> (String, ManagedListPriceSnapshot) {
    (
        session
            .frozen_baseline()
            .expect("fixture is frozen")
            .execution_model_ref
            .clone(),
        session
            .budget
            .price_snapshot()
            .expect("fixture budget is active")
            .clone(),
    )
}

/// O1: the gate's decision for the next controlled effect equals what the
/// CURRENT persisted authority admits.
fn obeys_current_authority(decision: bool, current: &SessionBudgetState) -> bool {
    decision == current.can_admit_model_request()
}

async fn gate(app: &SessionApplication, session: &str, run: &RunId) -> bool {
    awaken_session_execution_contract::SessionAgentCoordination::admit_session_model_request(
        app.coordination.as_ref(),
        &SessionId::from(session),
        &ThreadId(session.into()),
        run,
    )
    .await
    .expect("model-request gate decision")
}

async fn fixture(
    sessions: &[(&str, &str)],
) -> (
    Arc<awaken_session_store::SqliteManagedSessionRepository>,
    Arc<RecordingBoundaryBudgetRuntime>,
    SessionApplication,
) {
    let repo = authority_repository("T13-C02 budget repository").await;
    let runtime = Arc::new(RecordingBoundaryBudgetRuntime::new(Default::default()));
    for (id, execution) in sessions {
        create(repo.as_ref(), budgeted_session(id, execution)).await;
        runtime.commit_boundary(
            id,
            &ThreadId((*id).into()),
            &RunId(format!("{id}-run")),
            RunState::Running,
            "",
        );
    }
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    (repo, runtime, app)
}

#[tokio::test]
async fn tightened_budget_governs_the_next_model_request_and_keeps_frozen_pins() {
    let tightened = "t13-c02-tightened";
    let creation_only = "t13-c02-creation-only";
    let tightened_run = RunId(format!("{tightened}-run"));
    let creation_only_run = RunId(format!("{creation_only}-run"));
    let (repo, runtime, app) = fixture(&[(tightened, "idle"), (creation_only, "idle")]).await;

    // G1: the live Run's first two model requests are admitted.
    for tokens in [0, FIRST_STEP_TOKENS] {
        runtime.set_root_usage(usage(tokens));
        assert!(gate(&app, tightened, &tightened_run).await, "G1 {tokens}");
        assert!(
            gate(&app, creation_only, &creation_only_run).await,
            "G1 twin {tokens}"
        );
    }
    let before = repo.get(&SessionId::from(tightened)).await.unwrap();
    let pins_before = frozen_pins(&before);
    assert!(!pins_before.0.is_empty(), "G1 frozen execution model");
    assert_eq!(
        before.budget.max_list_cost_minor(),
        Some(CREATION_LIMIT_MINOR),
        "G1 creation limit"
    );

    // G2: at the barrier before the next model effect, tighten through the
    // production update command.
    let outcome = Box::pin(
        app.update_commands
            .update_session(&SessionId::from(tightened), tighten("tighten")),
    )
    .await
    .expect("G2 tightening above consumed is accepted");
    assert!(outcome.command_applied, "G2 applied");
    let after_tighten = repo.get(&SessionId::from(tightened)).await.unwrap();
    assert_eq!(
        after_tighten.budget.max_list_cost_minor(),
        Some(TIGHTENED_LIMIT_MINOR),
        "G2 current dynamic limit"
    );
    assert_eq!(
        frozen_pins(&after_tighten),
        pins_before,
        "G2/O0 frozen model and price pins"
    );
    assert_eq!(
        after_tighten.budget.usage_cursor(),
        before.budget.usage_cursor(),
        "G2/O0 accounting is not recharged under the new limit"
    );
    assert!(
        gate(&app, tightened, &tightened_run).await,
        "G2 already-settled usage below the new cap stays admitted"
    );

    // G3/O1: the next step settles at the tightened cap.
    runtime.set_root_usage(usage(SECOND_STEP_TOKENS));
    let decision = gate(&app, tightened, &tightened_run).await;
    let current = repo.get(&SessionId::from(tightened)).await.unwrap();
    assert!(!decision, "G3/O1 next model request is denied");
    assert!(
        obeys_current_authority(decision, &current.budget),
        "G3/O1 decision follows the current authority"
    );
    assert_eq!(
        current.budget.reach_transitions().len(),
        1,
        "G3/O1 exactly one reach transition"
    );
    assert_eq!(
        frozen_pins(&current),
        pins_before,
        "G3/O0 pins after denial"
    );
    assert!(
        !gate(&app, tightened, &tightened_run).await,
        "G3 replayed gate remains denied"
    );
    assert_eq!(
        repo.get(&SessionId::from(tightened))
            .await
            .unwrap()
            .budget
            .reach_transitions()
            .len(),
        1,
        "G3 replay appends no second transition"
    );

    // NC: the same production gate fed only the creation-time authority (the
    // twin was never tightened) admits the identical usage. Judged against the
    // tightened Session's current authority, the O1 check must reject it.
    let creation_only_decision = gate(&app, creation_only, &creation_only_run).await;
    assert!(
        creation_only_decision,
        "NC creation-only authority admits 3 of 10 minor"
    );
    assert!(
        !obeys_current_authority(creation_only_decision, &current.budget),
        "NC the O1 check detects a creation-only budget decision"
    );
    let twin = repo.get(&SessionId::from(creation_only)).await.unwrap();
    assert!(
        twin.budget.reach_transitions().is_empty(),
        "NC twin never reached"
    );
    assert_eq!(
        frozen_pins(&twin),
        pins_before,
        "NC twin shares the same pins"
    );
}

#[tokio::test]
async fn running_session_budget_tightening_governs_the_next_model_request() {
    let id = "t13-c02-running";
    let run = RunId(format!("{id}-run"));
    let (repo, runtime, app) = fixture(&[(id, "running")]).await;
    for tokens in [0, FIRST_STEP_TOKENS] {
        runtime.set_root_usage(usage(tokens));
        assert!(gate(&app, id, &run).await, "R1 admitted {tokens}");
    }
    let pins_before = frozen_pins(&repo.get(&SessionId::from(id)).await.unwrap());

    if let Err(error) = Box::pin(
        app.update_commands
            .update_session(&SessionId::from(id), tighten("tighten-running")),
    )
    .await
    {
        panic!("R1 tightening a Running Session before its next effect is refused: {error}");
    }
    runtime.set_root_usage(usage(SECOND_STEP_TOKENS));
    let decision = gate(&app, id, &run).await;
    let current = repo.get(&SessionId::from(id)).await.unwrap();
    assert!(!decision, "R1/O1 next model request is denied");
    assert!(obeys_current_authority(decision, &current.budget), "R1/O1");
    assert_eq!(frozen_pins(&current), pins_before, "R1/O0");
}

mod frozen_versus_current;
