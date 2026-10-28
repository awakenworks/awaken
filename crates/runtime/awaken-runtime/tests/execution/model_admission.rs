use super::*;

struct RefusedGate {
    unavailable: bool,
    uncertain: bool,
    evidence: awaken_runtime_contract::runtime_context::AttemptQuiescence,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelRequestGate for RefusedGate {
    fn process_bound(&self) -> bool {
        true
    }

    async fn admit_model_request(
        &self,
        _: ModelRequestAdmissionRequest,
    ) -> awaken_runtime_contract::llm::Result<ModelRequestAdmission> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.uncertain {
            self.evidence.mark_uncertain();
        }
        Err(if self.unavailable {
            awaken_runtime_contract::llm::Error::Provider("admission dependency unavailable".into())
        } else {
            awaken_runtime_contract::llm::Error::Binding("admission request rejected".into())
        })
    }
}

#[tokio::test]
async fn model_request_gate_failure_keeps_admission_origin_without_provider_entry() {
    // Cause/effect rules: R1 an unavailable admission dependency -> the known
    // admission-unavailable code; R2 a rejected request -> admission-rejected.
    // R3 either grade + an independent physical veto -> Indeterminate, never
    // an ordinary failure that could acknowledge the other participant.
    // All rows return before inference: exactly one gate call, zero Provider calls,
    // zero model-request audit observations, and no fresh retry budget. The
    // existing gate decision-table test covers Admit/Pause, continuation and
    // Provider retries; this table cannot turn a gate refusal into Provider work.
    for unavailable in [false, true] {
        for uncertain in [false, true] {
            let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
            let context = RuntimeRunContext::new().with_commit(commit.clone());
            let gate = Arc::new(RefusedGate {
                unavailable,
                uncertain,
                evidence: context.physical_quiescence.clone(),
                calls: AtomicUsize::new(0),
            });
            let model = Arc::new(RetryOnceLlm(AtomicUsize::new(0)));
            let outcome = Runtime::new()
                .with_llm(model.clone())
                .with_infer_retries(3)
                .execute(
                    activation("catalog-a"),
                    context.with_model_request_gate(gate.clone()),
                )
                .await
                .expect("typed terminal Run");
            let expected = if unavailable {
                "model_admission_unavailable"
            } else {
                "model_admission_rejected"
            };
            if uncertain {
                assert_eq!(outcome, RunState::Ended(EndCause::Indeterminate));
            } else {
                assert!(
                    matches!(&outcome, RunState::Ended(EndCause::Error(
                awaken_agent_contract::agent::run::Failure::Inference { code, .. }
            )) if code == expected),
                    "unavailable={unavailable}: {outcome:?}"
                );
            }
            assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
            assert_eq!(model.0.load(Ordering::SeqCst), 0);
            assert!(committed_model_requests(&commit).is_empty());
        }
    }
}

struct DrainingGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    completed: AtomicUsize,
    outcome: u8,
}

#[async_trait::async_trait]
impl ModelRequestGate for DrainingGate {
    fn process_bound(&self) -> bool {
        true
    }
    async fn admit_model_request(
        &self,
        _: ModelRequestAdmissionRequest,
    ) -> awaken_runtime_contract::llm::Result<ModelRequestAdmission> {
        self.entered.notify_one();
        self.release.notified().await;
        self.completed.fetch_add(1, Ordering::SeqCst);
        match self.outcome {
            0 => Ok(ModelRequestAdmission::Admit),
            1 => Ok(ModelRequestAdmission::Pause(
                awaken_agent_contract::agent::awaiting::PauseReason::BudgetReached,
            )),
            _ => Err(awaken_runtime_contract::llm::Error::Binding(
                "admission rejected after accounting returned".into(),
            )),
        }
    }
}

#[tokio::test]
async fn cancellation_drains_entered_model_admission_before_ending_without_provider_entry() {
    // Cause/effect graph: C1 gate entered an owned accounting operation; C2 Run
    // cancellation arrives before its response; C3 result Admit/Pause/Error;
    // C4 an independent physical veto appears while admission is pending. R1 C1+C2
    // must remain pending with no local return ACK until the SAME gate returns.
    // R2 return+C2+!C4 dominates every C3: Ended(Cancelled), confirmed local return.
    // R3 return+C2+C4 => logical Cancelled but no local ACK: draining this gate cannot
    // erase another participant's physical uncertainty. Every row has one completed gate,
    // no model call or audit. A cancelled read may not turn
    // this accounting write into an abandoned future or unknown physical slot.
    use futures_util::FutureExt;
    for (outcome, uncertain) in
        (0..3).flat_map(|outcome| [false, true].map(|uncertain| (outcome, uncertain)))
    {
        let gate = Arc::new(DrainingGate {
            entered: Default::default(),
            release: Default::default(),
            completed: AtomicUsize::new(0),
            outcome,
        });
        let cancellation = awaken_runtime_contract::CancellationToken::new();
        let commit = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        let model = Arc::new(RetryOnceLlm(AtomicUsize::new(0)));
        let context = RuntimeRunContext::new()
            .with_commit(commit.clone())
            .with_model_request_gate(gate.clone())
            .with_cancellation(cancellation.clone());
        let physical = context.physical_quiescence.clone();
        let runtime = Runtime::new().with_llm(model.clone());
        let execution = runtime.execute(activation("catalog-a"), context);
        tokio::pin!(execution);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::select! {
                biased;
                () = gate.entered.notified() => {},
                result = execution.as_mut() => panic!("gate must be pending: {result:?}"),
            }
        })
        .await
        .expect("gate entered before cancellation");
        if uncertain {
            physical.mark_uncertain();
        }
        cancellation.cancel();
        assert!(
            execution.as_mut().now_or_never().is_none(),
            "R1 drains in-flight admission"
        );
        assert!(
            !physical.can_acknowledge_return(),
            "R1 no premature local return"
        );
        gate.release.notify_one();
        let state = tokio::time::timeout(std::time::Duration::from_secs(2), execution)
            .await
            .expect("owned admission returned")
            .expect("cancelled Run commits");
        assert_eq!(
            state,
            RunState::Ended(EndCause::Cancelled),
            "R2/R3 outcome={outcome} uncertain={uncertain}"
        );
        assert_eq!(
            gate.completed.load(Ordering::SeqCst),
            1,
            "same admission finished once"
        );
        assert_eq!(
            model.0.load(Ordering::SeqCst),
            0,
            "no provider request after cancel"
        );
        assert!(
            committed_model_requests(&commit).is_empty(),
            "no synthetic model operation"
        );
        assert_eq!(
            physical.close_and_wait().await,
            !uncertain,
            "only an independent veto prevents local acknowledgement"
        );
    }
}
