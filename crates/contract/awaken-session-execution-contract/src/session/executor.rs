// @formal surface requirement=session.lifecycle.session_state
// @formal reason: One composition selects every required Session role once; accessors cannot replace a
// @formal reason: selected role or introduce separate durable truth, and the projection is derived from the
// @formal reason: selected read roles. Construction and caller checks remain executable evidence, not a
// @formal reason: proof of external effects.
//! Immutable required executor roles selected at Session application construction.
//!
//! Each accessor exposes one caller role. A composition selects the roles once,
//! either from one complete runtime or from the executor components that each
//! own one role; the binding owns no execution state or durable truth.

use crate::{
    BudgetRuntime, CapabilityRuntime, ColocatedRealizationRoles, CommittedSessionProjection,
    CompleteSessionRuntime, CoordinationRuntime, LiveInboxRuntime, OutcomeRuntime,
    SessionCoordinationReads, SessionPlacementRoles, SessionProjectionSource, SessionRuntime,
    StreamingRunRuntime, ThreadReadRuntime,
};
use std::sync::Arc;

/// Required Session executor roles, selected once by the composition. A
/// selected role cannot be replaced afterwards, and the committed projection is
/// derived from the selected Thread and coordination read roles rather than
/// selected separately, so it can never read through a third owner.
///
/// A core-only Runtime cannot satisfy application composition:
///
/// ```compile_fail,E0277
/// use std::sync::Arc;
/// use awaken_session_execution_contract::{NeutralRuntime, SessionExecutor, SessionRuntime};
///
/// fn cannot_bind_partial<T: SessionRuntime + 'static>(runtime: Arc<T>) {
///     let _ = SessionExecutor::colocated(runtime, Arc::new(NeutralRuntime));
/// }
/// ```
///
/// A selected role cannot be replaced with an unrelated owner:
///
/// ```compile_fail,E0594
/// use std::sync::Arc;
/// use awaken_session_execution_contract::{BudgetRuntime, SessionExecutor};
/// fn cannot_replace(executor: &mut SessionExecutor, other: Arc<dyn BudgetRuntime>) {
///     executor.budget = other;
/// }
/// ```
#[derive(Clone)]
pub struct SessionExecutor {
    roles: SessionExecutionRoles,
    projection: Arc<CommittedSessionProjection>,
}

/// Required capabilities selected by one composition. The containing
/// `SessionExecutor` exposes this view immutably; roles cannot be replaced.
#[derive(Clone)]
pub struct SessionExecutionRoles {
    pub core: Arc<dyn SessionRuntime>,
    /// Where the Sessions are realized: a co-located Worker's realization
    /// roles, or the frozen-input admission of Worker-placed Sessions.
    pub placement: SessionPlacementRoles,
    pub coordination: Arc<dyn CoordinationRuntime>,
    pub coordination_reads: Arc<dyn SessionCoordinationReads>,
    pub budget: Arc<dyn BudgetRuntime>,
    pub streaming_runs: Arc<dyn StreamingRunRuntime>,
    pub thread_reads: Arc<dyn ThreadReadRuntime>,
    pub outcomes: Arc<dyn OutcomeRuntime>,
    pub live_inbox: Arc<dyn LiveInboxRuntime>,
    pub capability: Arc<dyn CapabilityRuntime>,
}

impl std::ops::Deref for SessionExecutor {
    type Target = SessionExecutionRoles;
    fn deref(&self) -> &Self::Target {
        &self.roles
    }
}

impl SessionExecutor {
    /// Committed projection facts without execution authority.
    pub fn projection(&self) -> &dyn SessionProjectionSource {
        self.projection.as_ref()
    }

    /// Share this same selected read projection without exposing execution roles.
    pub fn projection_handle(&self) -> Arc<dyn SessionProjectionSource> {
        self.projection.clone()
    }

    /// The required `SessionRuntime` selected at construction.
    pub fn core(&self) -> &dyn SessionRuntime {
        self.core.as_ref()
    }

    /// Where this process's Sessions are realized.
    pub fn placement(&self) -> &SessionPlacementRoles {
        &self.placement
    }

    /// The required `CoordinationRuntime` selected at construction.
    pub fn coordination(&self) -> &dyn CoordinationRuntime {
        self.coordination.as_ref()
    }

    /// The required `SessionCoordinationReads` selected at construction.
    pub fn coordination_reads(&self) -> &dyn SessionCoordinationReads {
        self.coordination_reads.as_ref()
    }

