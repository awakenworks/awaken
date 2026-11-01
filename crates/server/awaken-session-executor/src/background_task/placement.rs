// @formal entrypoint background_task.terminal_observer_reads_authoritative_committed_state_not_process_projection
//! Where a BackgroundTask holder reads committed truth and sends its
//! observations: the co-located Session application, or a registered Worker's
//! authenticated transport (rows R1-R6, H1-H5 and W1-W5 of the remote Task
//! control design).

use std::sync::Arc;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::state::{Command as StateCommand, Store};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_ext_background_task::{BackgroundTask, task_state_cell};
use awaken_session_application_contract::{
    RegisteredTaskControl, SessionRunBackgroundApplication, TaskAttention, TaskAttentionAck,
    TaskReadPartition, TaskTenureRead,
};
use awaken_session_execution_contract::SessionApplicationBinding;
use awaken_session_run_delivery::CommittedAuthority;

/// The committed reads a Task observer decides from.
#[derive(Clone)]
pub(crate) enum TaskCommittedReads {
    /// R1/H1: committed local authority.
    Local(CommittedAuthority<Arc<dyn crate::LocalCommit>>),
    /// A registered Worker: R2-R5, the Coordinator's committed truth under the
    /// claim that is delivering the observed Run's terminal; H2-H5, a held
    /// Task's committed slot read with this incarnation's holder.
    Registered {
        terminal: awaken_dispatch_execution::ClaimedTerminalReads,
        control: Arc<dyn RegisteredTaskControl>,
        /// The Session Thread whose partition this Session's Threads are
        /// committed in.
        session_thread: String,
    },
}

impl From<CommittedAuthority<Arc<dyn crate::LocalCommit>>> for TaskCommittedReads {
    fn from(authority: CommittedAuthority<Arc<dyn crate::LocalCommit>>) -> Self {
        Self::Local(authority)
    }
}

impl TaskCommittedReads {
    /// The reads for one Session's commit boundary: its local authority (R1,
    /// H1), else the claim-bound reads it shares with the Dispatch Worker that
    /// delivers its terminals (R2-R5) and this incarnation's Task control
    /// (H2-H5). Only a database-less Worker's Dispatch Worker records
    /// delivering claims in them. A Worker without Task control has no
    /// committed read for a Task (W4). `session_thread` is the Session Thread
    /// these reads belong to.
    pub(crate) fn of(
        commit: &awaken_session_run_delivery::SessionCommit,
        claimed: &awaken_dispatch_execution::ClaimedTerminalReads,
        control: Option<&Arc<dyn RegisteredTaskControl>>,
        session_thread: &str,
    ) -> Result<Self, &'static str> {
        match (commit.committed_authority(), control) {
            (Some(authority), _) => Ok(Self::Local(authority)),
            (None, Some(control)) => Ok(Self::Registered {
                terminal: claimed.clone(),
                control: control.clone(),
                session_thread: session_thread.to_owned(),
            }),
            (None, None) => Err(
                "BackgroundTask on a registered Worker requires its Task control for committed reads",
            ),
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
            Self::Registered { terminal, .. } => terminal.committed_state(thread_id, run_id).await,
        }
    }

    /// The committed slot of `expected` for a launched attempt's ownership
    /// check, which runs after the observed Run's claim has settled.
    ///
    /// | Rule | reads | effect |
    /// |---|---|---|
    /// | H1 | local authority | the slot in its committed State |
    /// | H2 | registered Worker; the Coordinator returns the held slot | that slot, decoded by the Task codec |
    /// | H3 | registered Worker; no committed attempt names this holder | `None`: the attempt is lost |
    /// | H4/H5 | the read is refused or unavailable, or the slot does not decode | refused: the attempt cannot prove it is current |
    pub(crate) async fn committed_task(
        &self,
        expected: &BackgroundTask,
    ) -> Result<Option<BackgroundTask>, String> {
        match self {
            Self::Local(authority) => {
                let state = Store::rebuild(
                    &authority
                        .committed_state(&expected.origin.thread_id)
                        .await?,
                );
                task_state_cell(&expected.id)
                    .load(&state)
                    .map_err(|error| error.to_string())
            }
            Self::Registered {
                control,
                session_thread,
                ..
            } => {
                // P1/P2: the read addresses the partition the Task's Thread
                // is committed in, as the Run's own committed reads do.
                let read = task_tenure_read(expected, session_thread);
                if read.holder != control.holder() {
                    return Err("Task attempt belongs to another Worker holder".into());
                }
                let Some(record) = control
                    .read_tenure(read)
                    .await
                    .map_err(|error| format!("Task tenure read: {error}"))?
                else {
                    return Ok(None);
                };
                serde_json::from_value(record.slot().clone())
                    .map(Some)
                    .map_err(|error| {
                        format!("committed BackgroundTask slot does not decode: {error}")
                    })
            }
        }
    }
}

