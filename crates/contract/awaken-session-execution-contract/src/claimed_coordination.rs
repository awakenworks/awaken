// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Static owner: claim-fenced Session coordination port. Dynamic path: a Worker may read or
// @formal reason: mutate Session coordination state only through the exact claimed dispatch snapshot and
// @formal reason: epoch. Evidence: worker.remote_execution.remote_protocol and claimed-worker conformance
// @formal reason: tests; this is a product-requirement classification, not a direct proof claim.
//! Claim-fenced Session Agent coordination over the authenticated Worker
//! transport.
//!
//! The Session owns this role; the dispatch claim only fences it. The claim is
//! a type parameter because the Session never interprets it: the Coordinator
//! transport validates it before calling the claim-free Session application,
//! and the Session contract takes no dependency on the dispatch vocabulary.

use awaken_session_realization_contract::ClaimedSessionControlError;

use crate::{
    SessionAgentBoundaryCommand, SessionAgentMessageCommand, SessionAgentMessageReceipt,
    SessionAgentRosterEntry, SessionRunActivityAdmission, SessionRunActivityAdmissionMode,
};

/// Claim-fenced Session Agent coordination. It is independent from
/// realization resume so a topology that only recovers projections cannot
/// inherit runtime failures for coordination.
#[async_trait::async_trait]
pub trait ClaimedSessionCoordinationControl<Claim: Send + Sync>: Send + Sync {
    async fn list_session_agents(
        &self,
        claim: &Claim,
        session_id: &str,
    ) -> Result<Vec<SessionAgentRosterEntry>, ClaimedSessionControlError>;

    async fn admit_session_model_request(
        &self,
        claim: &Claim,
        session_id: &str,
        thread_id: &awaken_agent_contract::agent::thread::Id,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<bool, ClaimedSessionControlError>;

    async fn admit_session_run_activity(
        &self,
        claim: &Claim,
        session_id: &str,
        agent_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
        mode: SessionRunActivityAdmissionMode,
    ) -> Result<SessionRunActivityAdmission, ClaimedSessionControlError>;

    async fn send_session_agent_message(
        &self,
        claim: &Claim,
        command: SessionAgentMessageCommand,
    ) -> Result<SessionAgentMessageReceipt, ClaimedSessionControlError>;

    async fn settle_session_agent_boundary(
        &self,
        claim: &Claim,
        command: SessionAgentBoundaryCommand,
    ) -> Result<(), ClaimedSessionControlError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use std::sync::Mutex;

    /// A claim the Session never interprets: only the transport compares it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct FenceClaim {
        run: &'static str,
        generation: u64,
    }

    /// Transport fake that fences every call on the exact current claim and
    /// records the claim it was handed.
    struct FencedTransport {
        current: FenceClaim,
        seen: Mutex<Vec<FenceClaim>>,
    }

    impl FencedTransport {
        fn admit(&self, claim: &FenceClaim) -> Result<(), ClaimedSessionControlError> {
            self.seen.lock().unwrap().push(claim.clone());
            if claim == &self.current {
                Ok(())
            } else {
                Err(ClaimedSessionControlError::new("stale claim"))
            }
        }
    }

    #[async_trait::async_trait]
    impl ClaimedSessionCoordinationControl<FenceClaim> for FencedTransport {
        async fn list_session_agents(
            &self,
            claim: &FenceClaim,
            session_id: &str,
        ) -> Result<Vec<SessionAgentRosterEntry>, ClaimedSessionControlError> {
            self.admit(claim)?;
            Ok(vec![SessionAgentRosterEntry {
                agent_id: format!("{session_id}-agent"),
                name: "agent".into(),
                description: None,
            }])
        }

        async fn admit_session_model_request(
            &self,
            claim: &FenceClaim,
            _session_id: &str,
            _thread_id: &ThreadId,
            run_id: &RunId,
        ) -> Result<bool, ClaimedSessionControlError> {
            self.admit(claim)?;
            Ok(run_id.0 == claim.run)
        }

        async fn admit_session_run_activity(
            &self,
            claim: &FenceClaim,
            _session_id: &str,
            _agent_id: &str,
            _run_id: &RunId,
            mode: SessionRunActivityAdmissionMode,
        ) -> Result<SessionRunActivityAdmission, ClaimedSessionControlError> {
            self.admit(claim)?;
            Ok(match mode {
                SessionRunActivityAdmissionMode::RecoverOrAdmit => {
                    SessionRunActivityAdmission::Admitted {
                        session_activity_epoch: claim.generation,
                    }
                }
                SessionRunActivityAdmissionMode::RecoverOnly => {
                    SessionRunActivityAdmission::Rejected
                }
            })
        }

        async fn send_session_agent_message(
            &self,
            claim: &FenceClaim,
            command: SessionAgentMessageCommand,
        ) -> Result<SessionAgentMessageReceipt, ClaimedSessionControlError> {
            self.admit(claim)?;
            Ok(SessionAgentMessageReceipt {
                thread_id: command.source_thread_id,
            })
        }

        async fn settle_session_agent_boundary(
            &self,
            claim: &FenceClaim,
            _command: SessionAgentBoundaryCommand,
        ) -> Result<(), ClaimedSessionControlError> {
            self.admit(claim)
        }
    }

    #[tokio::test]
    async fn coordination_port_carries_any_claim_verbatim_to_its_fencing_transport() {
        // Cause/effect table for the generic claim parameter: C1 the caller's
        // claim is the transport's current claim or a stale one. E1 C1 current
        // -> the call reaches the Session answer; E2 C1 stale -> the transport's
        // claimed-control error, with no Session answer; E3 in both rows the
        // transport receives exactly the caller's claim. The port compiles for
        // a claim type that is not the dispatch claim, so the Session contract
        // needs no dispatch vocabulary.
        let current = FenceClaim {
            run: "run-1",
            generation: 7,
        };
        let stale = FenceClaim {
            run: "run-1",
            generation: 6,
        };
        let transport = FencedTransport {
            current: current.clone(),
            seen: Mutex::new(Vec::new()),
        };
        let port: &dyn ClaimedSessionCoordinationControl<FenceClaim> = &transport;

        let roster = port
            .list_session_agents(&current, "session-1")
            .await
            .expect("E1 roster");
        assert_eq!(roster[0].agent_id, "session-1-agent", "E1");
        assert_eq!(
            port.admit_session_run_activity(
                &current,
                "session-1",
                "agent",
                &RunId("run-1".into()),
                SessionRunActivityAdmissionMode::RecoverOrAdmit,
            )
            .await,
            Ok(SessionRunActivityAdmission::Admitted {
                session_activity_epoch: 7
            }),
            "E1"
        );
        assert_eq!(
            port.admit_session_model_request(
                &current,
                "session-1",
                &ThreadId("session-1".into()),
                &RunId("run-1".into()),
            )
            .await,
            Ok(true),
            "E1"
        );

        let refused = port
            .list_session_agents(&stale, "session-1")
            .await
            .expect_err("E2 stale claim");
        assert_eq!(
            refused,
            ClaimedSessionControlError::new("stale claim"),
            "E2"
        );
        assert!(
            port.admit_session_run_activity(
                &stale,
                "session-1",
                "agent",
                &RunId("run-1".into()),
                SessionRunActivityAdmissionMode::RecoverOrAdmit,
            )
            .await
            .is_err(),
            "E2"
        );

        assert_eq!(
            *transport.seen.lock().unwrap(),
            vec![
                current.clone(),
                current.clone(),
                current,
                stale.clone(),
                stale
            ],
            "E3"
        );
    }
}
