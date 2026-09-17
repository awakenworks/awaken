//! Direct execution of the existing asynchronous replacement boundary.
use crate::mutation::commit_observed_snapshot;
use awaken_session_contract::SessionId;
use awaken_session_contract::{
    IdempotencyRecord, ManagedLifecycleFact, ManagedSessionRepository, PersistedSession,
    SessionCreateResult, SessionIdempotencyReceipt, SessionMutation, SessionMutationPayload,
    SessionMutationResult, SessionRepositoryConflict, SessionRepositoryError, SessionRevision,
};
use std::sync::atomic::{AtomicUsize, Ordering};

// A port observation, not a repository/state-machine implementation. All
// unrequested methods fail; the production future must make exactly one CAS.
struct ObservedCommitPort {
    expected: SessionMutation,
    pending_once: bool,
    calls: AtomicUsize,
    result: Result<SessionMutationResult, SessionRepositoryError>,
}

#[async_trait::async_trait]
impl ManagedSessionRepository for ObservedCommitPort {
    async fn commit_mutation(
        &self,
        owner_scope: &str,
        mutation: SessionMutation,
    ) -> Result<SessionMutationResult, SessionRepositoryError> {
        assert_eq!(owner_scope, "workspace");
        assert_eq!(self.calls.fetch_add(1, Ordering::SeqCst), 0);
        assert_eq!(mutation, self.expected);
        let mut pending = self.pending_once;
        std::future::poll_fn(|_cx| {
            if std::mem::take(&mut pending) {
                std::task::Poll::Pending
            } else {
                std::task::Poll::Ready(self.result.clone())
            }
        })
        .await
    }
    async fn create(
        &self,
        _owner_scope: &str,
        _session: PersistedSession,
        _idempotency: IdempotencyRecord,
        _lifecycle_facts: Vec<ManagedLifecycleFact>,
    ) -> Result<SessionCreateResult, SessionRepositoryError> {
        panic!("unexpected repository operation")
    }
    async fn replay_create(
        &self,
        _owner_scope: &str,
        _session_id: &SessionId,
        _idempotency: &IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionRepositoryError> {
        panic!("unexpected repository operation")
    }
    async fn append_lifecycle(
        &self,
        _fact: ManagedLifecycleFact,
    ) -> Result<(), SessionRepositoryError> {
        panic!("unexpected repository operation")
    }
    async fn pending_lifecycle(&self) -> Result<Vec<ManagedLifecycleFact>, SessionRepositoryError> {
        panic!("unexpected repository operation")
    }
    async fn complete_lifecycle(&self, _fact_id: &str) -> Result<(), SessionRepositoryError> {
        panic!("unexpected repository operation")
    }
    async fn get(
        &self,
        _session_id: &SessionId,
    ) -> Result<PersistedSession, SessionRepositoryError> {
        panic!("unexpected repository operation")
    }
    async fn sessions_referencing_credential_source(
        &self,
        _workspace_id: &str,
        _source_id: &awaken_credential_contract::CredentialSourceId,
    ) -> Result<Vec<PersistedSession>, SessionRepositoryError> {
        panic!("unexpected repository operation")
    }
    async fn idempotency_receipt(
        &self,
        _session_id: &SessionId,
        _key: &str,
    ) -> Result<Option<SessionIdempotencyReceipt>, SessionRepositoryError> {
        panic!("unexpected repository operation")
    }
    async fn owner(&self, _session_id: &SessionId) -> Result<String, SessionRepositoryError> {
        panic!("unexpected repository operation")
    }
}

#[test]
fn observed_commit_future_cause_effect_decision_table() {
    // Causes: C1 old observation absent/zero/nonzero/MAX; C2 current clock
    // lower/equal/higher; C3 zero/one lifecycle fact; C4 immediate/delayed port;
    // C5 each of four receipts or seven typed repository errors. Effects:
    // R1 C1+C2 => candidate time is exact max, every other field is unchanged.
    // R2 all C3-C5 => exactly one commit receives the full replacement, original
    // revision/owner/key/hash/facts; Pending cannot resubmit or lose arguments.
    // R3 all C5 => return the exact port result, without repository reads.
    // Failure changes the LOCAL candidate's observation too; only the durable
    // repository owns rejection/stutter. This test does not implement that CAS.
    // The product below crosses all causes; whole-value equality covers fields
    // outside the scalar time kernels and prevents an empty-facts-only oracle.
    for old in [None, Some(0), Some(1), Some(u64::MAX)] {
        for now in [0, 1, u64::MAX] {
            for has_fact in [false, true] {
                for pending_once in [false, true] {
                    for kind in 0..11 {
                        check_observed_commit(old, now, has_fact, pending_once, kind);
                    }
                }
            }
        }
    }
}

fn check_observed_commit(old: Option<u64>, now: u64, has_fact: bool, pending_once: bool, kind: u8) {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};
    let mut session = super::persisted("observed", false, "idle");
    session.created_at_unix_ms = if has_fact { Some(1) } else { None };
    session.updated_at_unix_ms = old;
    session.revision = SessionRevision(if has_fact { u64::MAX } else { 0 });
    session.title = Some("retained".into());
    session
        .metadata
        .insert("metadata".into(), "retained".into());
    let mut expected = session.clone();
    expected.updated_at_unix_ms = Some(old.map_or(now, |previous| previous.max(now)));
    let idempotency = IdempotencyRecord {
        key: "precompiled-key".into(),
        payload_hash: "precompiled-hash".into(),
    };
    let facts = if has_fact {
        vec![ManagedLifecycleFact {
            id: "fact".into(),
            object_id: "observed".into(),
            workspace_id: Some("workspace".into()),
            event_type: "session.updated".into(),
            timestamp: -1,
            runtime_interval: None,
        }]
    } else {
        Vec::new()
    };
    let result = match kind {
        0 => Ok(SessionMutationResult::Applied {
            new_revision: SessionRevision(7),
        }),
        1 => Ok(SessionMutationResult::Replayed {
            new_revision: SessionRevision(8),
        }),
        2 => Ok(SessionMutationResult::Conflict {
            current_revision: SessionRevision(9),
        }),
        3 => Ok(SessionMutationResult::IdempotencyMismatch),
        4 => Err(SessionRepositoryError::NotFound),
        5 => Err(SessionRepositoryError::Unavailable("unavailable".into())),
        6 => Err(SessionRepositoryError::Corrupt("corrupt".into())),
        7 => Err(SessionRepositoryError::InvalidMutation("invalid".into())),
        8 => Err(SessionRepositoryError::Conflict(
            SessionRepositoryConflict::AlreadyExists,
        )),
        9 => Err(SessionRepositoryError::Conflict(
            SessionRepositoryConflict::Tombstoned,
        )),
        10 => Err(SessionRepositoryError::Conflict(
            SessionRepositoryConflict::IdempotencyMismatch,
        )),
        _ => unreachable!(),
    };
    let port = ObservedCommitPort {
        expected: SessionMutation {
            expected_revision: session.revision,
            idempotency: idempotency.clone(),
            payload: SessionMutationPayload::Replace(expected.clone()),
            lifecycle_facts: facts.clone(),
        },
        calls: AtomicUsize::new(0),
        pending_once,
        result,
    };
    {
        let mut future = std::pin::pin!(commit_observed_snapshot(
            &port,
            "workspace",
            &mut session,
            idempotency,
            facts,
            now
        ));
        let mut context = Context::from_waker(Waker::noop());
        if pending_once {
            assert_eq!(
                future.as_mut().poll(&mut context),
                Poll::Pending,
                "R2 delay witness"
            );
            assert_eq!(port.calls.load(Ordering::SeqCst), 1, "R2 before resume");
        }
        assert_eq!(
            future.as_mut().poll(&mut context),
            Poll::Ready(port.result.clone()),
            "R3"
        );
    }
    assert_eq!(port.calls.load(Ordering::SeqCst), 1, "R2 after resume");
    assert_eq!(session, expected, "R1 full local candidate");
}
