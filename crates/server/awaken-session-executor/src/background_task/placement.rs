// @formal entrypoint background_task.terminal_observer_reads_authoritative_committed_state_not_process_projection
//! Where a BackgroundTask holder reads committed truth and sends its
//! observations: the co-located Session application, or a registered Worker's
//! authenticated transport (rows R1-R6 and W1-W5 of the remote Task control
//! design).

use std::sync::Arc;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::state::Command as StateCommand;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_session_application_contract::{
    SessionRunBackgroundApplication, TaskAttention, TaskAttentionAck, TaskAttentionDelivery,
};
use awaken_session_execution_contract::SessionApplicationBinding;
use awaken_session_run_delivery::CommittedAuthority;

/// The committed reads a Task observer decides from.
#[derive(Clone)]
pub(crate) enum TaskCommittedReads {
    /// R1/H1: committed local authority.
    Local(CommittedAuthority<Arc<dyn crate::LocalCommit>>),
    /// R2-R5: the Coordinator's committed truth under the claim that is
    /// delivering the observed Run's terminal on this Worker.
    Claimed(awaken_dispatch_execution::ClaimedTerminalReads),
}

impl From<CommittedAuthority<Arc<dyn crate::LocalCommit>>> for TaskCommittedReads {
    fn from(authority: CommittedAuthority<Arc<dyn crate::LocalCommit>>) -> Self {
        Self::Local(authority)
    }
}

impl TaskCommittedReads {
    /// The reads for one Session's commit boundary: its local authority (R1),
    /// else the claim-bound reads it shares with the Dispatch Worker that
    /// delivers its terminals (R2-R5). Only a database-less Worker's Dispatch
    /// Worker records delivering claims in them.
    pub(crate) fn of(
        commit: &awaken_session_run_delivery::SessionCommit,
        claimed: &awaken_dispatch_execution::ClaimedTerminalReads,
    ) -> Self {
        match commit.committed_authority() {
            Some(authority) => Self::Local(authority),
            None => Self::Claimed(claimed.clone()),
        }
    }

    /// Committed State of `thread_id` while the terminal of `run_id` is
    /// observed. A local authority answers at any time; a Worker answers only
    /// under that Run's delivering claim.
    pub(crate) async fn committed_state(
        &self,
        thread_id: &ThreadId,
        run_id: &RunId,
    ) -> Result<Vec<StateCommand>, String> {
        match self {
            Self::Local(authority) => authority.committed_state(thread_id).await,
            Self::Claimed(reads) => reads.committed_state(thread_id, run_id).await,
        }
    }

