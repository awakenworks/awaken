use super::manager::SubAgentCompletion;
use super::remote_a2a::{
    cancel_a2a_run, fetch_a2a_task_snapshot, poll_a2a_run_to_completion, submit_a2a_run,
};
use super::tools::{
    append_remote_mirror_messages, persisted_summary, remote_a2a_run_ref,
    remote_mirror_input_messages, remote_mirror_thread_id, resolve_remote_a2a_target,
    state_write_failed, sub_agent_thread_id, sync_remote_snapshot_to_mirror_thread, to_tool_result,
    tool_error, ToolArgError,
};
use super::*;
use crate::composition::{A2aAgentBinding, AgentBinding, AgentCatalog, ResolvedAgent};
use crate::contracts::ToolCallContext;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub(super) struct AgentBackendStartRequest {
    pub(super) run_id: String,
    pub(super) owner_thread_id: String,
    pub(super) agent_id: String,
    pub(super) parent_run_id: Option<String>,
    pub(super) output_thread_id: String,
    pub(super) messages: Vec<Message>,
    pub(super) initial_state: Option<Value>,
    pub(super) background: bool,
}

#[derive(Debug, Clone)]
pub(super) struct AgentBackendRefreshRequest {
    pub(super) run_id: String,
    pub(super) owner_thread_id: String,
    pub(super) persisted: SubAgent,
}

#[derive(Debug, Clone)]
pub(super) struct AgentBackendOutputSyncRequest {
    pub(super) run_id: String,
    pub(super) owner_thread_id: Option<String>,
    pub(super) thread_id: String,
    pub(super) sub: SubAgent,
}

#[async_trait]
pub(super) trait AgentBackend: Send + Sync {
    fn supports_resume(&self) -> bool {
        false
    }

    fn supports_fork_context(&self) -> bool {
        false
    }

    fn supports_orphan_refresh(&self) -> bool {
        false
    }

    fn default_output_thread_id(&self, run_id: &str) -> String;

    async fn start(
        &self,
        os: &AgentOs,
        handles: &Arc<SubAgentHandleTable>,
        ctx: &ToolCallContext<'_>,
        request: AgentBackendStartRequest,
        tool_name: &str,
    ) -> ToolResult;

    async fn refresh(
        &self,
        _os: &AgentOs,
        _ctx: &ToolCallContext<'_>,
        request: AgentBackendRefreshRequest,
        tool_name: &str,
    ) -> ToolResult {
        tool_error(
            tool_name,
            "invalid_state",
            format!(
                "Run '{}' cannot be refreshed without a live executor",
                request.run_id
            ),
        )
    }

    async fn stop(&self, run_id: &str, _sub: &SubAgent) -> Result<SubAgentSummary, ToolArgError> {
        Err(ToolArgError::new(
            "invalid_state",
            format!("Run '{run_id}' is not backed by a remote agent backend"),
        ))
    }

    async fn sync_output(
        &self,
        _os: &AgentOs,
        _ctx: &ToolCallContext<'_>,
        _request: AgentBackendOutputSyncRequest,
    ) -> Result<(), ToolArgError> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct LocalAgentBackend;

#[async_trait]
impl AgentBackend for LocalAgentBackend {
    fn supports_resume(&self) -> bool {
        true
    }

    fn supports_fork_context(&self) -> bool {
        true
    }

    fn default_output_thread_id(&self, run_id: &str) -> String {
        sub_agent_thread_id(run_id)
    }

