//! Shared Managed-state test fixtures.
//!
//! These fixtures serve rehydration, deployment-session, and activity tests.
//! Keeping the single fake Runtime here avoids parallel fake implementations in
//! those behavior modules while leaving each test beside the behavior it covers.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Message;
use awaken_session_contract::{
    ManagedSessionRepository, PersistedSession, SessionInit, ToolPermissionDecision,
};
use awaken_session_store::SqliteManagedSessionRepository;

use super::{DelegatedRun, OutcomeDrive, RunError, SessionRuntime, StepOutcome};

pub(super) fn ephemeral_session_repo() -> SqliteManagedSessionRepository {
    SqliteManagedSessionRepository::open_in_memory()
        .expect("open ephemeral managed Session repository")
}

pub(super) async fn create_session_fixture(
    repo: &dyn ManagedSessionRepository,
    owner: &str,
    mut session: PersistedSession,
) {
    session.revision = awaken_session_contract::SessionRevision(0);
    let payload = awaken_session_contract::SessionMutationPayload::Replace(session.clone());
    let payload_hash = payload.stable_hash();
    repo.create(
        owner,
        session.clone(),
        awaken_session_contract::IdempotencyRecord {
            key: format!("test:create:{}:{payload_hash}", session.session_id),
            payload_hash,
        },
        Vec::new(),
    )
    .await
    .expect("create Session fixture");
}

pub(super) type RestoredRuntime = (
    String,
    Option<String>,
    usize,
    awaken_session_contract::SessionNetworkPolicy,
    awaken_provisioning_contract::SandboxOverride,
);

pub(super) type ChildCommitAfterHistorySnapshot = (
    String,
    Vec<Message>,
    Vec<awaken_agent_contract::RunLifecycleEvent>,
);

/// A runtime that reports committed Thread facts after the durable Session root
/// has authorized rehydration. The transcript is projection input only and can
/// never establish Session existence by itself.
#[derive(Clone, Default)]
pub(crate) struct RehydrateFake {
    pub(super) restored: Arc<
        std::sync::Mutex<
            Vec<(
                String,
                String,
                awaken_session_contract::ResolvedSessionResources,
            )>,
        >,
    >,
    pub(super) restored_environments: Arc<std::sync::Mutex<Vec<(String, String, String)>>>,
    pub(super) restored_runtimes: Arc<std::sync::Mutex<Vec<RestoredRuntime>>>,
    pub(super) delegated: Arc<std::sync::Mutex<Vec<DelegatedRun>>>,
    pub(super) coordinated:
        Arc<std::sync::Mutex<Vec<awaken_session_contract::CoordinatedThreadLink>>>,
    pub(super) lifecycle: Arc<std::sync::Mutex<Vec<awaken_agent_contract::RunLifecycleEvent>>>,
    /// Test-only interleaving control for lifecycle projection: when set, a
    /// root recovery read exposes this older backend watermark while the
    /// lifecycle feed may already contain newer commits.
    pub(super) root_store_cursor_override: Arc<std::sync::Mutex<Option<u64>>>,
    pub(super) pending_by_thread:
        Arc<std::sync::Mutex<std::collections::HashMap<String, awaken_session_contract::Pending>>>,
    pub(super) disposition_by_thread: Arc<
        std::sync::Mutex<
            std::collections::HashMap<(String, String), awaken_agent_contract::ThreadDisposition>,
        >,
    >,
    pub(super) archive_commits: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    pub(super) thread_tool_replies:
        Arc<std::sync::Mutex<Vec<awaken_session_contract::SessionThreadToolReplyCommand>>>,
    pub(super) delegate_ids: Arc<std::sync::Mutex<Vec<String>>>,
    pub(super) custom_tools: Arc<std::sync::Mutex<Vec<awaken_session_contract::CustomTool>>>,
    pub(super) order: Arc<std::sync::Mutex<Vec<&'static str>>>,
    pub(super) committed: Arc<std::sync::Mutex<Option<Vec<Message>>>>,
    pub(super) committed_by_thread:
        Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<Message>>>>,
    /// Explicit Run records used by canonical coordination fixtures whose
    /// accepted root transcript is committed before any root lifecycle edge.
    pub(super) run_ids_by_thread: Arc<
        std::sync::Mutex<std::collections::HashMap<String, awaken_agent_contract::agent::run::Id>>,
    >,
    /// Exact fake commit coordinates aligned with `committed_by_thread`.
    /// Absent entries mean the fixture committed its configured messages in
    /// one atomic prefix, not that production may omit the coordinates.
    pub(super) message_cursors_by_thread:
        Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u64>>>>,
    pub(super) state_by_thread: Arc<
        std::sync::Mutex<
            std::collections::HashMap<String, Vec<awaken_agent_contract::agent::state::Command>>,
        >,
    >,
    pub(super) state_cursors_by_thread:
        Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u64>>>>,
    pub(super) outcome_projections: Arc<
        std::sync::Mutex<
            std::collections::HashMap<
                (String, String),
                awaken_session_contract::CommittedOutcomeProjection,
            >,
        >,
    >,
    pub(super) usage_by_thread: Arc<
        std::sync::Mutex<std::collections::HashMap<String, awaken_session_contract::SessionUsage>>,
    >,
    /// Test-only commit interleaving: return the current child transcript, then
    /// atomically expose the supplied newer transcript/lifecycle facts. This
    /// models a terminal commit racing a projector read without a second fake.
    pub(super) child_commit_after_history_snapshot:
        Arc<std::sync::Mutex<Option<ChildCommitAfterHistorySnapshot>>>,
    pub(super) pending: Arc<std::sync::Mutex<Option<awaken_session_contract::Pending>>>,
    pub(super) ended: Arc<std::sync::Mutex<Vec<String>>>,
    pub(super) reject_environment_adoption: Arc<std::sync::atomic::AtomicBool>,
}

