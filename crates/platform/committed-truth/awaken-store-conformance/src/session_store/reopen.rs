// @formal surface exclude=test_support_crate
// @formal owner crates/platform/session/awaken-session-store/tests/session_store_parity.rs
// @formal reason: T34-C01 SessionStore reopen row shared by file SQLite and PostgreSQL. Tooling-only;
// @formal reason: it admits nothing at runtime.
//! T34-C01 reopen row for the `SessionStore` port.
//!
//! | Step | Through | Effect re-asserted after drop + reopen |
//! |---|---|---|
//! | S1 | `first` | create with a lifecycle fact and explicit creation clock |
//! | S2 | `first` | replace (title) through the aggregate update law: history entry, update clock, fact |
//! | S3 | `first` | retire the create fact from the outbox |
//! | R1 | `reopen()` | root (incl. history and clock), owner, both receipts and pending outbox identical |
//! | R2 | `reopen()` | exact replace replay -> `Replayed { 2 }`, nothing new |
//! | R3 | `reopen()` | stale-revision replace with a new key -> `Conflict { 2 }`, nothing new |
//!
//! A factory that does not reach the same durable authority fails R1.
use super::{create_session, fact, record, record_update, session};
use awaken_session_contract::{
    ManagedLifecycleFact, PersistedSession, SessionMutation, SessionMutationPayload,
    SessionMutationResult,
};
use awaken_session_types::{SessionId, SessionRevision};

const T_CREATED: u64 = 1_786_000_000_000;
const T_UPDATED: u64 = 1_786_000_005_000;

/// Every authority fact one handle can observe for the reopen fixture.
#[derive(Debug, PartialEq)]
pub struct DurableSessionImage {
    pub root: PersistedSession,
    pub owner: String,
    pub create_receipt: Option<awaken_session_contract::SessionIdempotencyReceipt>,
    pub replace_receipt: Option<awaken_session_contract::SessionIdempotencyReceipt>,
    pub pending_facts: Vec<ManagedLifecycleFact>,
}

pub async fn durable_session_image<
    R: awaken_session_contract::SessionRepository + awaken_session_contract::LifecycleOutbox + ?Sized,
>(
    r: &R,
    id: &SessionId,
    create_key: &str,
    replace_key: &str,
) -> DurableSessionImage {
    let mut pending_facts = r.pending_lifecycle().await.expect("read pending outbox");
    pending_facts.sort_by(|a, b| a.id.cmp(&b.id));
    DurableSessionImage {
        root: r.get(id).await.expect("reopen fixture root is readable"),
        owner: r.owner(id).await.expect("reopen fixture owner"),
        create_receipt: r
            .idempotency_receipt(id, create_key)
            .await
            .expect("create receipt read"),
        replace_receipt: r
            .idempotency_receipt(id, replace_key)
            .await
            .expect("replace receipt read"),
        pending_facts,
    }
}

pub async fn reopen_preserves_authority<R, F>(first: R, reopen: impl Fn() -> F)
where
    R: awaken_session_contract::SessionRepository + awaken_session_contract::LifecycleOutbox,
    F: std::future::Future<Output = R>,
{
    let id = SessionId::from("sesn_t34_reopen");
    let create_key = format!("test:create:{id}");
    let replace_key = "t34:reopen:replace";

    // S1
    let mut initial = session(id.as_str(), "before-reopen");
    initial.created_at_unix_ms = Some(T_CREATED);
    initial.updated_at_unix_ms = Some(T_CREATED);
    let created = create_session(
        &first,
        "ws_a",
        initial,
        vec![fact("t34-reopen-created", id.as_str(), "session.created")],
    )
    .await;
    assert_eq!(created.revision, SessionRevision(1), "S1 created");

    // S2: the same aggregate law `replace_session` uses, with the mutation kept
    // so the exact replay can be repeated through the reopened handle.
    let mut candidate = created.clone();
    candidate.title = Some("after-update".into());
    candidate.updated_at_unix_ms = Some(T_UPDATED);
    let candidate = record_update(&created, candidate, 0).into_snapshot();
    let payload = SessionMutationPayload::Replace(candidate.clone());
    let replace = SessionMutation {
        expected_revision: created.revision,
        idempotency: record(replace_key, &payload),
        payload,
        lifecycle_facts: vec![fact("t34-reopen-updated", id.as_str(), "session.updated")],
    };
    assert_eq!(
        first.commit("ws_a", replace.clone()).await,
        Ok(SessionMutationResult::Applied {
            new_revision: SessionRevision(2)
        }),
        "S2 replace applied"
    );

    // S3
    first
        .complete_lifecycle("t34-reopen-created")
        .await
        .expect("S3 retire the create fact");

    let before = durable_session_image(&first, &id, &create_key, replace_key).await;
    assert_eq!(before.root.revision, SessionRevision(2), "S2 head");
    assert_eq!(
        before.root.title.as_deref(),
        Some("after-update"),
        "S2 head"
    );
    assert_eq!(before.root.created_at_unix_ms, Some(T_CREATED), "S1 clock");
    assert!(
        before.root.updated_at_unix_ms.is_some(),
        "S2 update clock is recorded"
    );
    let history =
        serde_json::to_value(&before.root).expect("encode root")["update_history"].clone();
    assert!(!history.is_null(), "S2 update history is recorded");
    assert!(before.create_receipt.is_some(), "S1 receipt");
    assert_eq!(
        before
            .replace_receipt
            .as_ref()
            .map(|r| r.committed_revision),
        Some(SessionRevision(2)),
        "S2 receipt"
    );
    assert_eq!(
        before
            .pending_facts
            .iter()
            .map(|f| f.id.as_str())
            .collect::<Vec<_>>(),
        ["t34-reopen-updated"],
        "S3 only the update fact is pending"
    );

    drop(first);
    let reopened = reopen().await;

    // R1
    let after = durable_session_image(&reopened, &id, &create_key, replace_key).await;
    assert_eq!(
        after, before,
        "R1 reopen image (root, history, clock, receipts, outbox)"
    );
    assert_eq!(
        serde_json::to_value(&after.root).expect("encode root")["update_history"],
        history,
        "R1 history"
    );

    // R2
    assert_eq!(
        reopened.commit("ws_a", replace).await,
        Ok(SessionMutationResult::Replayed {
            new_revision: SessionRevision(2)
        }),
        "R2 exact replay after reopen returns the original receipt"
    );

    // R3
    let mut stale = after.root.clone();
    stale.revision = SessionRevision(1);
    stale.title = Some("stale-writer".into());
    let payload = SessionMutationPayload::Replace(stale);
    assert_eq!(
        reopened
            .commit(
                "ws_a",
                SessionMutation {
                    expected_revision: SessionRevision(1),
                    idempotency: record("t34:reopen:stale", &payload),
                    payload,
                    lifecycle_facts: Vec::new(),
                },
            )
            .await,
        Ok(SessionMutationResult::Conflict {
            current_revision: SessionRevision(2)
        }),
        "R3 stale CAS still conflicts after reopen"
    );
    assert_eq!(
        durable_session_image(&reopened, &id, &create_key, replace_key).await,
        before,
        "R2/R3 changed nothing"
    );
    assert_eq!(
        reopened
            .idempotency_receipt(&id, "t34:reopen:stale")
            .await
            .expect("stale receipt read"),
        None,
        "R3 the loser has no receipt"
    );
}
