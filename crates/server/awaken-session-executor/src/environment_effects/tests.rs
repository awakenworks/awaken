//! Realizer effects continue with only their selected roles after the Host is gone.
use super::*;
use awaken_dispatch_contract::dispatch::AttemptExecutionBinding;
use awaken_dispatch_contract::*;
use awaken_session_execution_contract::SessionEnvironmentBindingSink;
use awaken_session_state_contract::{SessionEnvironmentReceipt, SessionEnvironmentState};
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct Execution {
    outcome: &'static str,
    bindings: Mutex<Vec<(RunClaim, String)>>,
}

#[cfg(test)]
#[async_trait::async_trait]
impl DispatchExecution for Execution {
    async fn close_attempt(&self, _: &RunClaim) -> Result<SettleOutcome, DispatchError> {
        unreachable!("only binding was requested")
    }
    async fn renew_lease(
        &self,
        _: &RunClaim,
        _: u64,
    ) -> Result<LeaseRenewalOutcome, DispatchError> {
        unreachable!("only binding was requested")
    }
    async fn begin_attempt(
        &self,
        _: &RunClaim,
        _: Option<&AttemptExecutionBinding>,
    ) -> Result<AttemptAdmission, DispatchError> {
        unreachable!("only binding was requested")
    }
    async fn handoff_attempt(
        &self,
        _: &RunClaim,
        _: Option<&AttemptExecutionBinding>,
    ) -> Result<AttemptAdmission, DispatchError> {
        unreachable!("only binding was requested")
    }
    async fn finish_attempt(
        &self,
        _: &RunClaim,
        _: Option<AttemptReturnCause>,
    ) -> Result<SettleOutcome, DispatchError> {
        unreachable!("only binding was requested")
    }
    async fn relinquish_claim(&self, _: &RunClaim) -> Result<SettleOutcome, DispatchError> {
        unreachable!("only binding was requested")
    }
    async fn defer_claim(&self, _: &RunClaim, _: u64) -> Result<SettleOutcome, DispatchError> {
        unreachable!("only binding was requested")
    }
    async fn record_credential_realization(
        &self,
        _: &RunClaim,
        _: CredentialRealizationReceipt,
    ) -> Result<SettleOutcome, DispatchError> {
        unreachable!("only binding was requested")
    }
    async fn bind_sandbox(
        &self,
        claim: &RunClaim,
        binding: &str,
    ) -> Result<SettleOutcome, DispatchError> {
        self.bindings
            .lock()
            .unwrap()
            .push((claim.clone(), binding.into()));
        match self.outcome {
            "fenced" => Ok(SettleOutcome::Fenced),
            "unavailable" => Err(DispatchError::Unavailable(
                "selected transport unavailable".into(),
            )),
            _ => Ok(SettleOutcome::Applied),
        }
    }
}

struct Sink {
    response_lost: bool,
    persists: AtomicUsize,
}

#[async_trait::async_trait]
impl SessionEnvironmentBindingSink for Sink {
    async fn authorize(
        &self,
        intent: &SessionEnvironmentEffectIntent,
    ) -> Result<SessionEnvironmentEffectAuthorization, awaken_session_types::RunError> {
        Ok(SessionEnvironmentEffectAuthorization::Authorized {
            intent: Box::new(intent.clone()),
        })
    }
    async fn persist(
        &self,
        receipt: SessionEnvironmentReceipt,
    ) -> Result<SessionEnvironmentState, awaken_session_types::RunError> {
        self.persists.fetch_add(1, Ordering::SeqCst);
        if self.response_lost {
            return Err(awaken_session_types::RunError::unavailable(
                "store response unavailable",
            ));
        }
        let generation = awaken_session_state_contract::SandboxGeneration::new(
            &receipt.session_id,
            1,
            u64::MAX,
            "selected-environment",
            "selected-image",
        );
        Ok(SessionEnvironmentState::Resident {
            binding: receipt.binding,
            effect_id: Some(receipt.effect_id),
            generation: Some(generation),
            idle_since_unix_ms: None,
        })
    }
}

async fn detached_realizer() -> (
    crate::realization_executor::RealizationExecutor,
    Arc<crate::installed_session::InstalledSessions>,
) {
    let host = crate::HostDependencies::ephemeral(
        Arc::new(crate::no_model::NoModelConfiguredExecutor),
        "stub",
    )
    .await
    .build()
    .unwrap();
    let sessions = host.installed_sessions.clone();
    let weak = Arc::downgrade(&host);
    let owner = Arc::try_unwrap(host)
        .ok()
        .expect("composition retains only weak Host edges");
    assert!(
        weak.upgrade().is_none(),
        "the operation cannot reach a Host"
    );
    (owner.realizer, sessions)
}