/// The same exact coordinate projection is used by effect ownership reads
/// and attention; it grants no authority without the Coordinator's check.
pub(super) fn task_tenure_read(expected: &BackgroundTask, session_thread: &str) -> TaskTenureRead {
    let tenure = expected.tenure().expect("a launched Task has an attempt");
    TaskTenureRead {
        thread: expected.origin.thread_id.0.clone(),
        partition: TaskReadPartition::of(&expected.origin.thread_id.0, session_thread),
        run: expected.origin.run_id.0.clone(),
        task: expected.id.as_str().to_owned(),
        holder: tenure.holder().clone(),
        epoch: tenure.epoch(),
    }
}

/// The route a Run's Task observer publishes attention through.
#[derive(Clone)]
pub(crate) enum TaskAttentionRoute {
    /// W1: the co-located Session application, through the one late binding.
    CoLocated(SessionApplicationBinding),
    /// W3: a registered Worker's authenticated Task attention route.
    Registered(Arc<dyn RegisteredTaskControl>),
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
    Registered(Arc<dyn RegisteredTaskControl>),
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
        registered: Option<&Arc<dyn RegisteredTaskControl>>,
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
    /// | W3 | registered Worker with its Task control | the registered route |
    /// | W4 | registered Worker without Task control | refused |
    pub(crate) fn plan(
        enabled: bool,
        hosts_application: bool,
        application: &SessionApplicationBinding,
        registered: Option<&Arc<dyn RegisteredTaskControl>>,
    ) -> Result<Option<Self>, &'static str> {
        if !enabled {
            return Ok(None);
        }
        if !hosts_application && registered.is_none() {
            return Err(
                "background_task requires a co-located Session application or a registered Worker's Task control",
            );
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
    impl RegisteredTaskControl for UnusedDelivery {
        fn holder(&self) -> &str {
            "registered-holder"
        }

        async fn deliver(
            &self,
            _attention: TaskAttention,
        ) -> Result<TaskAttentionAck, awaken_session_types::RunError> {
            panic!("route selection never delivers")
        }

        async fn read_tenure(
            &self,
            _read: TaskTenureRead,
        ) -> Result<
            Option<awaken_session_application_contract::TaskTenureRecord>,
            awaken_session_types::RunError,
        > {
            panic!("route selection never reads tenure")
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
        // W1 needs a live application: this binding borrows it weakly;
        // retain the original strong owner through every route assertion.
        let bound = crate::test_application_bindings::background_binding(application.clone());
        let unbound = SessionApplicationBinding::default();
        let delivery: Arc<dyn RegisteredTaskControl> = Arc::new(UnusedDelivery);
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
    fn the_plan_admits_background_tasks_where_a_route_and_committed_reads_exist() {
        let application: Arc<dyn SessionRunBackgroundApplication> =
            crate::test_application_bindings::fresh();
        // W1 needs a live application: this binding borrows it weakly;
        // retain the original strong owner through every route assertion.
        let bound = crate::test_application_bindings::background_binding(application.clone());
        let unbound = SessionApplicationBinding::default();
        let delivery: Arc<dyn RegisteredTaskControl> = Arc::new(UnusedDelivery);
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
        assert!(
            matches!(
                TaskAttentionRoute::plan(true, true, &unbound, None),
                Ok(None)
            ),
            "W2"
        );
        assert!(
            matches!(
                TaskAttentionRoute::plan(true, false, &unbound, Some(&delivery)),
                Ok(Some(TaskAttentionRoute::Registered(_)))
            ),
            "W3"
        );
        let refused = TaskAttentionRoute::plan(true, false, &bound, None)
            .err()
            .expect("W4");
        assert!(refused.contains("co-located Session application"), "W4");
    }

    #[test]
    fn a_worker_commit_boundary_reads_under_claims_never_from_its_projection() {
        let claimed = awaken_dispatch_execution::ClaimedTerminalReads::default();
        let control: Arc<dyn RegisteredTaskControl> = Arc::new(UnusedDelivery);
        let remote = awaken_session_run_delivery::SessionCommit::Remote(
            awaken_session_run_delivery::WorkerRecoveryCommit::new(),
        );
        assert!(
            matches!(
                TaskCommittedReads::of(&remote, &claimed, Some(&control), "session"),
                Ok(TaskCommittedReads::Registered { .. })
            ),
            "R2-R6 and H2-H5"
        );
        assert!(
            TaskCommittedReads::of(&remote, &claimed, None, "session").is_err(),
            "W4 no committed read without Task control"
        );
        let local = awaken_session_run_delivery::SessionCommit::Local(Arc::new(
            crate::LocalCommitAdapter::projected(awaken_store_inmem::MemoryCommitCoordinator::new(
                awaken_time::system_clock(),
            )),
        ));
        for control in [None, Some(&control)] {
            assert!(
                matches!(
                    TaskCommittedReads::of(&local, &claimed, control, "session"),
                    Ok(TaskCommittedReads::Local(_))
                ),
                "R1/H1"
            );
        }
    }

    #[tokio::test]
    async fn worker_reads_refuse_outside_a_delivering_claim() {
        let reads = TaskCommittedReads::Registered {
            terminal: Default::default(),
            control: Arc::new(UnusedDelivery),
            session_thread: "thread".into(),
        };
        assert!(
            reads
                .committed_state(&ThreadId("thread".into()), &RunId("run".into()))
                .await
                .is_err(),
            "R3 no delivering claim"
        );
    }

    /// A registered incarnation whose Coordinator answers one scripted tenure.
    struct ScriptedTaskControl {
        answer: Result<Option<serde_json::Value>, awaken_session_types::RunErrorKind>,
        reads: std::sync::Mutex<Vec<TaskTenureRead>>,
    }

    #[async_trait::async_trait]
    impl RegisteredTaskControl for ScriptedTaskControl {
        fn holder(&self) -> &str {
            "registered-holder"
        }

        async fn deliver(
            &self,
            _attention: TaskAttention,
        ) -> Result<TaskAttentionAck, awaken_session_types::RunError> {
            panic!("tenure reads never deliver")
        }

        async fn read_tenure(
            &self,
            read: TaskTenureRead,
        ) -> Result<
            Option<awaken_session_application_contract::TaskTenureRecord>,
            awaken_session_types::RunError,
        > {
            self.reads
                .lock()
                .expect("tenure reads mutex poisoned")
                .push(read);
            match &self.answer {
                Ok(slot) => Ok(slot
                    .clone()
                    .map(awaken_session_application_contract::TaskTenureRecord::new)),
                Err(awaken_session_types::RunErrorKind::BadRequest) => Err(
                    awaken_session_types::RunError::bad_request("not this incarnation's holder"),
                ),
                Err(_) => Err(awaken_session_types::RunError::unavailable(
                    "Coordinator unavailable",
                )),
            }
        }
    }

    fn held_task(thread: &str) -> BackgroundTask {
        let mut task = BackgroundTask::requested(
            awaken_tool_contract::tool::background_task::BackgroundTaskId::new("held-task")
                .expect("task id"),
            awaken_ext_background_task::BackgroundTaskOrigin {
                thread_id: ThreadId(thread.into()),
                run_id: RunId("origin-run".into()),
                operation_id: "operation".into(),
            },
            awaken_ext_background_task::BackgroundInvocation {
                call: awaken_tool_contract::tool::ToolCall {
                    call_id: "call".into(),
                    tool_id: "tool".into(),
                    arguments: serde_json::json!({}),
                },
            },
        );
        task.start(
            "registered-holder",
            0,
            10,
            awaken_ext_background_task::TaskExecutionPolicy {
                recovery: awaken_tool_contract::tool::ToolRecoveryPolicy::replay_safe(),
                concurrency: awaken_tool_contract::tool::ToolConcurrency::Parallel,
            },
        )
        .expect("start");
        task
    }

    #[tokio::test]
    async fn a_registered_worker_reads_a_held_task_with_its_own_holder() {
        let expected = held_task("thread");
        let slot = serde_json::to_value(&expected).expect("slot");
        let rows = [
            ("H2", Ok(Some(slot)), Ok(Some(expected.clone()))),
            ("H3", Ok(None), Ok(None)),
            (
                "H4",
                Err(awaken_session_types::RunErrorKind::BadRequest),
                Err(()),
            ),
            (
                "H5",
                Err(awaken_session_types::RunErrorKind::Unavailable),
                Err(()),
            ),
            (
                "H5 undecodable",
                Ok(Some(serde_json::json!("not a task"))),
                Err(()),
            ),
        ];
        for (rule, answer, effect) in rows {
            let control = Arc::new(ScriptedTaskControl {
                answer,
                reads: Default::default(),
            });
            let reads = TaskCommittedReads::Registered {
                terminal: Default::default(),
                control: control.clone(),
                session_thread: "thread".into(),
            };
            let read = reads.committed_task(&expected).await;
            assert_eq!(read.map_err(|_| ()), effect, "{rule}");
            assert_eq!(
                control
                    .reads
                    .lock()
                    .expect("tenure reads mutex poisoned")
                    .as_slice(),
                &[TaskTenureRead {
                    thread: "thread".into(),
                    partition: TaskReadPartition::Thread {},
                    run: "origin-run".into(),
                    task: "held-task".into(),
                    holder: "registered-holder".into(),
                    epoch: awaken_lease_contract::fence::Epoch::from_persisted(1)
                        .expect("nonzero Task epoch"),
                }],
                "{rule} one read with this incarnation's holder"
            );
        }
    }

    #[tokio::test]
    async fn a_task_on_a_child_thread_is_read_in_its_session_partition() {
        // P1: a Task on the Session's own Thread reads that Thread as its own
        // partition; P2: a Task on a child Thread reads the child inside the
        // Session's partition.
        for (rule, thread, partition) in [
            ("P1", "session", TaskReadPartition::Thread {}),
            (
                "P2",
                "child",
                TaskReadPartition::Session {
                    session_thread: "session".into(),
                },
            ),
        ] {
            let expected = held_task(thread);
            let control = Arc::new(ScriptedTaskControl {
                answer: Ok(Some(serde_json::to_value(&expected).expect("slot"))),
                reads: Default::default(),
            });
            let reads = TaskCommittedReads::Registered {
                terminal: Default::default(),
                control: control.clone(),
                session_thread: "session".into(),
            };
            assert_eq!(
                reads.committed_task(&expected).await,
                Ok(Some(expected.clone())),
                "{rule}"
            );
            let sent = control
                .reads
                .lock()
                .expect("tenure reads mutex poisoned")
                .clone();
            assert_eq!(sent.len(), 1, "{rule}");
            assert_eq!(sent[0].thread, thread, "{rule}");
            assert_eq!(sent[0].partition, partition, "{rule}");
        }
    }
}