    async fn start(
        &self,
        os: &AgentOs,
        handles: &Arc<SubAgentHandleTable>,
        ctx: &ToolCallContext<'_>,
        request: AgentBackendStartRequest,
        tool_name: &str,
    ) -> ToolResult {
        let AgentBackendStartRequest {
            run_id,
            owner_thread_id,
            agent_id,
            parent_run_id,
            output_thread_id,
            messages,
            initial_state,
            background,
        } = request;
        let execution = SubAgentExecutionRef::local(output_thread_id.clone());
        let sub = SubAgent {
            execution: execution.clone(),
            parent_run_id: parent_run_id.clone(),
            agent_id: agent_id.clone(),
            status: SubAgentStatus::Running,
            error: None,
        };
        let parent_tool_call_id = ctx.call_id().to_string();

        if background {
            let token = RunCancellationToken::new();
            let parent_run_id_bg = parent_run_id.clone();
            let parent_tool_call_id_bg = parent_tool_call_id.clone();
            let epoch = handles
                .put_running(
                    &run_id,
                    owner_thread_id.clone(),
                    execution.clone(),
                    agent_id.clone(),
                    parent_run_id.clone(),
                    Some(token.clone()),
                )
                .await;

            if let Err(err) = ctx
                .state_of::<SubAgentState>()
                .runs_insert(run_id.to_string(), sub)
            {
                let _ = handles.remove_if_epoch(&run_id, epoch).await;
                return state_write_failed(tool_name, err);
            }

            let handles = handles.clone();
            let os = os.clone();
            let run_id_bg = run_id.clone();
            let agent_id_bg = agent_id.clone();
            let child_thread_id_bg = output_thread_id;
            let parent_thread_id_bg = owner_thread_id;
            tokio::spawn(async move {
                let completion = execute_sub_agent(
                    os,
                    SubAgentExecutionRequest {
                        agent_id: agent_id_bg,
                        child_thread_id: child_thread_id_bg,
                        run_id: run_id_bg.clone(),
                        parent_run_id: parent_run_id_bg,
                        parent_tool_call_id: Some(parent_tool_call_id_bg),
                        parent_thread_id: parent_thread_id_bg,
                        messages,
                        initial_state,
                        cancellation_token: Some(token),
                    },
                    None,
                )
                .await;
                let _ = handles
                    .update_after_completion(&run_id_bg, epoch, completion)
                    .await;
            });

            return to_tool_result(
                tool_name,
                SubAgentSummary {
                    run_id,
                    agent_id,
                    status: SubAgentStatus::Running,
                    error: None,
                },
            );
        }

        let epoch = handles
            .put_running(
                &run_id,
                owner_thread_id.clone(),
                execution.clone(),
                agent_id.clone(),
                parent_run_id.clone(),
                None,
            )
            .await;

        let forward_progress =
            |update: crate::contracts::runtime::tool_call::ToolCallProgressUpdate| {
                ctx.report_tool_call_progress(update)
            };

        let completion = execute_sub_agent(
            os.clone(),
            SubAgentExecutionRequest {
                agent_id: agent_id.clone(),
                child_thread_id: output_thread_id.clone(),
                run_id: run_id.clone(),
                parent_run_id: parent_run_id.clone(),
                parent_tool_call_id: Some(parent_tool_call_id),
                parent_thread_id: owner_thread_id,
                messages,
                initial_state,
                cancellation_token: None,
            },
            Some(&forward_progress),
        )
        .await;

        let completed_sub = SubAgent {
            execution: execution.clone(),
            parent_run_id,
            agent_id: agent_id.clone(),
            status: completion.status,
            error: completion.error.clone(),
        };

        let summary = handles
            .update_after_completion(&run_id, epoch, completion)
            .await
            .unwrap_or_else(|| SubAgentSummary {
                run_id: run_id.clone(),
                agent_id,
                status: completed_sub.status,
                error: completed_sub.error.clone(),
            });

        if let Err(err) = ctx
            .state_of::<SubAgentState>()
            .runs_insert(run_id.to_string(), completed_sub)
        {
            return state_write_failed(tool_name, err);
        }
        to_tool_result(tool_name, summary)
    }
}

#[derive(Debug, Clone)]
struct A2aAgentBackend {
    target_id: String,
    target: A2aAgentBinding,
}

impl A2aAgentBackend {
    fn new(target_id: impl Into<String>, target: A2aAgentBinding) -> Self {
        Self {
            target_id: target_id.into(),
            target,
        }
    }
}

#[async_trait]
impl AgentBackend for A2aAgentBackend {
    fn default_output_thread_id(&self, run_id: &str) -> String {
        remote_mirror_thread_id(run_id)
    }

    fn supports_orphan_refresh(&self) -> bool {
        true
    }