impl RehydrateFake {
    /// Install the one Runtime-owned accepted `send_to_agent` prefix that
    /// authorizes an ordinary coordinated link. Tests must not fabricate a
    /// Managed-only link anchor: the root ToolUse/ToolResult messages, their
    /// exact commit coordinate, the root Run, and the relationship are one
    /// recovery fixture.
    pub(crate) fn install_agent_coordination_prefix(
        &self,
        session_id: &str,
        root_run_id: awaken_agent_contract::agent::run::Id,
        step: usize,
        call_id: &str,
        link: awaken_session_contract::CoordinatedThreadLink,
    ) {
        assert!(matches!(
            &link.target,
            awaken_session_contract::CoordinatedThreadTarget::Agent { .. }
        ));
        assert_eq!(
            link.created_by_operation_id,
            awaken_runtime_contract::tool_batch::ToolBatch::operation_id_for_step(
                &root_run_id,
                step,
                call_id,
            )
        );
        let agent_id = link
            .target
            .agent_id()
            .expect("ordinary coordination fixture has an Agent target")
            .to_string();
        let mut messages = self
            .committed_by_thread
            .lock()
            .unwrap()
            .get(session_id)
            .cloned()
            .or_else(|| self.committed.lock().unwrap().clone())
            .unwrap_or_default();
        let mut cursors = self
            .message_cursors_by_thread
            .lock()
            .unwrap()
            .get(session_id)
            .filter(|cursors| cursors.len() == messages.len())
            .cloned()
            .unwrap_or_else(|| {
                (1..=messages.len())
                    .map(|cursor| u64::try_from(cursor).expect("fixture message cursor"))
                    .collect()
            });
        let source_commit_cursor = cursors
            .iter()
            .copied()
            .max()
            .unwrap_or_default()
            .checked_add(1)
            .expect("fixture message cursor exhausted");
        messages.extend([
            Message::new(
                awaken_agent_contract::agent::message::Id::assistant(&root_run_id, step),
                awaken_agent_contract::agent::message::Role::Assistant,
                vec![ContentBlock::tool_use(
                    call_id,
                    awaken_ext_builtin_tools::SEND_TO_AGENT,
                    serde_json::json!({
                        "agent_id": agent_id,
                        "message": "fixture coordination"
                    }),
                )],
            ),
            Message::new(
                awaken_agent_contract::agent::message::Id::tool_result(call_id),
                awaken_agent_contract::agent::message::Role::Tool,
                vec![ContentBlock::tool_result(
                    call_id,
                    vec![ContentBlock::text(
                        serde_json::json!({
                            "accepted": true,
                            "session_thread_id": link.thread_id.0.clone()
                        })
                        .to_string(),
                    )],
                )],
            ),
        ]);
        cursors.extend([source_commit_cursor, source_commit_cursor]);
        self.message_cursors_by_thread
            .lock()
            .unwrap()
            .insert(session_id.to_string(), cursors);
        self.committed_by_thread
            .lock()
            .unwrap()
            .insert(session_id.to_string(), messages);
        self.run_ids_by_thread
            .lock()
            .unwrap()
            .insert(session_id.to_string(), root_run_id);
        self.coordinated.lock().unwrap().push(link);
    }