fn claim() -> RunClaim {
    RunClaim {
        run_id: awaken_agent_contract::agent::run::Id("original-run".into()),
        owner: "original-worker".into(),
        epoch: 7,
    }
}

#[tokio::test]
async fn detached_realizer_binds_only_the_original_claim_and_preserves_failure_classes() {
    // One selected execution role; no Host, queue discovery or other execution
    // operation. Missing claims cause no I/O, missing roles fail closed, and
    // fenced versus unavailable transports retain different failure codes.
    for row in ["direct", "applied", "fenced", "unavailable", "missing_role"] {
        let (mut realizer, sessions) = detached_realizer().await;
        let selected = Arc::new(Execution {
            outcome: row,
            bindings: Mutex::new(Vec::new()),
        });
        realizer.environments.dispatch_execution =
            (row != "missing_role").then(|| selected.clone() as Arc<dyn DispatchExecution>);
        if row != "direct" {
            sessions.update_run("session", |run| run.dispatch_claim = Some(claim()));
        }
        let result = realizer
            .bind_deferred_dispatch_before_publish("session", "exact-binding")
            .await;
        match row {
            "direct" => assert!(!result.unwrap()),
            "applied" => assert!(result.unwrap()),
            "fenced" => assert_eq!(result.unwrap_err().code(), "dispatch_claim_stale"),
            "unavailable" => assert_eq!(
                result.unwrap_err().code(),
                "dispatch_sandbox_binding_unavailable"
            ),
            "missing_role" => assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("injected dispatch binding")
            ),
            _ => unreachable!(),
        }
        let calls = selected.bindings.lock().unwrap();
        if matches!(row, "direct" | "missing_role") {
            assert!(calls.is_empty(), "{row}");
        } else {
            assert_eq!(
                calls.as_slice(),
                &[(claim(), "exact-binding".into())],
                "{row}"
            );
        }
    }
}

#[tokio::test]
async fn detached_realizer_keeps_committed_identity_and_never_rebinds_resource_reservations() {
    // P1 successful root receipt followed by stale dispatch keeps exact Store
    // generation for candidate recovery; P2 reservation never touches dispatch;
    // P3 uncertain Store response carries no invented committed identity.
    for row in ["committed_then_fenced", "reservation", "response_lost"] {
        let (mut realizer, sessions) = detached_realizer().await;
        let selected = Arc::new(Execution {
            outcome: "fenced",
            bindings: Mutex::new(Vec::new()),
        });
        realizer.environments.dispatch_execution = Some(selected.clone());
        let sink = Arc::new(Sink {
            response_lost: row == "response_lost",
            persists: AtomicUsize::new(0),
        });
        *realizer.environment_binding_sink.write().unwrap() = Some(sink.clone());
        sessions.update_run("session", |run| {
            run.dispatch_claim = Some(claim());
            run.session_dispatch = true;
        });
        let kind = if row == "reservation" {
            SessionEnvironmentEffectKind::ResourceProjectionReservation {
                transition_fingerprint: "exact-transition".into(),
            }
        } else {
            SessionEnvironmentEffectKind::Create
        };
        let intent = realizer
            .execution
            .new_environment_effect_intent(
                "session",
                kind,
                (row == "reservation").then_some("original-binding"),
            )
            .expect("installed Sessions");
        let effect = realizer
            .authorize_environment_effect_before_io("session", &intent)
            .await
            .unwrap();
        let result = realizer
            .persist_authorized_environment_binding("session", "next-binding".into(), &effect)
            .await;
        let generation = awaken_session_state_contract::SandboxGeneration::new(
            &awaken_session_types::SessionId::from("session"),
            1,
            u64::MAX,
            "selected-environment",
            "selected-image",
        );
        let expected = BoundSessionEnvironmentIdentity::Durable {
            effect_id: intent.effect_id().into(),
            generation,
        };
        assert_eq!(sink.persists.load(Ordering::SeqCst), 1, "{row}");
        match row {
            "committed_then_fenced" => {
                let failure = result.expect_err("stale dispatch cannot publish");
                assert_eq!(failure.error.code(), "dispatch_claim_stale");
                assert_eq!(failure.committed_identity, Some(expected));
                assert_eq!(
                    selected.bindings.lock().unwrap().as_slice(),
                    &[(claim(), "next-binding".into())]
                );
            }
            "reservation" => {
                assert_eq!(result.ok(), Some(expected));
                assert!(
                    selected.bindings.lock().unwrap().is_empty(),
                    "no stale Run placement write"
                );
            }
            "response_lost" => {
                assert!(
                    result
                        .expect_err("lost binding response")
                        .committed_identity
                        .is_none(),
                    "uncertainty is not a new generation"
                );
                assert!(selected.bindings.lock().unwrap().is_empty());
            }
            _ => unreachable!(),
        }
    }
}