    async fn start(
        &self,
        os: &AgentOs,
        handles: &Arc<SubAgentHandleTable>,
        ctx: &ToolCallContext<'_>,
        request: AgentBackendStartRequest,
        tool_name: &str,
    ) -> ToolResult {
        if request.initial_state.is_some() {
            return tool_error(
                tool_name,
                "invalid_arguments",
                "remote agents do not support seeded local state",
            );
        }

        let AgentBackendStartRequest {
            run_id,
            owner_thread_id,
            agent_id,
            parent_run_id,
            output_thread_id,
            messages,
            background,
            ..
        } = request;

        let submission = match submit_a2a_run(&self.target, &messages).await {
            Ok(submission) => submission,
            Err(err) => return tool_error(tool_name, "remote_error", err),
        };
        if let Err(err) = append_remote_mirror_messages(
            os,
            &output_thread_id,
            Some(&owner_thread_id),
            &run_id,
            parent_run_id.as_deref(),
            crate::contracts::CheckpointReason::UserMessage,
            remote_mirror_input_messages(&run_id, &messages),
        )
        .await
        {
            let _ = cancel_a2a_run(&self.target, &submission.task_id).await;
            return tool_error(tool_name, "store_error", err);
        }
        let execution = SubAgentExecutionRef::remote_a2a(
            self.target_id.clone(),
            submission.context_id.clone(),
            submission.task_id.clone(),
            Some(output_thread_id.clone()),
        );

        if background {
            let epoch = handles
                .put_running(
                    &run_id,
                    owner_thread_id.clone(),
                    execution.clone(),
                    agent_id.clone(),
                    parent_run_id.clone(),
                    None,
                )
                .await;
            let running = SubAgent {
                execution: execution.clone(),
                parent_run_id: parent_run_id.clone(),
                agent_id: agent_id.clone(),
                status: SubAgentStatus::Running,
                error: None,
            };
            if let Err(err) = ctx
                .state_of::<SubAgentState>()
                .runs_insert(run_id.to_string(), running)
            {
                let _ = handles.remove_if_epoch(&run_id, epoch).await;
                let _ = cancel_a2a_run(&self.target, &submission.task_id).await;
                return state_write_failed(tool_name, err);
            }

            let handles = handles.clone();
            let os = os.clone();
            let run_id_bg = run_id.clone();
            let task_id_bg = submission.task_id.clone();
            let target_bg = self.target.clone();
            let output_thread_id_bg = output_thread_id.clone();
            let owner_thread_id_bg = owner_thread_id.clone();
            let parent_run_id_bg = parent_run_id.clone();
            tokio::spawn(async move {
                let snapshot = poll_a2a_run_to_completion(&target_bg, &task_id_bg).await;
                let mirror_error = sync_remote_snapshot_to_mirror_thread(
                    &os,
                    &output_thread_id_bg,
                    Some(&owner_thread_id_bg),
                    &run_id_bg,
                    parent_run_id_bg.as_deref(),
                    &snapshot,
                )
                .await
                .err();
                let completion = SubAgentCompletion {
                    status: if mirror_error.is_some() {
                        SubAgentStatus::Failed
                    } else {
                        snapshot.status
                    },
                    error: mirror_error.or(snapshot.error),
                };
                let _ = handles
                    .update_after_completion(&run_id_bg, epoch, completion)
                    .await;
            });

            return to_tool_result(
                tool_name,
                SubAgentSummary {
                    run_id,
                    agent_id,
                    status: SubAgentStatus::Running,
                    error: None,
                },
            );
        }

        let epoch = handles
            .put_running(
                &run_id,
                owner_thread_id.clone(),
                execution.clone(),
                agent_id.clone(),
                parent_run_id.clone(),
                None,
            )
            .await;
        let snapshot = poll_a2a_run_to_completion(&self.target, &submission.task_id).await;
        let mirror_error = sync_remote_snapshot_to_mirror_thread(
            os,
            &output_thread_id,
            Some(&owner_thread_id),
            &run_id,
            parent_run_id.as_deref(),
            &snapshot,
        )
        .await
        .err();
        let completion = SubAgentCompletion {
            status: if mirror_error.is_some() {
                SubAgentStatus::Failed
            } else {
                snapshot.status
            },
            error: mirror_error.clone().or(snapshot.error.clone()),
        };
        let completed_sub = SubAgent {
            execution: execution.clone(),
            parent_run_id: parent_run_id.clone(),
            agent_id: agent_id.clone(),
            status: completion.status,
            error: completion.error.clone(),
        };
        let summary = handles
            .update_after_completion(&run_id, epoch, completion)
            .await
            .unwrap_or_else(|| persisted_summary(&run_id, &completed_sub));
        if let Err(err) = ctx
            .state_of::<SubAgentState>()
            .runs_insert(run_id.to_string(), completed_sub)
        {
            return state_write_failed(tool_name, err);
        }
        if let Some(err) = mirror_error {
            return tool_error(tool_name, "store_error", err);
        }
        to_tool_result(tool_name, summary)
    }