    fn commit_staged_child_prefix_after_snapshot(&self, thread_id: &str) {
        let staged = self
            .child_commit_after_history_snapshot
            .lock()
            .unwrap()
            .take();
        if let Some((target_thread, messages, lifecycle)) = staged {
            if target_thread == thread_id {
                let message_cursor = lifecycle
                    .iter()
                    .filter(|event| event.thread_id.0 == target_thread)
                    .map(|event| event.source_commit_cursor)
                    .max()
                    .expect("staged child message has its atomic lifecycle commit");
                self.message_cursors_by_thread
                    .lock()
                    .unwrap()
                    .insert(target_thread.clone(), vec![message_cursor; messages.len()]);
                self.committed_by_thread
                    .lock()
                    .unwrap()
                    .insert(target_thread, messages);
                self.lifecycle.lock().unwrap().extend(lifecycle);
            } else {
                *self.child_commit_after_history_snapshot.lock().unwrap() =
                    Some((target_thread, messages, lifecycle));
            }
        }
    }
}

#[async_trait]
impl SessionRuntime for RehydrateFake {
    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_contract::FrozenSessionProjection,
        mode: awaken_session_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        let adopts_environment = mode.adopts_resident_environment();
        if let Some(init) =
            crate::test_support::complete_session_projection_init(thread, &projection, &mode)?
        {
            self.prepare_session(thread, init).await?;
        }
        if adopts_environment && let Some(binding) = projection.environment.binding() {
            self.adopt_session_environment(&projection.baseline.agent_id, thread, binding)
                .await?;
        }
        Ok(())
    }

    async fn prepare_session(&self, thread: &str, init: SessionInit) -> Result<(), RunError> {
        self.order.lock().unwrap().push("runtime");
        self.restored_runtimes.lock().unwrap().push((
            thread.to_string(),
            init.runtime,
            0,
            init.environment.network,
            init.environment.sandbox,
        ));
        Ok(())
    }

    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        unreachable!()
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        unreachable!()
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<ContentBlock>,
        _is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        unreachable!()
    }

    async fn define_outcome(
        &self,
        _thread: &str,
        _description: &str,
        _rubric: &str,
        _max_iterations: u32,
    ) -> Result<OutcomeDrive, RunError> {
        unreachable!()
    }

    async fn committed_messages(&self, thread: &str) -> Result<Vec<Message>, RunError> {
        self.order.lock().unwrap().push("history");
        if let Some(messages) = self
            .committed_by_thread
            .lock()
            .unwrap()
            .get(thread)
            .cloned()
        {
            return Ok(messages);
        }
        if let Some(messages) = self.committed.lock().unwrap().clone() {
            return Ok(messages);
        }
        Ok(vec![Message::text(
            awaken_agent_contract::agent::message::Id(format!("{thread}-m0")),
            awaken_agent_contract::agent::message::Role::User,
            "hello",
        )])
    }

    async fn pending_tool(
        &self,
        _thread: &str,
    ) -> Result<Option<awaken_session_contract::Pending>, RunError> {
        Ok(self.pending.lock().unwrap().clone())
    }

    async fn committed_outcome_projection(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<Option<awaken_session_contract::CommittedOutcomeProjection>, RunError> {
        Ok(self
            .outcome_projections
            .lock()
            .unwrap()
            .get(&(thread.to_string(), outcome_id.to_string()))
            .cloned())
    }

    async fn session_usage(
        &self,
        thread: &str,
    ) -> Result<awaken_session_contract::SessionUsage, RunError> {
        Ok(self
            .usage_by_thread
            .lock()
            .unwrap()
            .get(thread)
            .cloned()
            .unwrap_or_default())
    }

    async fn session_thread_usage(
        &self,
        _session_id: &str,
        thread_id: &str,
    ) -> Result<awaken_session_contract::SessionUsage, RunError> {
        Ok(self
            .usage_by_thread
            .lock()
            .unwrap()
            .get(thread_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn coordinated_threads(
        &self,
        _session_id: &str,
    ) -> Result<Vec<awaken_session_contract::CoordinatedThreadLink>, RunError> {
        self.order.lock().unwrap().push("coordination");
        Ok(self.coordinated.lock().unwrap().clone())
    }

    async fn session_thread_recovery_snapshot(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        self.order.lock().unwrap().push(if thread_id == session_id {
            "root_snapshot"
        } else {
            "child_snapshot"
        });
        let lifecycle = self.lifecycle.lock().unwrap().clone();
        let has_outcome_projection = self
            .outcome_projections
            .lock()
            .unwrap()
            .keys()
            .any(|(owner, _)| owner == thread_id);
        let explicit_run_id = self
            .run_ids_by_thread
            .lock()
            .unwrap()
            .get(thread_id)
            .cloned();
        let latest_run_id = if thread_id == session_id {
            lifecycle
                .iter()
                .rev()
                .find(|event| event.thread_id.0 == thread_id)
                .map(|event| event.run_id.clone())
                .or(explicit_run_id.clone())
                .or_else(|| {
                    (self.committed.lock().unwrap().is_some()
                        || self.pending.lock().unwrap().is_some()
                        || has_outcome_projection)
                        .then(|| {
                            awaken_agent_contract::agent::run::Id(format!(
                                "test-root-run:{thread_id}"
                            ))
                        })
                })
        } else {
            self.coordinated
                .lock()
                .unwrap()
                .iter()
                .find(|link| link.thread_id.0 == thread_id)
                .and_then(|link| link.latest_run_id.clone())
        };
        let Some(latest_run_id) = latest_run_id else {
            return Ok(None);
        };
        let messages = if thread_id == session_id {
            self.committed_by_thread
                .lock()
                .unwrap()
                .get(thread_id)
                .cloned()
                .or_else(|| self.committed.lock().unwrap().clone())
                .unwrap_or_default()
        } else {
            self.committed_by_thread
                .lock()
                .unwrap()
                .get(thread_id)
                .cloned()
                .unwrap_or_default()
        };
        let pending = if thread_id == session_id {
            self.pending.lock().unwrap().clone()
        } else {
            self.pending_by_thread
                .lock()
                .unwrap()
                .get(thread_id)
                .cloned()
        };
        let resume_tickets = pending
            .map(|pending| {
                let reason = if pending.client_executed {
                    awaken_agent_contract::agent::awaiting::ToolAwaitReason::ClientExecution
                } else {
                    awaken_agent_contract::agent::awaiting::ToolAwaitReason::Permission
                };
                awaken_agent_contract::thread::read::recovery::RunResumeTicket {
                    run_id: latest_run_id.clone(),
                    ticket: awaken_agent_contract::agent::awaiting::ResumeTicket::new(
                        format!("ticket:{}", pending.tool_use_id),
                        latest_run_id.clone(),
                        awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
                        "test-snapshot",
                        "test-catalog",
                        awaken_agent_contract::agent::awaiting::AwaitTarget::ToolCall {
                            reason,
                            call_id: pending.tool_use_id,
                            tool: awaken_agent_contract::agent::awaiting::PendingTool {
                                tool_id: pending.name,
                                arguments: pending.input,
                            },
                        },
                    ),
                }
            })
            .into_iter()
            .collect();
        let mut state = self
            .state_by_thread
            .lock()
            .unwrap()
            .get(thread_id)
            .cloned()
            .unwrap_or_default();
        if self
            .disposition_by_thread
            .lock()
            .unwrap()
            .get(&(session_id.to_string(), thread_id.to_string()))
            == Some(&awaken_agent_contract::ThreadDisposition::Archived)
        {
            state.push(awaken_agent_contract::archive_thread_command());
        }
        for ((owner, outcome_id), projection) in self.outcome_projections.lock().unwrap().iter() {
            if owner != thread_id {
                continue;
            }
            match projection {
                awaken_session_contract::CommittedOutcomeProjection::Completed(report) => {
                    for iteration in &report.iterations {
                        state.push(awaken_agent_contract::agent::state::Command::set(
                            awaken_agent_contract::agent::state::Scope::Thread,
                            awaken_agent_contract::agent::state::MergePolicy::Disjoint,
                            format!("outcome/{outcome_id}/evaluation/{}", iteration.iteration),
                            serde_json::json!({"fixture": "committed"}),
                        ));
                    }
                }
                awaken_session_contract::CommittedOutcomeProjection::Errored(_) => {}
            }
            // A terminal Outcome commit always updates its owned state and
            // removes the active pointer at the same commit coordinate. Keep
            // the fake structurally faithful so projector tests cannot accept a
            // partial transaction that no production coordinator emits.
            state.push(awaken_agent_contract::agent::state::Command::set(
                awaken_agent_contract::agent::state::Scope::Thread,
                awaken_agent_contract::agent::state::MergePolicy::Disjoint,
                format!("outcome/{outcome_id}/state"),
                serde_json::json!({"fixture": "terminal"}),
            ));
            state.push(awaken_agent_contract::agent::state::Command::remove(
                awaken_agent_contract::agent::state::Scope::Thread,
                awaken_agent_contract::agent::state::MergePolicy::Disjoint,
                "outcome/active",
            ));
        }
        let mut runs = Vec::<awaken_agent_contract::agent::run::Record>::new();
        for event in lifecycle
            .iter()
            .filter(|event| event.thread_id.0 == thread_id)
        {
            if let Some(run) = runs.iter_mut().find(|run| run.id == event.run_id) {
                run.state = event.state.clone();
            } else {
                runs.push(awaken_agent_contract::agent::run::Record {
                    id: event.run_id.clone(),
                    thread_id: event.thread_id.clone(),
                    state: event.state.clone(),
                });
            }
        }
        if let Some(run_id) = explicit_run_id
            && !runs.iter().any(|run| run.id == run_id)
        {
            runs.push(awaken_agent_contract::agent::run::Record {
                id: run_id,
                thread_id: awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
                state: awaken_agent_contract::agent::run::RunState::Running,
            });
        }
        let base_store_cursor = lifecycle
            .iter()
            .map(|event| event.source_commit_cursor)
            .max()
            .unwrap_or_default();
        let base_store_cursor = if thread_id == session_id {
            self.root_store_cursor_override
                .lock()
                .unwrap()
                .unwrap_or(base_store_cursor)
        } else {
            base_store_cursor
        };
        let atomic_fixture_cursor = if messages.is_empty() && state.is_empty() {
            base_store_cursor
        } else {
            base_store_cursor.max(1)
        };
        let message_commit_cursors = self
            .message_cursors_by_thread
            .lock()
            .unwrap()
            .get(thread_id)
            .cloned()
            .unwrap_or_else(|| vec![atomic_fixture_cursor; messages.len()]);
        let state_commit_cursors = self
            .state_cursors_by_thread
            .lock()
            .unwrap()
            .get(thread_id)
            .cloned()
            .unwrap_or_else(|| vec![atomic_fixture_cursor; state.len()]);
        let store_cursor = std::iter::once(base_store_cursor)
            .chain(message_commit_cursors.iter().copied())
            .chain(state_commit_cursors.iter().copied())
            .max()
            .unwrap_or_default();
        let snapshot = awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
            thread_id: awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
            claimed_run_id: latest_run_id.clone(),
            runs,
            latest_run_id: Some(latest_run_id),
            messages,
            message_commit_cursors,
            state,
            state_commit_cursors,
            events: Vec::new(),
            resume_tickets,
            thread_version: lifecycle.len() as u64,
            store_cursor,
            next_commit_ordinal: 0,
        };
        self.commit_staged_child_prefix_after_snapshot(thread_id);
        Ok(Some(snapshot))
    }

    async fn session_thread_disposition(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<awaken_agent_contract::ThreadDisposition, RunError> {
        Ok(self
            .disposition_by_thread
            .lock()
            .unwrap()
            .get(&(session_id.to_string(), thread_id.to_string()))
            .copied()
            .unwrap_or_default())
    }

    async fn archive_session_thread(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<(), RunError> {
        let key = (session_id.to_string(), thread_id.to_string());
        let mut dispositions = self.disposition_by_thread.lock().unwrap();
        if dispositions.get(&key) != Some(&awaken_agent_contract::ThreadDisposition::Archived) {
            dispositions.insert(
                key.clone(),
                awaken_agent_contract::ThreadDisposition::Archived,
            );
            self.archive_commits.lock().unwrap().push(key);
        }
        Ok(())
    }

    async fn session_thread_tool_reply_fence(
        &self,
        command: &awaken_session_contract::SessionThreadToolReplyCommand,
    ) -> Result<awaken_session_contract::SessionThreadToolReplyFence, RunError> {
        let thread_id = command.target.thread_id(&command.session_id);
        let pending = self
            .pending_by_thread
            .lock()
            .unwrap()
            .get(&thread_id.0)
            .cloned()
            .filter(|pending| pending.tool_use_id == command.tool_use_id)
            .ok_or_else(|| RunError::bad_request("reply does not match the pending ticket"))?;
        let run_id = match &command.target {
            awaken_session_contract::SessionThreadTarget::Primary => {
                command.expected_run_id.clone()
            }
            awaken_session_contract::SessionThreadTarget::Child(_) => self
                .coordinated
                .lock()
                .unwrap()
                .iter()
                .find(|link| link.thread_id == thread_id)
                .and_then(|link| link.latest_run_id.clone())
                .ok_or_else(|| RunError::bad_request("coordinated child has no committed Run"))?,
        };
        if run_id != command.expected_run_id
            || command.expected_correlation_id != format!("ticket:{}", pending.tool_use_id)
        {
            return Err(RunError::bad_request("reply ticket fence is stale"));
        }
        Ok(awaken_session_contract::SessionThreadToolReplyFence {
            prior_session_activity_epoch: Some(1),
            already_applied: false,
        })
    }

    async fn reply_session_thread_tool(
        &self,
        delivery: awaken_session_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), RunError> {
        let command = delivery.command;
        let thread_id = command.target.thread_id(&command.session_id);
        let expected_fence = self.session_thread_tool_reply_fence(&command).await?;
        if delivery.fence != expected_fence {
            return Err(RunError::bad_request("reply fence is stale"));
        }
        let removed = self.pending_by_thread.lock().unwrap().remove(&thread_id.0);
        if removed.as_ref().map(|pending| pending.tool_use_id.as_str())
            != Some(command.tool_use_id.as_str())
        {
            return Err(RunError::bad_request(
                "reply does not match the coordinated child pending ticket",
            ));
        }
        self.thread_tool_replies.lock().unwrap().push(command);
        Ok(())
    }

    async fn committed_run_lifecycle(
        &self,
        _thread: &str,
        cursor: awaken_agent_contract::RunLifecycleCursor,
        limit: usize,
    ) -> Result<awaken_agent_contract::RunLifecyclePage, RunError> {
        let events = self
            .lifecycle
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.cursor > cursor)
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        Ok(awaken_agent_contract::RunLifecyclePage {
            next_cursor: events.last().map_or(cursor, |event| event.cursor),
            events,
        })
    }

    async fn delegated_runs(&self, _thread: &str) -> Result<Vec<DelegatedRun>, RunError> {
        self.order.lock().unwrap().push("delegations");
        Ok(self.delegated.lock().unwrap().clone())
    }

    async fn execute_terminal_cleanup(
        &self,
        command: awaken_session_contract::SessionCleanupCommand,
    ) -> Result<awaken_session_contract::SessionCleanupCompletion, RunError> {
        self.ended.lock().unwrap().push(command.thread_id.clone());
        Ok(awaken_session_contract::SessionCleanupCompletion::new(
            &command,
            Vec::new(),
        ))
    }

    async fn adopt_session_environment(
        &self,
        agent: &str,
        thread: &str,
        binding: &str,
    ) -> Result<(), RunError> {
        if self
            .reject_environment_adoption
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(RunError::internal("sandbox is owned by another runtime"));
        }
        self.order.lock().unwrap().push("environment");
        self.restored_environments.lock().unwrap().push((
            agent.to_string(),
            thread.to_string(),
            binding.to_string(),
        ));
        Ok(())
    }

    fn model(&self) -> String {
        "host-default-model".to_string()
    }

    fn capabilities_for(&self, _thread: &str) -> awaken_session_contract::AgentCapabilities {
        awaken_session_contract::AgentCapabilities {
            delegates: self.delegate_ids.lock().unwrap().clone(),
            custom_tools: self
                .custom_tools
                .lock()
                .unwrap()
                .iter()
                .map(|tool| awaken_session_contract::CustomTool {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    input_schema: tool.input_schema.clone(),
                })
                .collect(),
            ..Default::default()
        }
    }

    async fn apply_session_inputs(
        &self,
        thread: &str,
        workspace_id: &str,
        _resource_revision: u64,
        inputs: &awaken_session_contract::ResolvedSessionResources,
    ) -> Result<(), RunError> {
        self.order.lock().unwrap().push("resources");
        self.restored.lock().unwrap().push((
            thread.to_string(),
            workspace_id.to_string(),
            inputs.clone(),
        ));
        Ok(())
    }
}

#[async_trait]
impl awaken_session_contract::McpAttachmentRealizer for RehydrateFake {
    async fn stage_mcp_attachment(
        &self,
        request: awaken_session_contract::StageMcpAttachment,
    ) -> Result<awaken_session_contract::McpRealizationReceipt, RunError> {
        self.order.lock().unwrap().push("mcp");
        if let Some(restored) = self
            .restored_runtimes
            .lock()
            .unwrap()
            .iter_mut()
            .find(|restored| restored.0 == request.generation.session_id)
        {
            restored.2 += 1;
        }
        let receipt_fingerprint = request.fingerprint();
        Ok(awaken_session_contract::McpRealizationReceipt {
            generation: request.generation,
            realization_id: request.realization_id,
            selected_plaintext_holder: request.selected_plaintext_holder,
            actual_realization_kind: None,
            receipt_fingerprint,
        })
    }

    async fn publish_mcp_generation(
        &self,
        _generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        Ok(())
    }

    async fn drain_mcp_generation(
        &self,
        _generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        Ok(())
    }
}
