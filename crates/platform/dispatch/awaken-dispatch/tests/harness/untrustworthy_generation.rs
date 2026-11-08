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
//! | U7  | pending at an exhausted epoch (no expired lease)   | `claim`                 | never claimed and not dead-lettered (no claim-free         |
//! |     |                                                   |                         | terminal transition exists for it); the Run behind it is   |
//! |     |                                                   |                         | claimed                                                    |
use std::collections::BTreeMap;
use std::future::Future;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_dispatch_contract::{
    DispatchClaims, DispatchCursor, DispatchEnqueue, DispatchOperation, DispatchOperationalFeed,
    DispatchReads, DispatchState, LeaseLossReason, RunDispatch,
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

/// Run U1-U7 against one empty store. `exhausted` is the adapter's exhausted
/// generation as a claim reports it.
///
/// `observes_pending_skip` is false only for PostgreSQL, whose discovery SQL
/// prunes a non-reservation row at the exhausted generation before decoding it
/// (a pre-existing discovery-cost bound), so U7 is not observed there.
pub async fn assert_untrustworthy_generation_rows<S, P>(
    store: &S,
    poison: &P,
    exhausted: u64,
    observes_pending_skip: bool,
) where
    S: DispatchEnqueue + DispatchClaims + DispatchReads + DispatchOperationalFeed,
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

    // U7: a pending generation with no successor and no expired lease.
    for (run, thread) in [
        ("u7-poisoned", "u7-thread"),
        ("u7-healthy", "u7-healthy-thread"),
    ] {
        store
            .enqueue(RunDispatch::new(activation_on(run, thread)))
            .await
            .unwrap();
        if run == "u7-poisoned" {
            poison.poison(run, None).await;
        }
    }
    let healthy = store
        .claim("pool", LEASE_MS, &caps)
        .with_subscriber(captured.clone())
        .await
        .unwrap()
        .expect("U7 the Run behind a pending exhausted generation is claimed");
    assert_eq!(healthy.lease.run_id().0, "u7-healthy", "U7");
    assert_eq!(
        state(store, "u7-poisoned").await,
        DispatchState::Pending,
        "U7"
    );
    assert!(
        operations(store, "u7-poisoned").await.is_empty(),
        "U7 never claimed"
    );
    let skipped = integrity_events(&captured, "u7-poisoned");
    assert!(
        skipped
            .iter()
            .all(|event| event["disposition"] == "skipped")
            && skipped.is_empty() != observes_pending_skip,
        "U7 observed as skipped: {skipped:?}"
    );
}
