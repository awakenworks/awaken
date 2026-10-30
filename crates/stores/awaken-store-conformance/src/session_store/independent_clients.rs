// @formal surface exclude=test_support_crate
// @formal owner crates/stores/awaken-store-conformance/src/session_store.rs
// @formal owner crates/stores/awaken-session-store/tests/independent_clients.rs
// @formal reason: T05-C01 oracle rows for two independently opened Session store clients. It is
// @formal reason: tooling-only; the backend test target supplies the real independent handles and
// @formal reason: the database-level contention witness.
//! T05-C01 (并发赢家): two independent clients submit different commands with
//! the same expected revision.
//!
//! Oracles (groups-1.json order):
//! - O0 exactly one legal winner;
//! - O1 root, lifecycle facts (outbox) and receipt are consistent;
//! - O2 the loser produces no fact, outbox row or receipt.
//!
//! | Row | Client | Command | Expected |
//! |---|---|---|---|
//! | W1 | left / right | different titles, same revision, distinct fact + key | one `Applied`, one `Conflict { current = N+1 }` |
//! | W2 | either reader | read root | complete winner candidate at N+1, identical through both clients |
//! | W3 | either reader | outbox | exactly the winner's fact |
//! | W4 | either reader | receipts | winner key -> (winner hash, N+1); loser key -> none |
//! | W5 | winner | exact replay | `Replayed { N+1 }`, no new fact |
//! | W6 | loser | retry same stale command | `Conflict`, still no fact or receipt |
use super::{fact, record, record_update, session};
use awaken_session_contract::{
    ManagedLifecycleFact, SessionCreateResult, SessionMutation, SessionMutationPayload,
    SessionMutationResult, SessionRepositoryError,
};
use awaken_session_types::{SessionId, SessionRevision};

/// The two prepared commands of one independent-client race.
pub struct IndependentRace {
    pub session_id: SessionId,
    pub base_revision: SessionRevision,
    pub commands: [IndependentCommand; 2],
}

pub struct IndependentCommand {
    pub title: String,
    pub key: String,
    pub fact: ManagedLifecycleFact,
    pub mutation: SessionMutation,
}

/// Create the Session through `creator` and prepare two legal, different
/// replacements against the same observed revision.
pub async fn prepare_independent_race<R>(creator: &R, id: &str) -> IndependentRace
where
    R: awaken_session_contract::SessionRepository
        + awaken_session_contract::LifecycleOutbox
        + ?Sized,
{
    let mut value = session(id, "before");
    value.revision = SessionRevision(0);
    let payload = SessionMutationPayload::Replace(value.clone());
    let current = match creator
        .create(
            "ws_a",
            value.clone(),
            record(&format!("test:create:{id}"), &payload),
            Vec::new(),
        )
        .await
        .expect("T05 create Session fixture")
    {
        SessionCreateResult::Applied(value) | SessionCreateResult::Replayed(value) => value,
    };
    let command = |side: &str| {
        let mut replacement = current.clone();
        let title = format!("{side}-wins");
        replacement.title = Some(title.clone());
        let replacement = record_update(&current, replacement, 0).into_snapshot();
        let payload = SessionMutationPayload::Replace(replacement);
        let key = format!("test:t05:{id}:{side}");
        let fact = fact(
            &format!("session:{id}:t05:{side}"),
            id,
            "session.status_idled",
        );
        IndependentCommand {
            title,
            key: key.clone(),
            fact: fact.clone(),
            mutation: SessionMutation {
                expected_revision: current.revision,
                idempotency: record(&key, &payload),
                payload,
                lifecycle_facts: vec![fact],
            },
        }
    };
    IndependentRace {
        session_id: SessionId::from(id),
        base_revision: current.revision,
        commands: [command("left"), command("right")],
    }
}

