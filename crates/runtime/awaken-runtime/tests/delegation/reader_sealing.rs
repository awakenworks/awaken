use super::*;
use awaken_agent_contract::agent::run::Failure;

#[tokio::test]
async fn bare_resume_seals_from_its_explicit_reader_when_the_executor_is_missing() {
    // R0/R1: real Awaiting producer + missing executor + explicit reader,
    // crossed with context reader Some/None. Both terminal outcomes must seal
    // the batch and cancel intent atomically; the producer later reconciles it.
    assert_unavailable_delegation_seals(false).await;
}

#[tokio::test]
async fn bare_resume_seals_from_its_explicit_reader_when_tool_handling_is_withdrawn() {
    // R2/R3 use the same original producer and frozen snapshot, but its same
    // service withdraws handles_tool while retaining its cancellation role.
    // Cross context reader Some/None: no child resume, one durable terminal
    // batch+intent, and cancellation delivered only after that commit.
    assert_unavailable_delegation_seals(true).await;
}

async fn assert_unavailable_delegation_seals(withdraw_tool_handling: bool) {
    // Cause/effect graph: C1 one actual Awaiting delegation and retained child
    // reference; C2 executor absent versus same executor no longer handling the
    // tool; C3 an explicit authoritative prefix; C4 optional context reader
    // Some/None. R0..R3 all end CapabilityBound, finalize the batch, retain exact
    // CancelRequested and clear the ticket, with zero child resumes. An absent
    // executor defers delivery to original reconciliation; a retained service
    // delivers after commit and its same-Runtime reconciliation is inert.
    // No inserted state or physical-stop receipt. This is pre-effect sealing;
    // no child/approval commit has superseded the explicit prefix.
    for context_reader_present in [true, false] {
        let reference = serde_json::json!({"task":"remote-42"});
        let executor = Arc::new(MockRunDelegationService::new(
            Step::Awaiting(reference.clone()),
            Step::Fail("must not resume the child".into()),
        ));
        let (producer, commit) = await_on_delegation(executor.clone()).await;
        let mut replacement = Runtime::new().with_llm(Arc::new(DelegateThenText {
            calls: AtomicUsize::new(1),
        }));
        if withdraw_tool_handling {
            executor.handles_calls.store(false, Ordering::SeqCst);
            replacement = replacement.with_run_delegation(executor.clone());
        }
        replacement.register_snapshot(snapshot());
        let mut context = RuntimeRunContext::new().with_commit(commit.clone());
        if context_reader_present {
            context = context.with_reader(commit.clone());
        }
        let state = replacement
            .resume(resume_command("more please"), commit.as_ref(), context)
            .await
            .expect("the original explicit prefix admits the existing ticket");
        assert_eq!(
            state,
            RunState::Ended(EndCause::Error(Failure::CapabilityBound))
        );
        assert!(executor.resumed_with.lock().unwrap().is_empty());
        assert!(commit.resume_ticket_for(&RunId("run-1".into())).is_none());
        let store = Store::rebuild(&commit.committed().state);
        let batch = ActiveToolBatch::load(&store).unwrap().unwrap();
        assert_eq!(
            batch.phase(),
            ToolBatchPhase::Finalized,
            "terminal batch sealing/reader={context_reader_present}"
        );
        let registry = RunDelegations::load(&store).unwrap().unwrap();
        let relationship = registry.delegations().next().unwrap();
        assert_eq!(
            relationship.status,
            DelegationStatus::CancelRequested,
            "atomic child cancellation intent/reader={context_reader_present}"
        );
        assert_eq!(relationship.cancellation_reference, Some(reference));
        let cancellation = ChildRunCancellation {
            delegation_id: relationship.id.clone(),
            child_run_id: relationship.child_run_id.clone(),
            target_agent_id: relationship.target_agent_id.clone(),
            execution_reference: relationship.cancellation_reference.clone(),
        };
        let reconcile = if withdraw_tool_handling {
            assert_eq!(
                executor.cancelled.lock().unwrap().as_slice(),
                std::slice::from_ref(&cancellation)
            );
            &replacement
        } else {
            assert!(executor.cancelled.lock().unwrap().is_empty());
            &producer
        };
        assert_eq!(
            reconcile
                .reconcile_delegation_cancellations(&ThreadId("thread-1".into()), commit.as_ref())
                .await
                .unwrap(),
            usize::from(!withdraw_tool_handling),
            "only undelivered original intent is reconciled"
        );
        assert_eq!(
            executor.cancelled.lock().unwrap().as_slice(),
            &[cancellation]
        );
    }
}
