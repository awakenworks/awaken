use super::*;

type SubAgentProgressReporter<'a> = dyn Fn(crate::contracts::runtime::tool_call::ToolCallProgressUpdate) -> tirea_state::TireaResult<()>
    + Send
    + Sync
    + 'a;

#[derive(Debug)]
pub(super) struct SubAgentCompletion {
    pub(super) status: SubAgentStatus,
    pub(super) error: Option<String>,
}

pub(super) struct SubAgentExecutionRequest {
    pub(super) agent_id: String,
    pub(super) child_thread_id: String,
    pub(super) run_id: String,
    pub(super) parent_run_id: Option<String>,
    pub(super) parent_tool_call_id: Option<String>,
    pub(super) parent_thread_id: String,
    pub(super) messages: Vec<crate::contracts::thread::Message>,
    pub(super) initial_state: Option<serde_json::Value>,
    pub(super) cancellation_token: Option<RunCancellationToken>,
}

pub(super) async fn execute_sub_agent(
    os: AgentOs,
    request: SubAgentExecutionRequest,
    progress_reporter: Option<&SubAgentProgressReporter<'_>>,
) -> SubAgentCompletion {
    let SubAgentExecutionRequest {
        agent_id,
        child_thread_id,
        run_id,
        parent_run_id,
        parent_tool_call_id,
        parent_thread_id,
        messages,
        initial_state,
        cancellation_token,
    } = request;

    let run_request = crate::contracts::io::RunRequest {
        agent_id,
        thread_id: Some(child_thread_id),
        run_id: Some(run_id),
        parent_run_id,
        parent_thread_id: Some(parent_thread_id),
        resource_id: None,
        origin: crate::contracts::storage::RunOrigin::Subagent,
        state: initial_state,
        messages,
        initial_decisions: Vec::new(),
        source_mailbox_entry_id: None,
    };

    let mut resolved = match os.resolve(&run_request.agent_id) {
        Ok(r) => r,
        Err(e) => {
            return SubAgentCompletion {
                status: SubAgentStatus::Failed,
                error: Some(e.to_string()),
            };
        }
    };
    resolved.parent_tool_call_id = parent_tool_call_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToOwned::to_owned);

    let mut prepared = match os.prepare_run(run_request, resolved).await {
        Ok(p) => p,
        Err(e) => {
            return SubAgentCompletion {
                status: SubAgentStatus::Failed,
                error: Some(e.to_string()),
            };
        }
    };

    if let Some(token) = cancellation_token {
        prepared.cancellation_token = Some(token);
    }

    let run_stream = match AgentOs::execute_prepared(prepared) {
        Ok(s) => s,
        Err(e) => {
            return SubAgentCompletion {
                status: SubAgentStatus::Failed,
                error: Some(e.to_string()),
            };
        }
    };

    let (saw_error, termination) =
        collect_sub_agent_terminal_state(run_stream.events, progress_reporter).await;

    if saw_error.is_some() {
        return SubAgentCompletion {
            status: SubAgentStatus::Failed,
            error: saw_error,
        };
    }

    let status = match termination {
        Some(crate::contracts::TerminationReason::Cancelled) => SubAgentStatus::Stopped,
        _ => SubAgentStatus::Completed,
    };

    SubAgentCompletion {
        status,
        error: None,
    }
}

fn is_tool_call_progress_activity(activity_type: &str) -> bool {
    activity_type == crate::contracts::runtime::tool_call::TOOL_CALL_PROGRESS_ACTIVITY_TYPE
        || activity_type == crate::contracts::runtime::tool_call::TOOL_PROGRESS_ACTIVITY_TYPE
        || activity_type == crate::contracts::runtime::tool_call::TOOL_PROGRESS_ACTIVITY_TYPE_LEGACY
}

fn decode_tool_call_progress_snapshot(
    content: &serde_json::Value,
) -> Option<(
    String,
    crate::contracts::runtime::tool_call::ToolCallProgressUpdate,
)> {
    let payload = serde_json::from_value::<
        crate::contracts::runtime::tool_call::ToolCallProgressState,
    >(content.clone())
    .ok()?;
    Some((
        payload.call_id,
        crate::contracts::runtime::tool_call::ToolCallProgressUpdate {
            status: payload.status,
            progress: payload.progress,
            loaded: payload.loaded,
            total: payload.total,
            message: payload.message,
        },
    ))
}