/// Assert W1-W6 after both clients returned. `left` submitted
/// `race.commands[0]`, `right` submitted `race.commands[1]`. Returns the index
/// of the winning command.
// CM2: both original client references had the same erased store type. Generic
// R retains that unsized case with the exact Core write/outbox capabilities;
// W1-W6 below retain the original independent-client and no-loser-effect oracle.
pub async fn assert_independent_race<R>(
    left: &R,
    right: &R,
    race: &IndependentRace,
    outcomes: [Result<SessionMutationResult, SessionRepositoryError>; 2],
) -> usize
where
    R: awaken_session_contract::SessionRepository
        + awaken_session_contract::LifecycleOutbox
        + ?Sized,
{
    let next = SessionRevision(race.base_revision.0 + 1);
    let applied = outcomes
        .iter()
        .enumerate()
        .filter(|(_, outcome)| matches!(outcome, Ok(SessionMutationResult::Applied { .. })))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert_eq!(applied.len(), 1, "W1/O0 exactly one winner: {outcomes:?}");
    let winner = applied[0];
    let loser = 1 - winner;
    assert_eq!(
        outcomes[winner],
        Ok(SessionMutationResult::Applied { new_revision: next }),
        "W1/O0 winner commits N+1"
    );
    assert_eq!(
        outcomes[loser],
        Ok(SessionMutationResult::Conflict {
            current_revision: next
        }),
        "W1/O0 loser observes the winner's revision"
    );

    let won = &race.commands[winner];
    let lost = &race.commands[loser];
    let from_left = left.get(&race.session_id).await.expect("W2 left read");
    let from_right = right.get(&race.session_id).await.expect("W2 right read");
    assert_eq!(from_left, from_right, "W2/O1 both clients read one root");
    assert_eq!(from_left.revision, next, "W2/O1 root revision");
    let SessionMutationPayload::Replace(expected) = &won.mutation.payload else {
        unreachable!("T05 commands are replacements")
    };
    let mut expected = expected.clone();
    expected.revision = next;
    assert_eq!(from_left, expected, "W2/O1 root is the complete winner");
    assert_eq!(
        from_left.title.as_deref(),
        Some(won.title.as_str()),
        "W2/O2 no loser field merged"
    );

    let outbox = |facts: Vec<ManagedLifecycleFact>| {
        facts
            .into_iter()
            .filter(|fact| fact.object_id == race.session_id.as_str())
            .collect::<Vec<_>>()
    };
    let left_outbox = outbox(left.pending_lifecycle().await.expect("W3 left outbox"));
    let right_outbox = outbox(right.pending_lifecycle().await.expect("W3 right outbox"));
    assert_eq!(left_outbox, right_outbox, "W3/O1 one durable outbox");
    assert_eq!(
        left_outbox,
        vec![won.fact.clone()],
        "W3/O1+O2 exactly the winner's fact"
    );

    for reader in [left, right] {
        assert_eq!(
            reader
                .idempotency_receipt(&race.session_id, &won.key)
                .await
                .expect("W4 winner receipt read"),
            Some(awaken_session_contract::SessionIdempotencyReceipt {
                payload_hash: won.mutation.idempotency.payload_hash.clone(),
                committed_revision: next,
            }),
            "W4/O1 winner receipt names the committed revision"
        );
        assert_eq!(
            reader
                .idempotency_receipt(&race.session_id, &lost.key)
                .await
                .expect("W4 loser receipt read"),
            None,
            "W4/O2 loser has no receipt"
        );
    }

    let (winner_client, loser_client) = if winner == 0 {
        (left, right)
    } else {
        (right, left)
    };
    assert_eq!(
        winner_client.commit("ws_a", won.mutation.clone()).await,
        Ok(SessionMutationResult::Replayed { new_revision: next }),
        "W5 exact replay returns the original receipt"
    );
    assert_eq!(
        loser_client.commit("ws_a", lost.mutation.clone()).await,
        Ok(SessionMutationResult::Conflict {
            current_revision: next
        }),
        "W6/O2 the stale loser retry still loses"
    );
    assert_eq!(
        outbox(left.pending_lifecycle().await.expect("W5/W6 outbox")),
        vec![won.fact.clone()],
        "W5/W6 no fact from replay or loser retry"
    );
    assert_eq!(
        left.idempotency_receipt(&race.session_id, &lost.key)
            .await
            .expect("W6 loser receipt"),
        None,
        "W6/O2 the loser retry created no receipt"
    );
    assert_eq!(
        left.get(&race.session_id).await.expect("W5/W6 root"),
        from_left,
        "W5/W6 root unchanged"
    );
    winner
}