    /// The required `BudgetRuntime` selected at construction.
    pub fn budget(&self) -> &dyn BudgetRuntime {
        self.budget.as_ref()
    }

    /// The required `StreamingRunRuntime` selected at construction.
    pub fn streaming_runs(&self) -> &dyn StreamingRunRuntime {
        self.streaming_runs.as_ref()
    }

    /// The required `ThreadReadRuntime` selected at construction.
    pub fn thread_reads(&self) -> &dyn ThreadReadRuntime {
        self.thread_reads.as_ref()
    }

    /// The required `OutcomeRuntime` selected at construction.
    pub fn outcomes(&self) -> &dyn OutcomeRuntime {
        self.outcomes.as_ref()
    }

    /// The required `LiveInboxRuntime` selected at construction.
    pub fn live_inbox(&self) -> &dyn LiveInboxRuntime {
        self.live_inbox.as_ref()
    }

    /// The required `CapabilityRuntime` selected at construction.
    pub fn capability(&self) -> &dyn CapabilityRuntime {
        self.capability.as_ref()
    }
}

impl From<SessionExecutionRoles> for SessionExecutor {
    fn from(roles: SessionExecutionRoles) -> Self {
        let projection = Arc::new(CommittedSessionProjection::new(
            roles.thread_reads.clone(),
            roles.coordination_reads.clone(),
        ));
        Self { roles, projection }
    }
}