async fn collect_sub_agent_terminal_state<S>(
    mut events: S,
    progress_reporter: Option<&SubAgentProgressReporter<'_>>,
) -> (Option<String>, Option<crate::contracts::TerminationReason>)
where
    S: futures::Stream<Item = AgentEvent> + Unpin,
{
    let mut saw_error: Option<String> = None;
    let mut termination: Option<crate::contracts::TerminationReason> = None;
    let mut seen_child_tool_calls: HashSet<String> = HashSet::new();

    while let Some(ev) = events.next().await {
        match ev {
            AgentEvent::Error { message, .. } => {
                if saw_error.is_none() {
                    saw_error = Some(message);
                }
            }
            AgentEvent::RunFinish {
                termination: reason,
                ..
            } => {
                termination = Some(reason);
            }
            AgentEvent::ToolCallStart { id, .. } => {
                seen_child_tool_calls.insert(id);
            }
            AgentEvent::ActivitySnapshot {
                activity_type,
                content,
                ..
            } if is_tool_call_progress_activity(&activity_type) => {
                let Some(reporter) = progress_reporter else {
                    continue;
                };
                let Some((child_call_id, update)) = decode_tool_call_progress_snapshot(&content)
                else {
                    continue;
                };
                if !seen_child_tool_calls.contains(&child_call_id) {
                    continue;
                }
                let _ = reporter(update);
            }
            _ => {}
        }
    }

    (saw_error, termination)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_tool_call_id_seed_is_trimmed() {
        let trimmed = Some("  call_parent_1  ")
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(ToOwned::to_owned);
        assert_eq!(trimmed.as_deref(), Some("call_parent_1"));
    }

    #[test]
    fn blank_parent_tool_call_id_seed_is_none() {
        let trimmed = Some("   ")
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(ToOwned::to_owned);
        assert!(trimmed.is_none());
    }

    #[tokio::test]
    async fn collect_sub_agent_terminal_state_forwards_child_tool_progress_snapshot() {
        let updates = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink_updates = updates.clone();
        let reporter =
            move |update: crate::contracts::runtime::tool_call::ToolCallProgressUpdate| {
                sink_updates.lock().unwrap().push(update);
                Ok(())
            };

        let payload = crate::contracts::runtime::tool_call::ToolCallProgressState {
            event_type: crate::contracts::runtime::tool_call::TOOL_CALL_PROGRESS_TYPE.to_string(),
            schema: crate::contracts::runtime::tool_call::TOOL_CALL_PROGRESS_SCHEMA.to_string(),
            node_id: "tool_call:child-call-1".to_string(),
            parent_node_id: Some("tool_call:parent-call".to_string()),
            parent_call_id: Some("parent-call".to_string()),
            call_id: "child-call-1".to_string(),
            tool_name: Some("echo".to_string()),
            status: crate::contracts::runtime::tool_call::ToolCallProgressStatus::Running,
            progress: Some(0.5),
            loaded: Some(1.0),
            total: Some(2.0),
            message: Some("half".to_string()),
            run_id: Some("child-run".to_string()),
            parent_run_id: Some("parent-run".to_string()),
            thread_id: Some("child-thread".to_string()),
            updated_at_ms: 1,
        };

        let events = futures::stream::iter(vec![
            AgentEvent::ToolCallStart {
                id: "child-call-1".to_string(),
                name: "echo".to_string(),
            },
            AgentEvent::ActivitySnapshot {
                message_id: "tool_call:child-call-1".to_string(),
                activity_type:
                    crate::contracts::runtime::tool_call::TOOL_CALL_PROGRESS_ACTIVITY_TYPE
                        .to_string(),
                content: serde_json::to_value(payload).expect("serialize payload"),
                replace: Some(true),
            },
            AgentEvent::RunFinish {
                thread_id: "child-thread".to_string(),
                run_id: "child-run".to_string(),
                result: None,
                termination: crate::contracts::TerminationReason::NaturalEnd,
            },
        ]);

        let (saw_error, termination) =
            collect_sub_agent_terminal_state(events, Some(&reporter)).await;
        assert!(saw_error.is_none());
        assert_eq!(
            termination,
            Some(crate::contracts::TerminationReason::NaturalEnd)
        );

        let forwarded = updates.lock().unwrap();
        assert_eq!(forwarded.len(), 1);
        assert_eq!(
            forwarded[0].status,
            crate::contracts::runtime::tool_call::ToolCallProgressStatus::Running
        );
        assert_eq!(forwarded[0].progress, Some(0.5));
        assert_eq!(forwarded[0].message.as_deref(), Some("half"));
    }

    #[tokio::test]
    async fn collect_sub_agent_terminal_state_ignores_unknown_child_progress_nodes() {
        let updates = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink_updates = updates.clone();
        let reporter =
            move |update: crate::contracts::runtime::tool_call::ToolCallProgressUpdate| {
                sink_updates.lock().unwrap().push(update);
                Ok(())
            };

        let payload = crate::contracts::runtime::tool_call::ToolCallProgressState {
            event_type: crate::contracts::runtime::tool_call::TOOL_CALL_PROGRESS_TYPE.to_string(),
            schema: crate::contracts::runtime::tool_call::TOOL_CALL_PROGRESS_SCHEMA.to_string(),
            node_id: "tool_call:grand-child-call".to_string(),
            parent_node_id: Some("tool_call:child-agent-run".to_string()),
            parent_call_id: Some("child-agent-run".to_string()),
            call_id: "grand-child-call".to_string(),
            tool_name: Some("hidden".to_string()),
            status: crate::contracts::runtime::tool_call::ToolCallProgressStatus::Running,
            progress: Some(0.1),
            loaded: None,
            total: None,
            message: Some("nested".to_string()),
            run_id: Some("child-run".to_string()),
            parent_run_id: Some("parent-run".to_string()),
            thread_id: Some("child-thread".to_string()),
            updated_at_ms: 1,
        };

        let events = futures::stream::iter(vec![
            AgentEvent::ActivitySnapshot {
                message_id: "tool_call:grand-child-call".to_string(),
                activity_type:
                    crate::contracts::runtime::tool_call::TOOL_CALL_PROGRESS_ACTIVITY_TYPE
                        .to_string(),
                content: serde_json::to_value(payload).expect("serialize payload"),
                replace: Some(true),
            },
            AgentEvent::RunFinish {
                thread_id: "child-thread".to_string(),
                run_id: "child-run".to_string(),
                result: None,
                termination: crate::contracts::TerminationReason::NaturalEnd,
            },
        ]);

        let (saw_error, termination) =
            collect_sub_agent_terminal_state(events, Some(&reporter)).await;
        assert!(saw_error.is_none());
        assert_eq!(
            termination,
            Some(crate::contracts::TerminationReason::NaturalEnd)
        );
        assert!(updates.lock().unwrap().is_empty());
    }
}
