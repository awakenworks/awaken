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