impl SessionExecutor {
    /// Bind a complete co-located runtime and its exact MCP effect owner once.
    /// Worker-only compositions supply `SessionExecutionRoles` directly and
    /// cannot acquire this local effect capability through a placeholder.
    pub fn colocated(
        runtime: Arc<dyn CompleteSessionRuntime>,
        mcp: Arc<dyn crate::McpAttachmentRealizer>,
    ) -> Self {
        Self::from(SessionExecutionRoles {
            core: runtime.clone(),
            placement: SessionPlacementRoles::Colocated(ColocatedRealizationRoles {
                mcp,
                realization: runtime.clone(),
                realization_effects: runtime.clone(),
                environment: runtime.clone(),
                terminal_cleanup: runtime.clone(),
            }),
            coordination: runtime.clone(),
            coordination_reads: runtime.clone(),
            budget: runtime.clone(),
            streaming_runs: runtime.clone(),
            thread_reads: runtime.clone(),
            outcomes: runtime.clone(),
            live_inbox: runtime.clone(),
            capability: runtime.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_roles_and_clones_keep_one_runtime_owner() {
        // Every command/read role must refer to the selected owner, including
        // after cloning the executor. Mixing owners could join unrelated
        // projection cursors and execution fences despite valid individual ports.
        let runtime = Arc::new(crate::NeutralRuntime);
        let owner = Arc::as_ptr(&runtime).cast::<()>();
        // C1 the caller selects an independent MCP effect owner; C2 the
        // executor is cloned. Both views retain C1 exactly, while all runtime
        // roles retain their original owner. No optional-role discovery or
        // implicit MCP fallback may replace either selected collaborator.
        let mcp: Arc<dyn crate::McpAttachmentRealizer> = Arc::new(crate::NeutralRuntime);
        let selected = SessionExecutor::colocated(runtime, mcp.clone());
        for executor in [&selected, &selected.clone()] {
            let colocated = executor
                .placement
                .colocated()
                .expect("a complete runtime realizes its Sessions");
            assert!(Arc::ptr_eq(&colocated.mcp, &mcp));
            for role in [
                Arc::as_ptr(&executor.core).cast::<()>(),
                Arc::as_ptr(&colocated.realization).cast::<()>(),
                Arc::as_ptr(&colocated.realization_effects).cast::<()>(),
                Arc::as_ptr(&executor.coordination).cast::<()>(),
                Arc::as_ptr(&executor.coordination_reads).cast::<()>(),
                Arc::as_ptr(&executor.budget).cast::<()>(),
                Arc::as_ptr(&executor.streaming_runs).cast::<()>(),
                Arc::as_ptr(&colocated.environment).cast::<()>(),
                Arc::as_ptr(&executor.thread_reads).cast::<()>(),
                Arc::as_ptr(&colocated.terminal_cleanup).cast::<()>(),
                Arc::as_ptr(&executor.outcomes).cast::<()>(),
                Arc::as_ptr(&executor.live_inbox).cast::<()>(),
                Arc::as_ptr(&executor.capability).cast::<()>(),
            ] {
                assert_eq!(role, owner);
            }
        }
    }

    /// A Thread read role that names itself in every failure, so a projection
    /// read proves which selected role served it.
    struct NamedThreadReads;

    #[async_trait::async_trait]
    impl ThreadReadRuntime for NamedThreadReads {
        async fn owns_thread(&self, _thread: &str) -> Result<bool, crate::RunError> {
            Ok(false)
        }
        async fn committed_messages(
            &self,
            _thread: &str,
        ) -> Result<Vec<awaken_agent_contract::agent::message::Message>, crate::RunError> {
            Ok(Vec::new())
        }
        async fn session_thread_recovery_snapshot(
            &self,
            _session_id: &crate::SessionId,
            _thread_id: &str,
        ) -> Result<
            Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
            crate::RunError,
        > {
            Err(crate::RunError::bad_request("thread-reads latest"))
        }
        async fn session_thread_run_recovery_snapshot(
            &self,
            _session_id: &crate::SessionId,
            _thread_id: &str,
            _run_id: &awaken_agent_contract::agent::run::Id,
        ) -> Result<
            Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
            crate::RunError,
        > {
            Err(crate::RunError::bad_request("thread-reads exact"))
        }
        async fn session_thread_disposition(
            &self,
            _session_id: &crate::SessionId,
            _thread_id: &str,
        ) -> Result<awaken_agent_contract::ThreadDisposition, crate::RunError> {
            Ok(awaken_agent_contract::ThreadDisposition::Active)
        }
        async fn committed_outcome_projection(
            &self,
            _thread: &str,
            _outcome_id: &str,
        ) -> Result<Option<crate::CommittedOutcomeProjection>, crate::RunError> {
            Err(crate::RunError::bad_request("thread-reads outcome"))
        }
    }

    fn message<T>(result: Result<T, crate::RunError>) -> String {
        result.err().expect("the named role refuses").message
    }

    #[tokio::test]
    async fn projection_is_derived_from_the_selected_read_roles() {
        // Decision table: cause = the projection read; effect = the selected
        // role that serves it. Thread prefixes and Outcomes are Thread reads;
        // coordinated links are coordination reads. No other role or owner is
        // reachable through the projection. P1-P4 run through both borrowed and
        // shared views and cloned executors; all retain the exact selected roles.
        // | Rule | projection read            | served by          |
        // | P1   | latest Thread prefix       | thread_reads       |
        // | P2   | exact-Run Thread prefix    | thread_reads       |
        // | P3   | terminal Outcome           | thread_reads       |
        // | P4   | coordinated Thread links   | coordination_reads |
        let neutral = Arc::new(crate::NeutralRuntime);
        let selected = SessionExecutor::from(SessionExecutionRoles {
            core: neutral.clone(),
            placement: SessionPlacementRoles::Colocated(ColocatedRealizationRoles {
                mcp: neutral.clone(),
                realization: neutral.clone(),
                realization_effects: neutral.clone(),
                environment: neutral.clone(),
                terminal_cleanup: neutral.clone(),
            }),
            coordination: neutral.clone(),
            coordination_reads: neutral.clone(),
            budget: neutral.clone(),
            streaming_runs: neutral.clone(),
            thread_reads: Arc::new(NamedThreadReads),
            outcomes: neutral.clone(),
            live_inbox: neutral.clone(),
            capability: neutral,
        });
        let session = crate::SessionId::from("session");
        let run = awaken_agent_contract::agent::run::Id("run".into());
        for executor in [&selected, &selected.clone()] {
            let shared = executor.projection_handle();
            let shared_again = executor.projection_handle();
            assert!(
                Arc::ptr_eq(&shared, &shared_again),
                "the shared view is the same projection"
            );
            for projection in [executor.projection(), shared.as_ref()] {
                assert_eq!(
                    message(
                        projection
                            .projection_thread_snapshot(&session, "session", None)
                            .await
                    ),
                    "thread-reads latest",
                    "P1"
                );
                assert_eq!(
                    message(
                        projection
                            .projection_thread_snapshot(&session, "session", Some(&run))
                            .await
                    ),
                    "thread-reads exact",
                    "P2"
                );
                assert_eq!(
                    message(projection.projection_outcome("session", "outcome").await),
                    "thread-reads outcome",
                    "P3"
                );
                assert_eq!(
                    projection
                        .projection_coordinated_threads(&session, None)
                        .await
                        .unwrap(),
                    executor
                        .coordination_reads()
                        .coordinated_threads(&session, None)
                        .await
                        .unwrap(),
                    "P4"
                );
            }
        }
    }
}
