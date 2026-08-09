use awaken_session_contract::{
    SessionResidencyAction, SessionResidencyPolicy, decide_session_residency,
};

use super::{ManagedState, StateError};

impl ManagedState {
    /// Reconcile rebuildable Session capabilities in the existing Session
    /// supervisor. Full environment suspension stays disabled until a provider
    /// can durably retain its workspace; hibernating Hand is lossless today.
    pub(crate) async fn reconcile_session_residency(
        &self,
        now_unix_ms: u64,
    ) -> Result<usize, StateError> {
        const POLICY: SessionResidencyPolicy = SessionResidencyPolicy {
            hand_idle_after_ms: 300_000,
        };

        let sessions = self.sessions_repo.reconcilable_sessions().await;
        let mut changed = 0;
        for scoped in sessions {
            let session = scoped.session;
            if matches!(session.status.as_str(), "deleted" | "terminated") {
                continue;
            }
            if decide_session_residency(
                &session.activity,
                &session.environment,
                now_unix_ms,
                POLICY,
            ) == SessionResidencyAction::HibernateHand
                && self
                    .runtime
                    .hibernate_session_environment(&session.session_id)
                    .await?
            {
                changed += 1;
            }
        }
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use awaken_agent_contract::agent::content::ContentBlock;

    use super::*;

    #[derive(Default)]
    struct HibernateRuntime {
        calls: Arc<Mutex<Vec<String>>>,
        fails: bool,
    }

    #[async_trait::async_trait]
    impl crate::state::SessionRuntime for HibernateRuntime {
        async fn run(
            &self,
            _agent: &str,
            _thread: &str,
            _content: Vec<ContentBlock>,
        ) -> Result<crate::state::StepOutcome, crate::state::RunError> {
            unreachable!()
        }

        async fn resume(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _decision: crate::state::ToolPermissionDecision,
        ) -> Result<crate::state::StepOutcome, crate::state::RunError> {
            unreachable!()
        }

        async fn resume_custom(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _content: Vec<ContentBlock>,
            _is_error: bool,
        ) -> Result<crate::state::StepOutcome, crate::state::RunError> {
            unreachable!()
        }

        async fn add_system(
            &self,
            _thread: &str,
            _text: &str,
        ) -> Result<(), crate::state::RunError> {
            Ok(())
        }

        async fn define_outcome(
            &self,
            _thread: &str,
            _description: &str,
            _rubric: &str,
            _max_iterations: u32,
        ) -> Result<crate::state::OutcomeReport, crate::state::RunError> {
            unreachable!()
        }

        async fn hibernate_session_environment(
            &self,
            thread: &str,
        ) -> Result<bool, crate::state::RunError> {
            self.calls.lock().unwrap().push(thread.to_string());
            if self.fails {
                Err(crate::state::RunError::unavailable("hibernate unavailable"))
            } else {
                Ok(true)
            }
        }

        fn model(&self) -> String {
            "test-model".into()
        }
    }

    /// Supervisor decision table at the application/port boundary.
    /// C1=resident environment; C2=idle reason is EndTurn; C3=idle age reaches
    /// five minutes; C4=Session is terminal. E1=call the sole runtime hibernate
    /// port once; E2=no runtime effect. Rules: S1 C1+C2+C3+!C4=>E1;
    /// S2 !C2=>E2; S3 !C3=>E2; S4 C4=>E2.
    #[tokio::test]
    async fn supervisor_hibernates_only_non_terminal_expired_end_turns() {
        let runtime = HibernateRuntime::default();
        let hibernated = runtime.calls.clone();
        let state = ManagedState::new(runtime);
        let request =
            || serde_json::from_value(serde_json::json!({ "agent": "assistant" })).unwrap();
        let old = state.create_session(request(), None).await.unwrap();
        let fresh = state.create_session(request(), None).await.unwrap();
        let awaiting = state.create_session(request(), None).await.unwrap();
        let terminal = state.create_session(request(), None).await.unwrap();

        for (id, activity, status) in [
            (
                old.id.as_str(),
                awaken_session_contract::SessionActivityState::Idle {
                    reason: awaken_session_contract::SessionIdleReason::EndTurn,
                    since_unix_ms: 0,
                },
                "idle",
            ),
            (
                fresh.id.as_str(),
                awaken_session_contract::SessionActivityState::Idle {
                    reason: awaken_session_contract::SessionIdleReason::EndTurn,
                    since_unix_ms: 900_000,
                },
                "idle",
            ),
            (
                awaiting.id.as_str(),
                awaken_session_contract::SessionActivityState::Idle {
                    reason: awaken_session_contract::SessionIdleReason::AwaitingAction,
                    since_unix_ms: 0,
                },
                "idle",
            ),
            (
                terminal.id.as_str(),
                awaken_session_contract::SessionActivityState::Idle {
                    reason: awaken_session_contract::SessionIdleReason::EndTurn,
                    since_unix_ms: 0,
                },
                "terminated",
            ),
        ] {
            let mut persisted = state.sessions_repo.get(id).await.unwrap();
            persisted.activity.state = activity;
            persisted.environment.set_resident(format!("binding-{id}"));
            persisted.status = status.into();
            state
                .commit_session_snapshot(
                    crate::state::DEFAULT_SCOPE,
                    persisted,
                    "test-residency",
                    Vec::new(),
                )
                .await
                .unwrap();
        }

        assert_eq!(
            state.reconcile_session_residency(1_000_000).await.unwrap(),
            1
        );
        assert_eq!(hibernated.lock().unwrap().as_slice(), &[old.id]);
    }

    /// Failure rule S5: an eligible idle Session plus a runtime hibernation
    /// failure produces no false success/count and leaves the durable aggregate
    /// Resident, so the canonical supervisor can retry on its next tick.
    #[tokio::test]
    async fn supervisor_fails_closed_and_leaves_hibernation_retryable() {
        let runtime = HibernateRuntime {
            fails: true,
            ..Default::default()
        };
        let hibernated = runtime.calls.clone();
        let state = ManagedState::new(runtime);
        let request = serde_json::from_value(serde_json::json!({ "agent": "assistant" })).unwrap();
        let session = state.create_session(request, None).await.unwrap();
        let mut persisted = state.sessions_repo.get(&session.id).await.unwrap();
        persisted.activity.state = awaken_session_contract::SessionActivityState::Idle {
            reason: awaken_session_contract::SessionIdleReason::EndTurn,
            since_unix_ms: 0,
        };
        persisted.environment.set_resident("binding");
        state
            .commit_session_snapshot(
                crate::state::DEFAULT_SCOPE,
                persisted,
                "test-residency",
                Vec::new(),
            )
            .await
            .unwrap();

        assert!(state.reconcile_session_residency(1_000_000).await.is_err());
        assert_eq!(hibernated.lock().unwrap().as_slice(), &[session.id.clone()]);
        assert_eq!(
            state
                .sessions_repo
                .get(&session.id)
                .await
                .unwrap()
                .environment
                .binding(),
            Some("binding")
        );
    }
}