    /// Committed State of `thread_id` for a launched attempt's ownership check,
    /// which runs after the observed Run's claim has settled.
    ///
    /// | Rule | reads | effect |
    /// |---|---|---|
    /// | H1 | local authority | its committed State |
    /// | H2-H5 | a Worker's claim-bound reads | refused: a settled claim authorizes no read, and the holder-bound Task tenure read is not available |
    pub(crate) async fn tenure_state(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<StateCommand>, String> {
        match self {
            Self::Local(authority) => authority.committed_state(thread_id).await,
            Self::Claimed(_) => Err(
                "a registered Worker has no holder-bound Task tenure read; the attempt cannot prove it is current"
                    .to_owned(),
            ),
        }
    }
}

/// The route a Run's Task observer publishes attention through.
#[derive(Clone)]
pub(crate) enum TaskAttentionRoute {
    /// W1: the co-located Session application, through the one late binding.
    CoLocated(SessionApplicationBinding),
    /// W3: a registered Worker's authenticated Task attention route.
    Registered(Arc<dyn TaskAttentionDelivery>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TaskAttentionPlacement {
    CoLocated,
    Registered,
}

/// Decision table (W1-W4):
/// | Rule | hosts the Session application | background application bound | registered delivery | route |
/// |---|---|---|---|---|
/// | W1 | yes | yes | any | co-located |
/// | W2 | yes | no | any | none |
/// | W3 | no | any | yes | registered |
/// | W4 | no | any | no | none |
///
/// W5 (a co-located host given a registered delivery) is refused when the
/// Host is constructed, so it never reaches this selection.
#[must_use]
const fn select_task_attention_placement(
    hosts_application: bool,
    application_bound: bool,
    registered: bool,
) -> Option<TaskAttentionPlacement> {
    match (hosts_application, application_bound, registered) {
        (true, true, _) => Some(TaskAttentionPlacement::CoLocated),
        (false, _, true) => Some(TaskAttentionPlacement::Registered),
        (true, false, _) | (false, _, false) => None,
    }
}

/// One resolved attention target for a publication and its retries.
pub(super) enum TaskAttentionTarget {
    Application(Arc<dyn SessionRunBackgroundApplication>),
    Registered(Arc<dyn TaskAttentionDelivery>),
}

impl TaskAttentionTarget {
    pub(super) async fn deliver(
        &self,
        attention: TaskAttention,
    ) -> Result<TaskAttentionAck, awaken_session_types::RunError> {
        match self {
            Self::Application(application) => application.submit_task_attention(attention).await,
            Self::Registered(delivery) => delivery.deliver(attention).await,
        }
    }
}

impl TaskAttentionRoute {
    /// Select this process's route (W1-W4).
    pub(crate) fn select(
        hosts_application: bool,
        application: &SessionApplicationBinding,
        registered: Option<&Arc<dyn TaskAttentionDelivery>>,
    ) -> Option<Self> {
        match select_task_attention_placement(
            hosts_application,
            application.background_runs().is_ok(),
            registered.is_some(),
        )? {
            TaskAttentionPlacement::CoLocated => Some(Self::CoLocated(application.clone())),
            TaskAttentionPlacement::Registered => registered.cloned().map(Self::Registered),
        }
    }

    /// The plan's decision for a Run whose snapshot enables BackgroundTask.
    ///
    /// | Rule | placement | effect |
    /// |---|---|---|
    /// | W1/W2 | co-located | the selected route (W2: none) |
    /// | W4 | registered Worker without a delivery | refused |
    /// | W3 | registered Worker with a delivery | refused until the Worker can read its Task tenure (H2-H5) |
    pub(crate) fn plan(
        enabled: bool,
        hosts_application: bool,
        application: &SessionApplicationBinding,
        registered: Option<&Arc<dyn TaskAttentionDelivery>>,
    ) -> Result<Option<Self>, &'static str> {
        if !enabled {
            return Ok(None);
        }
        if !hosts_application {
            return Err(match registered {
                None => {
                    "background_task requires a co-located Session application or a registered Worker's Task attention route"
                }
                Some(_) => {
                    "background_task requires a co-located Session application until a registered Worker can read its Task tenure from the Coordinator"
                }
            });
        }
        Ok(Self::select(hosts_application, application, registered))
    }

    /// The target one publication retries against. A co-located application
    /// that is gone (process shutdown) has no target.
    pub(super) fn target(&self) -> Option<TaskAttentionTarget> {
        match self {
            Self::CoLocated(binding) => binding
                .background_runs()
                .ok()
                .map(TaskAttentionTarget::Application),
            Self::Registered(delivery) => Some(TaskAttentionTarget::Registered(delivery.clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TaskAttentionPlacement::{CoLocated, Registered};

    struct UnusedDelivery;

    #[async_trait::async_trait]
    impl TaskAttentionDelivery for UnusedDelivery {
        fn holder(&self) -> &str {
            "registered-holder"
        }

        async fn deliver(
            &self,
            _attention: TaskAttention,
        ) -> Result<TaskAttentionAck, awaken_session_types::RunError> {
            panic!("route selection never delivers")
        }
    }

    #[test]
    fn the_attention_route_follows_the_placement() {
        // (hosts the application, background application bound, registered
        // delivery) -> route; every combination of the three causes.
        let rows = [
            ("W1", true, true, true, Some(CoLocated)),
            ("W1", true, true, false, Some(CoLocated)),
            ("W2", true, false, true, None),
            ("W2", true, false, false, None),
            ("W3", false, true, true, Some(Registered)),
            ("W3", false, false, true, Some(Registered)),
            ("W4", false, true, false, None),
            ("W4", false, false, false, None),
        ];
        for (rule, hosts, bound, registered, expected) in rows {
            assert_eq!(
                select_task_attention_placement(hosts, bound, registered),
                expected,
                "{rule}"
            );
        }
    }

    #[test]
    fn route_selection_reads_the_bound_application_and_the_installed_delivery() {
        let application: Arc<dyn SessionRunBackgroundApplication> =
            crate::test_application_bindings::fresh();
        let bound = crate::test_application_bindings::background_binding(application);
        let unbound = SessionApplicationBinding::default();
        let delivery: Arc<dyn TaskAttentionDelivery> = Arc::new(UnusedDelivery);
        assert!(
            matches!(
                TaskAttentionRoute::select(true, &bound, None),
                Some(TaskAttentionRoute::CoLocated(_))
            ),
            "W1"
        );
        assert!(
            TaskAttentionRoute::select(true, &unbound, None).is_none(),
            "W2"
        );
        assert!(
            matches!(
                TaskAttentionRoute::select(false, &unbound, Some(&delivery)),
                Some(TaskAttentionRoute::Registered(_))
            ),
            "W3"
        );
        assert!(
            TaskAttentionRoute::select(false, &bound, None).is_none(),
            "W4"
        );
    }

    #[test]
    fn the_plan_refuses_background_tasks_on_a_registered_worker_until_tenure_reads_exist() {
        let application: Arc<dyn SessionRunBackgroundApplication> =
            crate::test_application_bindings::fresh();
        let bound = crate::test_application_bindings::background_binding(application);
        let delivery: Arc<dyn TaskAttentionDelivery> = Arc::new(UnusedDelivery);
        for registered in [None, Some(&delivery)] {
            assert!(
                matches!(
                    TaskAttentionRoute::plan(false, false, &bound, registered),
                    Ok(None)
                ),
                "disabled"
            );
        }
        assert!(
            matches!(
                TaskAttentionRoute::plan(true, true, &bound, None),
                Ok(Some(TaskAttentionRoute::CoLocated(_)))
            ),
            "W1"
        );
        let refused = TaskAttentionRoute::plan(true, false, &bound, None)
            .err()
            .expect("W4");
        assert!(refused.contains("co-located Session application"), "W4");
        let refused = TaskAttentionRoute::plan(true, false, &bound, Some(&delivery))
            .err()
            .expect("W3 waits for H2-H5");
        assert!(refused.contains("Task tenure"), "W3");
    }

    #[test]
    fn a_worker_commit_boundary_reads_under_claims_never_from_its_projection() {
        let claimed = awaken_dispatch_execution::ClaimedTerminalReads::default();
        let remote = awaken_session_run_delivery::SessionCommit::Remote(
            awaken_session_run_delivery::WorkerRecoveryCommit::new(),
        );
        assert!(
            matches!(
                TaskCommittedReads::of(&remote, &claimed),
                TaskCommittedReads::Claimed(_)
            ),
            "R2-R6"
        );
        let local = awaken_session_run_delivery::SessionCommit::Local(Arc::new(
            crate::LocalCommitAdapter::projected(awaken_store_inmem::MemoryCommitCoordinator::new(
                awaken_time::system_clock(),
            )),
        ));
        assert!(
            matches!(
                TaskCommittedReads::of(&local, &claimed),
                TaskCommittedReads::Local(_)
            ),
            "R1"
        );
    }

    #[tokio::test]
    async fn worker_reads_refuse_outside_a_delivering_claim_and_after_settlement() {
        let reads = TaskCommittedReads::Claimed(Default::default());
        let thread = ThreadId("thread".into());
        assert!(
            reads
                .committed_state(&thread, &RunId("run".into()))
                .await
                .is_err(),
            "R3 no delivering claim"
        );
        assert!(
            reads.tenure_state(&thread).await.is_err(),
            "H2-H5 not available"
        );
    }
}