    async fn refresh(
        &self,
        os: &AgentOs,
        ctx: &ToolCallContext<'_>,
        request: AgentBackendRefreshRequest,
        tool_name: &str,
    ) -> ToolResult {
        let AgentBackendRefreshRequest {
            run_id,
            owner_thread_id,
            persisted,
        } = request;
        let remote = match remote_a2a_run_ref(&run_id, &persisted.execution, || {
            format!("Run '{run_id}' is not a remote delegated run")
        }) {
            Ok(remote) => remote,
            Err(err) => return err.into_tool_result(tool_name),
        };
        let snapshot = match fetch_a2a_task_snapshot(&self.target, &remote.remote_run_id).await {
            Ok(snapshot) => snapshot,
            Err(err) => return tool_error(tool_name, "remote_error", err),
        };
        let mirror_thread_id = remote
            .mirror_thread_id
            .clone()
            .unwrap_or_else(|| remote_mirror_thread_id(&run_id));
        let mirror_error = if snapshot.done {
            sync_remote_snapshot_to_mirror_thread(
                os,
                &mirror_thread_id,
                Some(&owner_thread_id),
                &run_id,
                persisted.parent_run_id.as_deref(),
                &snapshot,
            )
            .await
            .err()
        } else {
            None
        };
        let refreshed = SubAgent {
            execution: persisted
                .execution
                .clone()
                .with_mirror_thread_id(mirror_thread_id),
            status: if mirror_error.is_some() {
                SubAgentStatus::Failed
            } else {
                snapshot.status
            },
            error: mirror_error.clone().or(snapshot.error.clone()),
            ..persisted
        };
        if let Err(err) = ctx
            .state_of::<SubAgentState>()
            .runs_insert(run_id.to_string(), refreshed.clone())
        {
            return state_write_failed(tool_name, err);
        }
        if let Some(err) = mirror_error {
            return tool_error(tool_name, "store_error", err);
        }
        to_tool_result(tool_name, persisted_summary(&run_id, &refreshed))
    }

    async fn stop(&self, run_id: &str, sub: &SubAgent) -> Result<SubAgentSummary, ToolArgError> {
        let remote = remote_a2a_run_ref(run_id, &sub.execution, || {
            format!("Run '{run_id}' is not a remote A2A run")
        })?;
        cancel_a2a_run(&self.target, &remote.remote_run_id)
            .await
            .map_err(|err| ToolArgError::new("remote_error", err))?;
        let snapshot = fetch_a2a_task_snapshot(&self.target, &remote.remote_run_id)
            .await
            .map_err(|err| ToolArgError::new("remote_error", err))?;
        let status = if snapshot.done {
            snapshot.status
        } else {
            SubAgentStatus::Running
        };
        let error = if snapshot.done {
            snapshot.error
        } else {
            Some("remote cancellation requested".to_string())
        };
        Ok(SubAgentSummary {
            run_id: run_id.to_string(),
            agent_id: sub.agent_id.clone(),
            status,
            error,
        })
    }

    async fn sync_output(
        &self,
        os: &AgentOs,
        ctx: &ToolCallContext<'_>,
        request: AgentBackendOutputSyncRequest,
    ) -> Result<(), ToolArgError> {
        let AgentBackendOutputSyncRequest {
            run_id,
            owner_thread_id,
            thread_id,
            sub,
        } = request;
        let remote = remote_a2a_run_ref(&run_id, &sub.execution, || {
            format!("Run '{run_id}' is not a remote delegated run")
        })?;
        let snapshot = fetch_a2a_task_snapshot(&self.target, &remote.remote_run_id)
            .await
            .map_err(|err| ToolArgError::new("remote_error", err))?;
        if !snapshot.done {
            return Ok(());
        }

        sync_remote_snapshot_to_mirror_thread(
            os,
            &thread_id,
            owner_thread_id.as_deref(),
            &run_id,
            sub.parent_run_id.as_deref(),
            &snapshot,
        )
        .await
        .map_err(|err| ToolArgError::new("store_error", err))?;

        let updated = SubAgent {
            execution: sub.execution.clone().with_mirror_thread_id(thread_id),
            ..sub
        };
        ctx.state_of::<SubAgentState>()
            .runs_insert(run_id, updated)
            .map_err(|err| {
                ToolArgError::new(
                    "state_error",
                    format!("failed to persist sub-agent state: {err}"),
                )
            })?;
        Ok(())
    }
}

pub(super) fn resolve_backend_for_target(target: &ResolvedAgent) -> Box<dyn AgentBackend> {
    match &target.binding {
        AgentBinding::Local => Box::new(LocalAgentBackend),
        AgentBinding::A2a(binding) => Box::new(A2aAgentBackend::new(
            target.descriptor.id.clone(),
            binding.clone(),
        )),
    }
}

pub(super) fn resolve_backend_for_execution(
    catalog: Option<&dyn AgentCatalog>,
    execution: &SubAgentExecutionRef,
) -> Result<Box<dyn AgentBackend>, ToolArgError> {
    match execution {
        SubAgentExecutionRef::Local { .. } => Ok(Box::new(LocalAgentBackend)),
        SubAgentExecutionRef::Remote {
            protocol: SubAgentRemoteProtocol::A2a,
            target_id,
            ..
        } => {
            let Some(catalog) = catalog else {
                return Err(ToolArgError::new(
                    "invalid_state",
                    "remote agent backend resolution requires an AgentOs-bound catalog",
                ));
            };
            let target = resolve_remote_a2a_target(catalog, target_id)?;
            Ok(Box::new(A2aAgentBackend::new(target_id.clone(), target)))
        }
    }
}
