//! Entry, accepted resume, ownership and persistence failure partitions.
use super::*;
use awaken_agent_contract::audit::kind::Kind;

fn context(committed: &Arc<RecordingCoordinator>) -> RuntimeRunContext {
    RuntimeRunContext::new()
        .with_commit(committed.clone())
        .with_reader(committed.clone())
}

async fn seed(
    committed: &Arc<RecordingCoordinator>,
    activation: &RunActivation,
    run: RunDisposition,
) {
    crate::commit::commit(
        &context(committed),
        vec![],
        None,
        &activation.thread_id,
        run,
        vec![],
        vec![],
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn entry_distinguishes_running_terminal_awaiting_and_missing_commit() {
    // C1 original state and C2 commit availability partition entry. G2 Running
    // -> no duplicate entry; G3 terminal -> same outcome/no open or new write;
    // G4 Awaiting without accepted resume -> refuse/preserve ticket; G5 missing
    // commit -> refuse before open. Fresh+available is G1 in the parent test.
    for rule in ["running", "terminal", "awaiting", "missing-commit"] {
        let (executor, committed, observed) = fixture();
        let activation = activation_without_session_home();
        match rule {
            "running" => {
                seed(
                    &committed,
                    &activation,
                    RunDisposition::running(activation.run_id.clone()),
                )
                .await
            }
            "terminal" => {
                seed(
                    &committed,
                    &activation,
                    RunDisposition::ended(activation.run_id.clone(), EndCause::Cancelled),
                )
                .await
            }
            "awaiting" => {
                seed(
                    &committed,
                    &activation,
                    RunDisposition::awaiting(permission_ticket(&activation)),
                )
                .await
            }
            _ => {}
        }
        let before = committed.commits.lock().len();
        let ctx = if rule == "missing-commit" {
            RuntimeRunContext::new().with_reader(committed.clone())
        } else {
            context(&committed)
        };
        let result = executor.execute(activation.clone(), ctx).await;
        match rule {
            "running" => {
                assert_eq!(result.unwrap(), RunState::Ended(EndCause::NaturalEnd));
                assert_eq!(*observed.lock().await, vec![Some(RunState::Running)]);
                assert_eq!(committed.commits.lock().len(), before + 1);
            }
            "terminal" => {
                assert_eq!(result.unwrap(), RunState::Ended(EndCause::Cancelled));
                assert!(observed.lock().await.is_empty());
                assert_eq!(committed.commits.lock().len(), before);
            }
            _ => {
                assert!(result.is_err(), "{rule}");
                assert!(observed.lock().await.is_empty(), "{rule}");
                assert_eq!(committed.commits.lock().len(), before, "{rule}");
                if rule == "awaiting" {
                    assert!(committed.resume_ticket(&activation.run_id).is_some());
                }
            }
        }
    }
}

#[tokio::test]
async fn accepted_pause_and_permission_resumes_replace_ticket_before_channel_entry() {
    // C1 pause/opaque permission allow/deny; C2 named/unnamed delivery; C3 exact
    // committed ticket. Six interacting G6 rows -> Running + ticket removal
    // before open; named input has exactly one ResumeApplied in that same entry,
    // unnamed input has none. A CLI that does not reissue a permission-bound
    // tool gets the existing unexecuted-result settlement, never an effect.
    for reason in ["pause", "allow", "deny"] {
        for named in [false, true] {
            let (executor, committed, observed) = fixture();
            let activation = activation_without_session_home();
            let ticket = if reason == "pause" {
                ResumeTicket::new(
                    "pause",
                    activation.run_id.clone(),
                    activation.thread_id.clone(),
                    activation.snapshot.id.0.clone(),
                    activation.snapshot.fingerprint.0.clone(),
                    AwaitTarget::Pause(PauseReason::Manual),
                )
            } else {
                permission_ticket(&activation)
            };
            seed(
                &committed,
                &activation,
                RunDisposition::awaiting(ticket.clone()),
            )
            .await;
            let answer = match reason {
                "pause" => ResumeResult::Input("continue".into()),
                "allow" => ResumeResult::allow(),
                _ => ResumeResult::Permission(PermissionDecision::Deny {
                    reason: Some("denied".into()),
                }),
            };
            let mut command = ResumeCommand::from_ticket(&ticket, answer, 0);
            if named {
                command = command.with_operation_id("entry-resume");
            }
            assert_eq!(
                executor
                    .resume(activation.clone(), command, context(&committed))
                    .await
                    .unwrap(),
                RunState::Ended(EndCause::NaturalEnd)
            );
            assert_eq!(
                *observed.lock().await,
                vec![Some(RunState::Running)],
                "{reason}/{named}"
            );
            let commits = committed.commits.lock();
            assert_eq!(commits.len(), 3);
            assert_eq!(commits[1].run_state(), RunState::Running);
            assert!(commits[1].resume_ticket().is_none());
            assert!(commits[1].messages.is_empty());
            assert_eq!(
                commits[1]
                    .events
                    .iter()
                    .filter(|e| e.kind == Kind::ResumeApplied)
                    .count(),
                usize::from(named)
            );
            assert_eq!(
                commits
                    .iter()
                    .flat_map(|c| &c.events)
                    .filter(|e| e.kind == Kind::ResumeApplied)
                    .count(),
                usize::from(named)
            );
        }
    }
}

#[tokio::test]
async fn invalid_resume_never_consumes_ticket_or_opens_channel() {
    // G7 each independent identity/result mutation -> no entry/receipt/open.
    // The shared validate_resume complete binding table owns all field/deadline
    // combinations; these witnesses compose it before the new entry boundary.
    for mutation in ["correlation", "thread", "snapshot", "result"] {
        let (executor, committed, observed) = fixture();
        let activation = activation_without_session_home();
        let ticket = permission_ticket(&activation);
        seed(
            &committed,
            &activation,
            RunDisposition::awaiting(ticket.clone()),
        )
        .await;
        let mut command = ResumeCommand::from_ticket(&ticket, ResumeResult::allow(), 0)
            .with_operation_id("inert-invalid");
        match mutation {
            "correlation" => command.correlation_id = "foreign".into(),
            "thread" => command.thread_id = ThreadId("foreign".into()),
            "snapshot" => command.snapshot_id = ExecutableAgentSnapshotId("foreign".into()),
            _ => command.result = ResumeResult::Input("not a permission".into()),
        }
        assert!(
            executor
                .resume(activation.clone(), command, context(&committed))
                .await
                .is_err()
        );
        assert!(observed.lock().await.is_empty());
        assert_eq!(committed.commits.lock().len(), 1);
        assert_eq!(committed.resume_ticket(&activation.run_id), Some(ticket));
    }
}

struct FaultCoordinator {
    inner: Arc<RecordingCoordinator>,
    after_apply: bool,
    armed: AtomicBool,
}

#[async_trait]
impl Coordinator for FaultCoordinator {
    async fn commit(&self, commit: ThreadCommit) -> std::result::Result<CommitRecord, CommitError> {
        if self.armed.swap(false, Ordering::SeqCst) {
            if self.after_apply {
                self.inner.commit(commit).await?;
            }
            return Err(CommitError::Rejected("entry fault".into()));
        }
        self.inner.commit(commit).await
    }
}

#[tokio::test]
async fn rejected_or_lost_entry_ack_never_launches_or_splits_resume_receipt() {
    // C1 fresh/resume; C2 failure before apply/after apply before acknowledgement.
    // Four G8 rules -> zero channel opens. Before apply preserves None/Awaiting
    // without receipt; after apply retains Running and (only for resume) exactly
    // one receipt with the ticket removed. An exact answered command cannot be
    // replayed into another channel after that successful durable consumption.
    for resumed in [false, true] {
        for after_apply in [false, true] {
            let (executor, committed, observed) = fixture();
            let activation = activation_without_session_home();
            let ticket = permission_ticket(&activation);
            if resumed {
                seed(
                    &committed,
                    &activation,
                    RunDisposition::awaiting(ticket.clone()),
                )
                .await;
            }
            let ctx = context(&committed).with_commit(Arc::new(FaultCoordinator {
                inner: committed.clone(),
                after_apply,
                armed: AtomicBool::new(true),
            }));
            let command = ResumeCommand::from_ticket(&ticket, ResumeResult::allow(), 0)
                .with_operation_id("fault-resume");
            let result = if resumed {
                executor
                    .resume(activation.clone(), command.clone(), ctx)
                    .await
            } else {
                executor.execute(activation.clone(), ctx).await
            };
            assert!(result.is_err());
            assert!(observed.lock().await.is_empty());
            assert_eq!(
                committed.run_state(&activation.run_id),
                if after_apply {
                    Some(RunState::Running)
                } else if resumed {
                    Some(RunState::Awaiting)
                } else {
                    None
                }
            );
            let receipts = committed
                .commits
                .lock()
                .iter()
                .flat_map(|c| &c.events)
                .filter(|e| e.kind == Kind::ResumeApplied)
                .count();
            assert_eq!(receipts, usize::from(resumed && after_apply));
            if resumed && after_apply {
                assert!(committed.resume_ticket(&activation.run_id).is_none());
                assert!(
                    executor
                        .resume(activation.clone(), command, context(&committed))
                        .await
                        .is_err()
                );
                assert!(observed.lock().await.is_empty());
            }
        }
    }
}

#[tokio::test]
async fn ownership_failure_before_or_after_entry_prevents_channel_open() {
    // G9 loss/unavailability before entry -> no write/open. G10 loss after
    // committed entry before open -> Running remains, no stale terminal write
    // or process launch. Existing physical recovery classifies interruption;
    // these tests grant no additional retry or provider authorization.
    for decisions in [
        vec![OwnershipDecision::Lost],
        vec![OwnershipDecision::Unavailable],
        vec![OwnershipDecision::Current, OwnershipDecision::Lost],
    ] {
        let after_entry = decisions.len() == 2;
        let (executor, committed, observed) = fixture();
        let activation = activation_without_session_home();
        let ctx = context(&committed).with_ownership(Arc::new(ScriptedOwnership::new(decisions)));
        assert!(executor.execute(activation.clone(), ctx).await.is_err());
        assert!(observed.lock().await.is_empty());
        assert_eq!(
            committed.run_state(&activation.run_id),
            after_entry.then_some(RunState::Running)
        );
    }
}
