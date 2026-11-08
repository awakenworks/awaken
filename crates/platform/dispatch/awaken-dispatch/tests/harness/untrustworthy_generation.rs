//! One decision table for a claim scan that reaches a dispatch whose persisted
//! lease generation has no successor, run against every SQL adapter. The
//! adapter supplies only how to write the untrustworthy generation; the
//! reference store runs the same rows as a unit test beside its private state.
//!
//! | Row | head of the queue (oldest)                        | scan                    | effect                                                    |
//! |-----|---------------------------------------------------|-------------------------|-----------------------------------------------------------|
//! | U1  | expired lease, retry budget spent, epoch exhausted | `claim_retry_exhausted` | `Ok(None)`, never `Err`; the row is dead-lettered          |
//! | U2  | U1's healthy Run behind it                         | `claim`, same cycle     | claimed                                                   |
//! | U3  | expired lease, budget left, epoch exhausted        | `claim`                 | dead-lettered, and the healthy Run behind it is claimed by |
//! |     |                                                   |                         | this same call                                            |
//! | U4  | U1/U3 durable record                               | operational feed        | exactly one `LeaseLost` naming the integrity code, then    |
//! |     |                                                   |                         | `DeadLettered`; never claimed or reclaimed again           |
//! | U5  | U1/U3 repeated scans                               | 3 more of each          | no further claim, record or event (attempted once)          |
//! | U6  | U1/U3 observation                                  | captured tracing        | exactly one error event, target                            |
//! |     |                                                   |                         | `awaken::lease_generation_integrity`, the integrity code,  |
//! |     |                                                   |                         | `disposition = dead_lettered`                              |
//! | U7  | pending at an exhausted epoch (no expired lease)   | `claim` and the depth   | never claimed and not dead-lettered (no claim-free         |
//! |     |                                                   | read, 3 times each      | terminal transition exists for it); the Run behind it is   |
//! |     |                                                   |                         | claimed; exactly one error event, `disposition = skipped`, |
//! |     |                                                   |                         | the integrity code, however many scans reach it            |
//! | U8  | awaiting with due input at an exhausted epoch      | as U7                   | as U7                                                      |
//! | U9  | expired reservation at an exhausted epoch          | as U7                   | as U7                                                      |
//! | U10 | U7-U9's healthy Run                                | captured tracing        | no integrity event (control)                               |
//! | U11 | U7's row, by its exact id                          | `claim_run`             | `Err(Rejected)` carrying the integrity code; never claimed |
//! |     |                                                   |                         | (an exact claim of a healthy Run succeeds, the control)    |
//!
//! SQL adapters also run U12 ([`assert_negative_generation_rows`]): a signed
//! column below zero is the same untrustworthy generation.
//!
//! | Row | head of the queue (oldest)                        | scan                    | effect                                                    |
//! |-----|---------------------------------------------------|-------------------------|-----------------------------------------------------------|
//! | U12 | pending at a negative epoch                        | as U7, then `claim_run` | as U7 and U11, the event naming the negative cause; the    |
//! |     |                                                   |                         | scan never aborts on it                                    |
use std::collections::BTreeMap;
use std::future::Future;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_dispatch_contract::{
    DispatchClaims, DispatchControlReads, DispatchCursor, DispatchEnqueue, DispatchError,
    DispatchOperation, DispatchOperationalFeed, DispatchOutcome, DispatchReads, DispatchSettlement,
    DispatchState, Inbox, LeaseLossReason, PendingInput, RunDispatch,
};
use awaken_lease_contract::fence::EpochIntegrityViolation;
use awaken_reliability_testkit::events::CapturedEvents;
use tracing::instrument::WithSubscriber as _;

use super::activation_on;

const MAX_ATTEMPTS: u64 = 3;
const LEASE_MS: u64 = 60_000;

/// Write the untrustworthy generation onto one run: `(run_id, attempt_count)`
/// for a claimed run whose lease must also read as expired, or
/// `(run_id, None)` for a pending run whose generation alone is poisoned.
pub trait Poison {
    fn poison(&self, run_id: &str, expired_attempts: Option<u64>) -> impl Future<Output = ()>;

    /// Write `epoch` as the persisted generation of `run_id` in its current
    /// state, or, when `reserved`, turn its pending row into an expired
    /// reservation at that generation.
    fn poison_generation(
        &self,
        run_id: &str,
        epoch: i64,
        reserved: bool,
    ) -> impl Future<Output = ()>;
}

