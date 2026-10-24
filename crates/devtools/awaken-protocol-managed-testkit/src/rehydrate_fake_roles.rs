//! The Session Runtime roles of [`RehydrateFake`] beyond its core run loop.

use async_trait::async_trait;
use awaken_agent_contract::agent::message::Message;
use awaken_session_execution_contract::{OutcomeDrive, SessionRuntime};
use awaken_session_types::RunError;

use crate::rehydrate_fake::{RehydrateFake, scripted_read_failure};

#[async_trait::async_trait]
impl awaken_session_execution_contract::BudgetRuntime for RehydrateFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Resume an exact `BudgetReached` ticket without injecting transcript
    /// content. Implementations reuse the foreground Run driver or the existing
    /// durable dispatch row; they must not enqueue a replacement Run.
    async fn resume_budget_reached(
        &self,
        delivery: awaken_session_application_contract::SessionBudgetResumeDelivery,
    ) -> Result<
        awaken_session_application_contract::SessionBudgetResumeDisposition,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .resume_budget_reached(delivery)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Discover current budget-paused continuations from committed Run tickets
    /// and existing Session-affine dispatch rows. This is a recovery query over
    /// those authorities, never a pause registry.
    async fn session_budget_resume_tickets(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<
        Vec<awaken_session_application_contract::SessionBudgetResumeTicket>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .session_budget_resume_tickets(session_id)
            .await
    }

    async fn session_usage(
        &self,
        thread: &str,
    ) -> Result<awaken_session_budget_contract::SessionUsage, RunError> {
        self.reads
            .on_read("usage")
            .await
            .map_err(scripted_read_failure)?;
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
        _session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<awaken_session_budget_contract::SessionUsage, RunError> {
        self.reads
            .on_read("child_usage")
            .await
            .map_err(scripted_read_failure)?;
        Ok(self
            .usage_by_thread
            .lock()
            .unwrap()
            .get(thread_id)
            .cloned()
            .unwrap_or_default())
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::StreamingRunRuntime for RehydrateFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Run one request with its protocol-projected, backend-neutral content
    /// owner. Adapters that do not support attributed capture may use the
    /// compatibility default; runtime hosts override it and persist attribution
    /// on the durable activation.
    async fn run_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _data_subject_id: Option<String>,
    ) -> Result<awaken_session_application_contract::StepOutcome, awaken_session_types::RunError>
    {
        self.run(agent, thread, content).await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Run one user request, installing `sink` as the Run's best-effort live-progress
    /// channel so the adapter can project in-flight `stream::Kind` into
    /// `event_start`/`event_delta` previews. The committed [`awaken_session_application_contract::StepOutcome`] is
    /// identical to [`run`](Self::run) — the sink only mirrors in-flight events. The
    /// default ignores the sink and delegates to `run`, so a host without a
    /// streaming path (or a test double) is unaffected.
    async fn run_streaming(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _sink: std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<awaken_session_application_contract::StepOutcome, awaken_session_types::RunError>
    {
        self.run(agent, thread, content).await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Streaming counterpart of [`run_attributed`](Self::run_attributed).
    async fn run_streaming_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _data_subject_id: Option<String>,
        sink: std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<awaken_session_application_contract::StepOutcome, awaken_session_types::RunError>
    {
        self.run_streaming(agent, thread, content, sink).await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Interrupt the run in flight on `thread` (a `user.interrupt`): cancel it so
    /// an in-progress outcome ends `interrupted`. A no-op when nothing is running.
    async fn interrupt(&self, thread: &str) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .interrupt(thread)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Rebind `thread` to `model` for its subsequent Runs (R5, per-Run override).
    /// The default is a no-op, so a host without per-thread model routing is
    /// unaffected; the server impl re-stages the thread's model and evicts the
    /// cached context so the next Run resolves the new executor.
    async fn rebind_model(
        &self,
        thread: &str,
        model: &str,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .rebind_model(thread, model)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Whether the thread's selected model accepts a system message after the
    /// conversation has started. Runtimes whose models do not support this can
    /// fail admission before the Managed adapter persists the inbound event.
    async fn supports_mid_conversation_system(
        &self,
        thread: &str,
    ) -> Result<bool, awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .supports_mid_conversation_system(thread)
            .await
    }

    async fn pending_tool(
        &self,
        _thread: &str,
    ) -> Result<Option<awaken_session_application_contract::Pending>, RunError> {
        Ok(self.pending.lock().unwrap().clone())
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::EnvironmentRuntime for RehydrateFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Replace the complete Session-local tool configuration after an
    /// idle-session update.
    /// Implementations rebuild the disposable runtime context; durable desired
    /// state has already committed before this projection call.
    async fn replace_session_tools(
        &self,
        thread: &str,
        tools: awaken_session_configuration_contract::SessionToolConfiguration,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .replace_session_tools(thread, tools)
            .await
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

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Close Runtime admission and prove that no primary, delegated,
    /// shared-background, MCP, tool, Hand, or child-process effect can mutate
    /// this exact environment generation.
    async fn quiesce_session_environment(
        &self,
        thread: &str,
        operation: &awaken_session_state_contract::SessionEnvironmentOperation,
        source_effect_id: &str,
        source_binding: &str,
        generation: &awaken_session_state_contract::SandboxGeneration,
        expected_mcp_generations: &[awaken_session_configuration_contract::mcp::McpGenerationRef],
    ) -> Result<awaken_session_state_contract::QuiescenceReceipt, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .quiesce_session_environment(
                thread,
                operation,
                source_effect_id,
                source_binding,
                generation,
                expected_mcp_generations,
            )
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Resolve the current versions of already-authorized Skill resource ids once
    /// at Session creation. The implementation receives only a trusted Workspace
    /// and resource ids; it performs no principal/role/policy decision.
    async fn resolve_session_skills(
        &self,
        workspace_id: &str,
        skills: &[awaken_agent_contract::AgentSkillBinding],
    ) -> Result<
        Vec<awaken_session_configuration_contract::resource::ResolvedSkillBinding>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .resolve_session_skills(workspace_id, skills)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationEffects for RehydrateFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Validate the exact provider-effective Sandbox layout without publishing
    /// process-local state or performing provider, cache, Skill, File, Vault,
    /// Repository, or Git I/O. Session creation and Resource mutation call this
    /// before their durable/effect boundaries; Runtime repeats it at the final
    /// realization edge as defense in depth.
    fn validate_session_sandbox_layout(
        &self,
        thread: &str,
        layout: &awaken_session_configuration_contract::SessionSandboxLayout,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_sandbox_layout(thread, layout)
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Persist one verified filesystem checkpoint. Replays with the same
    /// operation id must return the same authoritative object receipt.
    async fn checkpoint_session_environment(
        &self,
        thread: &str,
        request: awaken_session_state_contract::SandboxCheckpointRequest,
    ) -> Result<awaken_session_state_contract::CheckpointReceipt, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .checkpoint_session_environment(thread, request)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Complete every live source-durability effect without deleting the
    /// source. The aggregate must durably admit the returned receipt before it
    /// invokes physical disposal.
    async fn prepare_checkpoint_source_disposal(
        &self,
        thread: &str,
        preparation: &awaken_session_state_contract::SourceReleasePreparationEffect,
        generation: &awaken_session_state_contract::SandboxGeneration,
        source_binding: &str,
    ) -> Result<
        awaken_session_state_contract::SourceReleasePreparedReceipt,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .prepare_checkpoint_source_disposal(thread, preparation, generation, source_binding)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Physically dispose the source only after the aggregate has durably
    /// committed the exact source-release preparation receipt.
    async fn dispose_prepared_checkpoint_source(
        &self,
        thread: &str,
        disposal: &awaken_session_state_contract::SourceReleaseDisposal,
    ) -> Result<awaken_session_state_contract::SourceDisposedReceipt, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .dispose_prepared_checkpoint_source(thread, disposal)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Restore a distinct environment from the exact durable checkpoint.
    async fn restore_checkpointed_session_environment(
        &self,
        request: awaken_session_state_contract::SandboxRestoreRequest,
    ) -> Result<awaken_session_state_contract::RestoreReceipt, awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .restore_checkpointed_session_environment(request)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Idempotently delete checkpoint bytes at a terminal edge. It never
    /// restores the Environment merely to clean it up.
    async fn delete_session_checkpoint(
        &self,
        thread: &str,
        checkpoint: &awaken_session_state_contract::SandboxCheckpointRef,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .delete_session_checkpoint(thread, checkpoint)
            .await
    }

    async fn apply_session_inputs(
        &self,
        thread: &str,
        transition: &awaken_session_configuration_contract::resource::SessionResourceTransition,
    ) -> Result<(), RunError> {
        self.order.lock().unwrap().push("resources");
        self.restored.lock().unwrap().push((
            thread.to_string(),
            transition.desired().workspace_id.clone(),
            transition.desired().resources.clone(),
        ));
        Ok(())
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Execute terminal Repository publication under the same realization
    /// generation that owns the cleanup command set and return either its
    /// secret-free receipt or one permanent compare-and-swap rejection. The
    /// caller verifies and durably records that outcome before ordinary root
    /// cleanup may dispose the shared environment. Runtimes must override this
    /// edge and carry the lease to the Repository transport boundary; falling
    /// back would erase the terminal ownership proof.
    async fn execute_terminal_repository_publication_for_lease(
        &self,
        command: awaken_session_state_contract::SessionRepositoryPublicationCommand,
        lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
    ) -> Result<
        awaken_session_state_contract::SessionRepositoryPublicationEffect,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .execute_terminal_repository_publication_for_lease(command, lease)
            .await
    }

    async fn dispose_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<awaken_session_state_contract::SessionCleanupDisposalReceipt, RunError> {
        Ok(crate::complete_terminal_cleanup_disposal(&effect))
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::ThreadReadRuntime for RehydrateFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// True when durable truth already exists for `thread`. Session-id minting
    /// consults this to skip ids a previous process persisted; implementations
    /// MUST answer without materializing any per-thread state (no context
    /// build, no cache entry) — probing must be free of side effects. The
    /// default reports nothing, so an ephemeral host mints densely from 0.
    async fn owns_thread(&self, thread: &str) -> Result<bool, awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .owns_thread(thread)
            .await
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

    async fn session_thread_recovery_snapshot(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        self.reads
            .on_read(if thread_id == session_id {
                "root_snapshot"
            } else {
                "child_snapshot"
            })
            .await
            .map_err(scripted_read_failure)?;
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
        let resume_tickets: Vec<awaken_agent_contract::thread::read::recovery::RunResumeTicket> = pending
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
                            authority: awaken_agent_contract::agent::awaiting::ToolCallAuthority::RuntimeToolBatch,
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
        if let Some(usage) = self.usage_by_thread.lock().unwrap().get(thread_id).cloned() {
            // Production derives accounting from this exact committed state
            // prefix; keep the fake structurally faithful.
            state.push(crate::fixture_usage_command(usage));
        }
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
                awaken_session_execution_contract::CommittedOutcomeProjection::Completed(
                    report,
                ) => {
                    for iteration in &report.iterations {
                        state.push(awaken_agent_contract::agent::state::Command::set(
                            awaken_agent_contract::agent::state::Scope::Thread,
                            awaken_agent_contract::agent::state::MergePolicy::Disjoint,
                            format!("outcome/{outcome_id}/evaluation/{}", iteration.iteration),
                            serde_json::json!({"fixture": "committed"}),
                        ));
                    }
                }
                awaken_session_execution_contract::CommittedOutcomeProjection::Errored(_) => {}
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
        // Honor the recovery contract: a latest coordinate always has its
        // committed Run record. A fixture Run without scripted lifecycle is
        // Awaiting exactly when the fixture exposes its ticket, else Running.
        if !runs.iter().any(|run| run.id == latest_run_id) {
            let awaiting = resume_tickets
                .iter()
                .any(|ticket| ticket.run_id == latest_run_id);
            runs.push(awaken_agent_contract::agent::run::Record {
                id: latest_run_id.clone(),
                thread_id: awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
                state: if awaiting {
                    awaken_agent_contract::agent::run::RunState::Awaiting
                } else {
                    awaken_agent_contract::agent::run::RunState::Running
                },
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
        let explicit_state_cursors = self
            .state_cursors_by_thread
            .lock()
            .unwrap()
            .get(thread_id)
            .cloned();
        let (state, state_commit_cursors) = match explicit_state_cursors {
            Some(cursors) => (state, cursors),
            None => crate::fixture_state_log(
                state,
                atomic_fixture_cursor,
                self.state_log
                    .lock()
                    .unwrap()
                    .entry(thread_id.to_string())
                    .or_default(),
            ),
        };
        let store_cursor = std::iter::once(base_store_cursor)
            .chain(message_commit_cursors.iter().copied())
            .chain(state_commit_cursors.iter().copied())
            .max()
            .unwrap_or_default();
        let snapshot = awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
            commit_timestamps: self
                .commit_times_by_thread
                .lock()
                .unwrap()
                .get(thread_id)
                .cloned()
                .unwrap_or_default(),
            thread_id: awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
            claimed_run_id: latest_run_id.clone(),
            runs,
            latest_run_id: Some(latest_run_id),
            messages,
            message_commit_cursors,
            state,
            state_commit_cursors,
            events: crate::fixture_thread_audit(
                &lifecycle,
                thread_id,
                store_cursor,
                resume_tickets.first().map(|ticket| ticket.ticket.target()),
                &self.unaudited_awaits.lock().unwrap(),
                &mut self.await_targets.lock().unwrap(),
            ),
            resume_tickets,
            thread_version: lifecycle.len() as u64,
            store_cursor,
            next_commit_ordinal: 0,
        };
        self.commit_staged_child_prefix_after_snapshot(thread_id);
        Ok(Some(snapshot))
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact committed Run through the same internally consistent
    /// logical-Thread prefix. Settlement already owns the immutable Run id from
    /// its guarded dispatch and must not rediscover it through a process-local
    /// "latest Run" projection.
    ///
    /// The compatibility default filters the existing Thread snapshot. Shared
    /// durable adapters override this method so a cold replica can query its
    /// authoritative [`RunRecoverySource`](awaken_agent_contract::thread::read::recovery::RunRecoverySource)
    /// directly by `run_id`.
    async fn session_thread_run_recovery_snapshot(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<
        Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::select_recovery_snapshot_run(
            self.session_thread_recovery_snapshot(session_id, thread_id)
                .await?,
            run_id,
        )
    }

    async fn session_thread_disposition(
        &self,
        session_id: &awaken_session_types::SessionId,
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

    async fn committed_outcome_projection(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<Option<awaken_session_execution_contract::CommittedOutcomeProjection>, RunError>
    {
        self.reads
            .on_read("outcome")
            .await
            .map_err(scripted_read_failure)?;
        Ok(self
            .outcome_projections
            .lock()
            .unwrap()
            .get(&(thread.to_string(), outcome_id.to_string()))
            .cloned())
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::TerminalCleanupRuntime for RehydrateFake {
    async fn observe_terminal_cleanup_retention(
        &self,
        session_id: &awaken_session_execution_contract::__macro_support::SessionId,
        projection: &awaken_session_execution_contract::__macro_support::FrozenSessionProjection,
        source_prepared: bool,
    ) -> Result<bool, awaken_session_execution_contract::__macro_support::RunError> {
        awaken_session_execution_contract::TerminalCleanupRuntime::observe_terminal_cleanup_retention(
            &awaken_session_execution_contract::NeutralRuntime,
            session_id,
            projection,
            source_prepared,
        )
        .await
    }

    async fn prepare_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_state_contract::SessionTerminalCleanupEffect,
        authorization: awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization,
    ) -> Result<awaken_session_state_contract::SessionCleanupPreparation, RunError> {
        self.ended
            .lock()
            .unwrap()
            .push(effect.command.thread_id.clone());
        crate::complete_terminal_cleanup_preparation(&effect, &authorization)
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Retire only a prepared child projection after Control durably accepts
    /// its exact preparation. The shared root Environment must remain until the
    /// aggregate-wide physical disposal is complete.
    async fn acknowledge_terminal_cleanup_preparation(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
    ) {
        awaken_session_execution_contract::NeutralRuntime
            .acknowledge_terminal_cleanup_preparation(effect)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Retire only the process-local teardown projection after Control has
    /// durably accepted the exact disposal receipt. No provider or Resource
    /// effect is permitted at this edge.
    async fn acknowledge_terminal_cleanup_disposal(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) {
        awaken_session_execution_contract::NeutralRuntime
            .acknowledge_terminal_cleanup_disposal(effect)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Retire a process-local terminal projection after Control reports that
    /// the aggregate has no remaining cleanup work. This closes final receipt
    /// response loss without replaying provider I/O or inventing another
    /// completion registry.
    async fn acknowledge_completed_terminal_cleanup(
        &self,
        session_id: &awaken_session_types::SessionId,
        lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
    ) {
        awaken_session_execution_contract::NeutralRuntime
            .acknowledge_completed_terminal_cleanup(session_id, lease)
            .await
    }

    async fn install_terminal_cleanup_assignment(
        &self,
        _assignment: &awaken_session_realization_contract::SessionTerminalCleanupAssignment,
        _proof: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), RunError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::OutcomeRuntime for RehydrateFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Persist one Outcome aggregate without executing a Worker or Grader Run.
    /// The opaque id is supplied by the application command so crash replay and
    /// activity admission share one stable operation identity.
    async fn prepare_outcome(
        &self,
        thread: &str,
        outcome_id: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<u64, awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .prepare_outcome(thread, outcome_id, description, rubric, max_iterations)
            .await
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

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Continue the active Outcome after the ordinary Run resume has committed.
    /// `None` proves there is no active aggregate; implementations must rebuild
    /// continuation exclusively from the Thread-owned Outcome state.
    async fn continue_outcome(
        &self,
        thread: &str,
    ) -> Result<
        Option<awaken_session_execution_contract::OutcomeDrive>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .continue_outcome(thread)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::CapabilityRuntime for RehydrateFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// The advertised capability surface echoed in the session's agent object. The
    /// default reports nothing; a real host overrides it with its built-in tools,
    /// custom tools, skills, and delegate roster so the session enumerates what it does.
    fn capabilities(
        &self,
    ) -> Result<awaken_session_configuration_contract::AgentCapabilities, RunError> {
        awaken_session_execution_contract::NeutralRuntime.capabilities()
    }

    fn capabilities_for(
        &self,
        _thread: &str,
    ) -> Result<awaken_session_configuration_contract::AgentCapabilities, RunError> {
        Ok(awaken_session_configuration_contract::AgentCapabilities {
            delegates: self.delegate_ids.lock().unwrap().clone(),
            custom_tools: self
                .custom_tools
                .lock()
                .unwrap()
                .iter()
                .map(|tool| awaken_session_configuration_contract::CustomTool {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    input_schema: tool.input_schema.clone(),
                })
                .collect(),
            ..Default::default()
        })
    }
}

#[async_trait]
impl awaken_session_execution_contract::McpAttachmentRealizer for RehydrateFake {
    async fn stage_mcp_attachment(
        &self,
        request: awaken_session_configuration_contract::mcp::StageMcpAttachment,
    ) -> Result<awaken_session_configuration_contract::mcp::McpRealizationReceipt, RunError> {
        self.order.lock().unwrap().push("mcp");
        if let Some(restored) = self
            .restored_runtimes
            .lock()
            .unwrap()
            .iter_mut()
            .find(|restored| restored.0 == request.generation.session_id())
        {
            restored.2 += 1;
        }
        let receipt_fingerprint = request.fingerprint();
        Ok(
            awaken_session_configuration_contract::mcp::McpRealizationReceipt {
                generation: request.generation,
                realization_id: request.realization_id,
                selected_plaintext_holder: request.selected_plaintext_holder,
                actual_realization_kind: None,
                receipt_fingerprint,
            },
        )
    }

    async fn publish_mcp_generation(
        &self,
        _generation: awaken_session_configuration_contract::mcp::McpGenerationRef,
    ) -> Result<(), RunError> {
        if self
            .reject_mcp_publication
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(RunError::unavailable("secret-publication-canary"));
        }
        Ok(())
    }

    async fn drain_mcp_generation(
        &self,
        _generation: awaken_session_configuration_contract::mcp::McpGenerationRef,
    ) -> Result<(), RunError> {
        Ok(())
    }
}
