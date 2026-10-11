// @formal evidence session_time.root_observations_are_revision_bound_and_replay_stable
//! Direct execution of the existing asynchronous replacement boundary.
use crate::mutation::commit_observed_snapshot;
use awaken_session_repository_contract::SessionRepository;
use awaken_session_state_contract::{
    IdempotencyRecord, ManagedLifecycleFact, PersistedSession, SessionCreateResult,
    SessionIdempotencyReceipt, SessionMutation, SessionMutationPayload, SessionMutationResult,
    SessionRepositoryConflict, SessionRepositoryError,
};
use awaken_session_types::SessionId;
use awaken_session_types::SessionRevision;
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
impl SessionRepository for ObservedCommitPort {
    async fn commit(
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
    async fn get(
        &self,
        _session_id: &SessionId,
    ) -> Result<PersistedSession, SessionRepositoryError> {
        panic!("unexpected repository operation")
    }
    async fn scan(
        &self,
        _query: awaken_session_state_contract::SessionScanQuery<'_>,
    ) -> Result<awaken_session_state_contract::SessionScan, SessionRepositoryError> {
        panic!("unexpected repository operation")
    }
    async fn deleted_event_admissions(
        &self,
        _owner_scope: &str,
        _session_id: &SessionId,
    ) -> Result<
        Option<awaken_session_state_contract::DeletedSessionEventAdmissions>,
        SessionRepositoryError,
    > {
        panic!("unexpected repository operation")
    }
    async fn count_environment_phase(
        &self,
        _phase: awaken_session_state_contract::SessionEnvironmentPhase,
    ) -> Result<u64, SessionRepositoryError> {
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
        5 => Err(SessionRepositoryError::AuthorityUnavailable(
            "unavailable".into(),
        )),
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
    let mut session = awaken_session_state_contract::SessionAggregate::try_from(session)
        .expect("valid observed candidate");
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
    assert_eq!(session.into_snapshot(), expected, "R1 full local candidate");
}

#[tokio::test]
async fn update_replay_keeps_the_original_atomic_receipt_after_a_later_root_write() {
    // Causes: C1 explicit/no HTTP retry key; C2 two update candidates share one
    // real SQLite snapshot; C3 a later independent root commits before replay
    // reload; C4 a different winner makes the delayed If-Match stale; C5 noop.
    // Effects: E1 only the winner reports command_applied; E2 a replay carries
    // its original repository revision even when its current root is newer;
    // E3 C4 returns Conflict without accepting the candidate; E4 C5 does not
    // write another root or produce a change mask/public update event.
    // Rules: U1 C1+C2 => E1; U2 U1+C3 => E1+E2; U3 C1+C4 => E3;
    // U4 after each rule, an exact keyed retry or unkeyed unchanged update =>
    // E4. The existing pause fixture gates the actual pre-CAS request; SQLite
    // computes Applied/Replayed/Conflict, never a fabricated receipt. This
    // title-only command has no Runtime repair; existing mixed-update tests own MCP/tool/
    // budget repair: replay must not disable those durable-truth effects. U4
    // checks the change-mask gate; existing Managed HTTP tests own observable
    // event emission rather than claiming this app-only table is wire history.
    use crate::{SessionFieldUpdate, SessionUpdateCommand, SessionUpdateError};
    use std::sync::Arc;
    for keyed in [false, true] {
        for schedule in ["later_root", "same_snapshot", "conflict"] {
            let id = SessionId::from("update-receipt-race");
            let durable = Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                    .await
                    .unwrap(),
            );
            super::create(
                durable.as_ref(),
                super::persisted(id.as_str(), false, "idle"),
            )
            .await;
            let repo = Arc::new(super::FaultingSessionRepository::new(durable.clone()));
            let environments = Arc::new(super::RecordingEnvironmentSource::default());
            let app = Arc::new(super::application(repo.clone(), environments.clone()));
            let origin = durable.get(&id).await.unwrap();
            let command = |title: &str, key: Option<&str>, if_match| SessionUpdateCommand {
                title: Some(SessionFieldUpdate::Replace(title.into())),
                metadata: None,
                budget: None,
                tools: None,
                mcp_update: None,
                idempotency_key: key.map(str::to_owned),
                request_fingerprint: awaken_agent_contract::stable_fingerprint(&title),
                if_match,
            };
            let delayed_command = command(
                "updated",
                keyed.then_some("shared"),
                (schedule == "conflict").then_some(origin.revision),
            );
            let pause = repo.pause_next_mutation(if keyed {
                "managed:update-command:"
            } else {
                "session:update:"
            });
            let delayed_app = app.clone();
            let delayed_id = id.clone();
            let delayed_request = delayed_command.clone();
            let delayed = tokio::spawn(async move {
                delayed_app
                    .update_session(&delayed_id, delayed_request)
                    .await
            });
            tokio::time::timeout(std::time::Duration::from_secs(5), pause.entered.notified())
                .await
                .unwrap();
            assert_eq!(
                pause
                    .candidate
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .expected_revision,
                origin.revision,
                "U1/U2/U3 detached original candidate"
            );
            let winner_command = if schedule == "conflict" {
                command("different winner", keyed.then_some("different"), None)
            } else {
                delayed_command.clone()
            };
            let winner = app
                .update_session(&id, winner_command.clone())
                .await
                .unwrap();
            assert!(winner.command_applied, "U1 actual winner");
            assert_eq!(
                winner.command_revision, winner.session.revision,
                "U1 original commit"
            );
            if schedule == "later_root" {
                let mut later = durable.get(&id).await.unwrap();
                later.metadata.insert("later".into(), "preserved".into());
                let advanced = app
                    .commit_session_snapshot_with_outcome(
                        "workspace",
                        awaken_session_state_contract::SessionAggregate::try_from(later)
                            .expect("valid test snapshot"),
                        "independent-after-update",
                        Vec::new(),
                    )
                    .await
                    .unwrap();
                assert!(
                    advanced.transitioned(),
                    "U2 independent root really applied"
                );
                assert!(
                    advanced.committed_revision() > winner.command_revision,
                    "U2 later receipt advances beyond the original command"
                );
            }
            let before_replay = durable.get(&id).await.unwrap();
            pause.release.notify_one();
            let delayed = tokio::time::timeout(std::time::Duration::from_secs(5), delayed)
                .await
                .unwrap()
                .unwrap();
            let actual = pause.result.lock().unwrap().clone().unwrap();
            if schedule == "conflict" {
                assert!(
                    matches!(actual, SessionMutationResult::Conflict { .. }),
                    "U3 real CAS"
                );
                assert!(
                    matches!(delayed, Err(SessionUpdateError::Conflict)),
                    "U3 no candidate acceptance"
                );
            } else {
                assert_eq!(
                    actual,
                    SessionMutationResult::Replayed {
                        new_revision: winner.command_revision,
                    },
                    "U1/U2 actual repository receipt"
                );
                let replayed = delayed.unwrap();
                assert_eq!(
                    (replayed.command_applied, replayed.command_revision),
                    (false, winner.command_revision),
                    "U1/U2 keyed={keyed}, schedule={schedule}, repository={actual:?}, current root={:?}",
                    replayed.session.revision,
                );
                assert_eq!(
                    replayed.session, before_replay,
                    "U2 replay reloads the full durable root"
                );
                if schedule == "later_root" {
                    assert!(
                        replayed.session.revision > replayed.command_revision,
                        "U2 current root and original command are distinct coordinates"
                    );
                }
            }
            assert_eq!(
                durable.get(&id).await.unwrap(),
                before_replay,
                "U1-U3 no second root mutation"
            );
            let mut noop = winner_command;
            noop.if_match = None;
            let noop = app.update_session(&id, noop).await.unwrap();
            assert!(!noop.changes.any(), "U4 no event-producing change mask");
            assert_eq!(
                durable.get(&id).await.unwrap(),
                before_replay,
                "U4 no extra root/receipt"
            );
            assert_eq!(
                environments.dispatch_calls.load(Ordering::SeqCst),
                0,
                "U1-U4 metadata never starts execution"
            );
            assert!(
                environments.retired.lock().unwrap().is_empty(),
                "U1-U4 no provider release"
            );
        }
    }
}