fn integrity_events(captured: &CapturedEvents, run_id: &str) -> Vec<BTreeMap<String, String>> {
    captured
        .snapshot()
        .into_iter()
        .filter(|event| {
            event.get("target").map(String::as_str) == Some("awaken::lease_generation_integrity")
                && event.get("run_id").map(String::as_str) == Some(run_id)
        })
        .collect()
}

async fn operations<S: DispatchOperationalFeed>(store: &S, run_id: &str) -> Vec<DispatchOperation> {
    store
        .events_after(
            DispatchCursor::default(),
            usize::MAX,
            Some(&RunId(run_id.into())),
        )
        .await
        .unwrap()
        .events
        .into_iter()
        .map(|event| event.operation)
        .collect()
}

async fn state<S: DispatchReads>(store: &S, run_id: &str) -> DispatchState {
    store
        .list_dispatches()
        .await
        .unwrap()
        .into_iter()
        .find(|summary| summary.run_id.0 == run_id)
        .expect("the dispatch row is retained")
        .state
}

/// U4/U5: the durable record is exactly one integrity `LeaseLost` then the
/// `DeadLettered`, after the one claim that wrote the original lease.
async fn assert_one_integrity_dead_letter<S>(store: &S, run_id: &str, exhausted: u64, rule: &str)
where
    S: DispatchOperationalFeed + DispatchReads,
{
    assert_eq!(
        state(store, run_id).await,
        DispatchState::DeadLetter,
        "{rule}"
    );
    assert!(
        store
            .dead_letters()
            .await
            .unwrap()
            .contains(&RunId(run_id.into())),
        "{rule} listed as a dead letter"
    );
    let operations = operations(store, run_id).await;
    let [
        DispatchOperation::Claimed { claim: first },
        DispatchOperation::LeaseLost { claim, reason },
        DispatchOperation::DeadLettered { claim: dead, .. },
    ] = operations.as_slice()
    else {
        panic!("{rule} one claim, then one integrity dead letter; got {operations:?}");
    };
    assert_eq!(first.epoch, 1, "{rule}");
    assert_eq!(
        *reason,
        LeaseLossReason::LeaseGenerationIntegrityViolated,
        "{rule}"
    );
    assert_eq!(
        serde_json::to_value(reason).unwrap(),
        serde_json::Value::from(EpochIntegrityViolation::CODE),
        "{rule} the durable record carries the integrity code"
    );
    assert_eq!(claim.epoch, exhausted, "{rule} the untrustworthy lease");
    assert_eq!(dead, claim, "{rule}");
}

fn assert_one_dead_letter_event(captured: &CapturedEvents, run_id: &str, rule: &str) {
    let events = integrity_events(captured, run_id);
    assert_eq!(events.len(), 1, "{rule} observed exactly once: {events:?}");
    let event = &events[0];
    assert_eq!(event["code"], EpochIntegrityViolation::CODE, "{rule}");
    assert_eq!(event["disposition"], "dead_lettered", "{rule}");
    assert_eq!(event["lease"], "dispatch_claim", "{rule}");
}

/// U7-U12: each skipped row is reported once, by a claim scan or (PostgreSQL,
/// whose discovery SQL prunes pending and awaiting rows at the exhausted
/// generation before decoding them) the depth read; never once per scan.
fn assert_one_skip_event(captured: &CapturedEvents, run_id: &str, cause: &str, rule: &str) {
    let events = integrity_events(captured, run_id);
    assert_eq!(events.len(), 1, "{rule} reported exactly once: {events:?}");
    let event = &events[0];
    assert_eq!(event["code"], EpochIntegrityViolation::CODE, "{rule}");
    assert_eq!(event["disposition"], "skipped", "{rule}");
    assert_eq!(event["lease"], "dispatch_claim", "{rule}");
    assert!(event["violation"].contains(cause), "{rule}: {event:?}");
}

/// Three claim scans and three depth reads: the periodic paths that reach a
/// skipped row. The first scan claims `healthy`.
async fn scan_three_times<S>(store: &S, captured: &CapturedEvents, healthy: &str, rule: &str)
where
    S: DispatchClaims + DispatchControlReads,
{
    let caps = Default::default();
    for scan in 0..3 {
        let claimed = store
            .claim("pool", LEASE_MS, &caps)
            .with_subscriber(captured.clone())
            .await
            .unwrap();
        match scan {
            0 => assert_eq!(
                claimed.map(|claimed| claimed.lease.run_id().0.clone()),
                Some(healthy.to_owned()),
                "{rule} the Run behind the skipped rows is claimed"
            ),
            _ => assert!(claimed.is_none(), "{rule} nothing else is claimable"),
        }
        store
            .dispatch_depths()
            .with_subscriber(captured.clone())
            .await
            .unwrap();
    }
}

/// U11/U12: an exact claim of a skipped row refuses it permanently with the
/// integrity code, and claims nothing.
async fn assert_exact_claim_refused<S>(store: &S, run_id: &str, rule: &str)
where
    S: DispatchClaims + DispatchOperationalFeed,
{
    let refused = store
        .claim_run(&RunId(run_id.into()), "pool", LEASE_MS, &Default::default())
        .await;
    assert!(
        matches!(&refused, Err(DispatchError::Rejected(message))
            if message.contains(EpochIntegrityViolation::CODE)),
        "{rule} {refused:?}"
    );
    assert!(
        operations(store, run_id).await.is_empty(),
        "{rule} never claimed"
    );
}

/// Run U1-U11 against one empty store. `exhausted` is the adapter's exhausted
/// generation as a claim reports it; `poison` writes it as `stored`.
pub async fn assert_untrustworthy_generation_rows<S, P>(
    store: &S,
    poison: &P,
    exhausted: u64,
    stored: i64,
) where
    S: DispatchEnqueue
        + DispatchClaims
        + DispatchReads
        + DispatchOperationalFeed
        + DispatchSettlement
        + DispatchControlReads
        + Inbox,
    P: Poison,
{
    let captured = CapturedEvents::default();
    let caps = Default::default();

    // U1/U2: a retry-exhausted head ahead of a healthy Run.
    for (run, thread) in [
        ("u1-poisoned", "u1-thread"),
        ("u1-healthy", "u1-healthy-thread"),
    ] {
        store
            .enqueue(RunDispatch::new(activation_on(run, thread)))
            .await
            .unwrap();
        if run == "u1-poisoned" {
            store
                .claim("crashed-owner", LEASE_MS, &caps)
                .await
                .unwrap()
                .expect("U1 the head is claimed once, honestly");
            poison.poison(run, Some(MAX_ATTEMPTS)).await;
        }
    }
    let exhausted_claim = store
        .claim_retry_exhausted("pool", LEASE_MS, MAX_ATTEMPTS)
        .with_subscriber(captured.clone())
        .await;
    assert!(
        matches!(exhausted_claim, Ok(None)),
        "U1 the head never aborts the retry-exhaustion selection: {exhausted_claim:?}"
    );
    let healthy = store
        .claim("pool", LEASE_MS, &caps)
        .with_subscriber(captured.clone())
        .await
        .unwrap()
        .expect("U2 the healthy Run behind the head is claimed");
    assert_eq!(healthy.lease.run_id().0, "u1-healthy", "U2");

    // U3: an ordinary recovery head; the healthy Run is claimed by the same call.
    for (run, thread) in [
        ("u3-poisoned", "u3-thread"),
        ("u3-healthy", "u3-healthy-thread"),
    ] {
        store
            .enqueue(RunDispatch::new(activation_on(run, thread)))
            .await
            .unwrap();
        if run == "u3-poisoned" {
            store
                .claim("crashed-owner", LEASE_MS, &caps)
                .await
                .unwrap()
                .expect("U3 the head is claimed once, honestly");
            poison.poison(run, Some(1)).await;
        }
    }
    let healthy = store
        .claim("pool", LEASE_MS, &caps)
        .with_subscriber(captured.clone())
        .await
        .unwrap()
        .expect("U3 the healthy Run behind the head is claimed by this same scan");
    assert_eq!(healthy.lease.run_id().0, "u3-healthy", "U3");

    // U5: later scans neither claim nor record the refused rows again.
    for _ in 0..3 {
        assert!(
            store
                .claim_retry_exhausted("pool", LEASE_MS, MAX_ATTEMPTS)
                .with_subscriber(captured.clone())
                .await
                .unwrap()
                .is_none(),
            "U5"
        );
        assert!(
            store
                .claim("pool", LEASE_MS, &caps)
                .with_subscriber(captured.clone())
                .await
                .unwrap()
                .is_none(),
            "U5"
        );
    }
    for (run, rule) in [
        ("u1-poisoned", "U1/U4/U5/U6"),
        ("u3-poisoned", "U3/U4/U5/U6"),
    ] {
        assert_one_integrity_dead_letter(store, run, exhausted, rule).await;
        assert_one_dead_letter_event(&captured, run, rule);
    }

    // U8 first: an Awaiting row needs its own claim and settlement, made
    // while it is the only runnable row.
    store
        .enqueue(RunDispatch::new(activation_on("u8-poisoned", "u8-thread")))
        .await
        .unwrap();
    let awaiting = store
        .claim("crashed-owner", LEASE_MS, &caps)
        .await
        .unwrap()
        .expect("U8 claimed once, honestly");
    assert_eq!(awaiting.lease.run_id().0, "u8-poisoned", "U8");
    store
        .settle(
            awaiting.lease.run_id(),
            awaiting.lease.epoch(),
            DispatchOutcome::Awaiting,
            &[],
        )
        .await
        .unwrap();
    store
        .append(PendingInput {
            message_id: "u8-input".to_string(),
            run_id: RunId("u8-poisoned".into()),
            thread_id: awaken_agent_contract::agent::thread::Id("u8-thread".into()),
            correlation_id: "u8-ticket".to_string(),
            available_at_ms: None,
            result: awaken_runtime_contract::resume::ResumeResult::allow(),
            context_messages: Vec::new(),
        })
        .await
        .unwrap();
    poison.poison_generation("u8-poisoned", stored, false).await;
    // U7: a pending generation with no successor and no expired lease; U9: an
    // expired reservation at it; the healthy Run behind them all.
    for (run, thread) in [
        ("u7-poisoned", "u7-thread"),
        ("u9-poisoned", "u9-thread"),
        ("u7-healthy", "u7-healthy-thread"),
    ] {
        store
            .enqueue(RunDispatch::new(activation_on(run, thread)))
            .await
            .unwrap();
    }
    poison.poison("u7-poisoned", None).await;
    poison.poison_generation("u9-poisoned", stored, true).await;
    scan_three_times(store, &captured, "u7-healthy", "U7-U9").await;
    for (run, expected, rule) in [
        ("u7-poisoned", DispatchState::Pending, "U7"),
        ("u8-poisoned", DispatchState::Awaiting, "U8"),
        ("u9-poisoned", DispatchState::Reserved, "U9"),
    ] {
        assert_eq!(state(store, run).await, expected, "{rule} never settled");
        assert_one_skip_event(&captured, run, "no successor", rule);
    }
    assert!(
        operations(store, "u7-poisoned").await.is_empty(),
        "U7 never claimed"
    );
    assert_eq!(
        operations(store, "u8-poisoned")
            .await
            .iter()
            .filter(|operation| matches!(operation, DispatchOperation::Claimed { .. }))
            .count(),
        1,
        "U8 only its one honest claim"
    );
    assert!(
        integrity_events(&captured, "u7-healthy").is_empty(),
        "U10 a healthy Run is never reported"
    );

    // U11: the exact claim refuses the exhausted row with the integrity code.
    assert_exact_claim_refused(store, "u7-poisoned", "U11").await;
    store
        .enqueue(RunDispatch::new(activation_on("u11-healthy", "u11-thread")))
        .await
        .unwrap();
    assert!(
        store
            .claim_run(&RunId("u11-healthy".into()), "pool", LEASE_MS, &caps)
            .await
            .unwrap()
            .is_some(),
        "U11 control: an exact claim of a healthy Run succeeds"
    );
}

/// U12 against a SQL store: a negative persisted generation is skipped and
/// reported once by the scans, never aborting them, and refused by an exact
/// claim with the integrity code.
pub async fn assert_negative_generation_rows<S, P>(store: &S, poison: &P)
where
    S: DispatchEnqueue
        + DispatchClaims
        + DispatchReads
        + DispatchOperationalFeed
        + DispatchControlReads,
    P: Poison,
{
    let captured = CapturedEvents::default();
    for (run, thread) in [
        ("u12-negative", "u12-thread"),
        ("u12-healthy", "u12-healthy-thread"),
    ] {
        store
            .enqueue(RunDispatch::new(activation_on(run, thread)))
            .await
            .unwrap();
    }
    poison.poison_generation("u12-negative", -1, false).await;
    scan_three_times(store, &captured, "u12-healthy", "U12").await;
    assert_eq!(
        state(store, "u12-negative").await,
        DispatchState::Pending,
        "U12 never settled"
    );
    assert_one_skip_event(&captured, "u12-negative", "negative", "U12");
    assert!(
        integrity_events(&captured, "u12-healthy").is_empty(),
        "U12 control"
    );
    assert_exact_claim_refused(store, "u12-negative", "U12").await;
}
