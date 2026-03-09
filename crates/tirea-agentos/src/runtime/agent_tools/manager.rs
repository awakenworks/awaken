use super::*;

type SubAgentProgressReporter<'a> = dyn Fn(crate::contracts::runtime::tool_call::ToolCallProgressUpdate) -> tirea_state::TireaResult<()>
    + Send
    + Sync
    + 'a;

#[derive(Debug, Clone)]
pub struct SubAgentSummary {
    pub run_id: String,
    pub agent_id: String,
    pub status: SubAgentStatus,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct SubAgentHandle {
    pub(super) epoch: u64,
    pub(super) owner_thread_id: String,
    pub(super) child_thread_id: String,
    pub(super) agent_id: String,
    pub(super) parent_run_id: Option<String>,
    pub(super) status: SubAgentStatus,
    pub(super) error: Option<String>,
    pub(super) cancellation_token: Option<RunCancellationToken>,
    pub(super) run_cancellation_requested: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SubAgentHandleTable {
    handles: Arc<Mutex<HashMap<String, SubAgentHandle>>>,
}

impl SubAgentHandleTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn get_owned_summary(
        &self,
        owner_thread_id: &str,
        run_id: &str,
    ) -> Option<SubAgentSummary> {
        let handles = self.handles.lock().await;
        let handle = handles.get(run_id)?;
        if handle.owner_thread_id != owner_thread_id {
            return None;
        }
        Some(SubAgentSummary {
            run_id: run_id.to_string(),
            agent_id: handle.agent_id.clone(),
            status: handle.status,
            error: handle.error.clone(),
        })
    }

    pub async fn running_or_stopped_for_owner(
        &self,
        owner_thread_id: &str,
    ) -> Vec<SubAgentSummary> {
        let handles = self.handles.lock().await;
        let mut out: Vec<SubAgentSummary> = handles
            .iter()
            .filter_map(|(run_id, handle)| {
                if handle.owner_thread_id != owner_thread_id {
                    return None;
                }
                match handle.status {
                    SubAgentStatus::Running | SubAgentStatus::Stopped => Some(SubAgentSummary {
                        run_id: run_id.clone(),
                        agent_id: handle.agent_id.clone(),
                        status: handle.status,
                        error: handle.error.clone(),
                    }),
                    _ => None,
                }
            })
            .collect();
        out.sort_by(|a, b| a.run_id.cmp(&b.run_id));
        out
    }

    pub async fn contains(&self, run_id: &str) -> bool {
        self.handles.lock().await.contains_key(run_id)
    }

    pub async fn stop_owned_tree(
        &self,
        owner_thread_id: &str,
        run_id: &str,
    ) -> Result<Vec<SubAgentSummary>, String> {
        let mut handles = self.handles.lock().await;
        let Some(root_status) = handles.get(run_id).map(|h| h.status) else {
            return Err(format!("Unknown run_id: {run_id}"));
        };
        if handles
            .get(run_id)
            .is_some_and(|h| h.owner_thread_id != owner_thread_id)
        {
            return Err(format!("Unknown run_id: {run_id}"));
        }

        let run_ids = collect_descendant_run_ids_by_parent(&handles, owner_thread_id, run_id, true);
        if run_ids.is_empty() {
            return Err(format!(
                "Run '{run_id}' is not running (current status: {})",
                root_status.as_str()
            ));
        }

        let mut stopped = false;
        let mut out = Vec::with_capacity(run_ids.len());
        for id in run_ids {
            if let Some(handle) = handles.get_mut(&id) {
                if handle.status == SubAgentStatus::Running {
                    handle.run_cancellation_requested = true;
                    handle.status = SubAgentStatus::Stopped;
                    stopped = true;
                    if let Some(token) = handle.cancellation_token.take() {
                        token.cancel();
                    }
                }
                out.push(SubAgentSummary {
                    run_id: id,
                    agent_id: handle.agent_id.clone(),
                    status: handle.status,
                    error: handle.error.clone(),
                });
            }
        }

        if stopped {
            return Ok(out);
        }

        Err(format!(
            "Run '{run_id}' is not running (current status: {})",
            root_status.as_str()
        ))
    }

    pub(super) async fn put_running(
        &self,
        run_id: &str,
        owner_thread_id: String,
        child_thread_id: String,
        agent_id: String,
        parent_run_id: Option<String>,
        cancellation_token: Option<RunCancellationToken>,
    ) -> u64 {
        let mut handles = self.handles.lock().await;
        let epoch = handles.get(run_id).map(|h| h.epoch + 1).unwrap_or(1);
        handles.insert(
            run_id.to_string(),
            SubAgentHandle {
                epoch,
                owner_thread_id,
                child_thread_id,
                agent_id,
                parent_run_id,
                status: SubAgentStatus::Running,
                error: None,
                run_cancellation_requested: false,
                cancellation_token,
            },
        );
        epoch
    }

    pub(super) async fn remove_if_epoch(&self, run_id: &str, epoch: u64) -> bool {
        let mut handles = self.handles.lock().await;
        if handles.get(run_id).is_some_and(|h| h.epoch == epoch) {
            handles.remove(run_id);
            return true;
        }
        false
    }

    pub(super) async fn update_after_completion(
        &self,
        run_id: &str,
        epoch: u64,
        completion: SubAgentCompletion,
    ) -> Option<SubAgentSummary> {
        let mut handles = self.handles.lock().await;
        let handle = handles.get_mut(run_id)?;
        if handle.epoch != epoch {
            return None;
        }
        handle.error = completion.error;
        handle.status = if handle.run_cancellation_requested {
            SubAgentStatus::Stopped
        } else {
            completion.status
        };
        handle.cancellation_token = None;

        Some(SubAgentSummary {
            run_id: run_id.to_string(),
            agent_id: handle.agent_id.clone(),
            status: handle.status,
            error: handle.error.clone(),
        })
    }

    pub(super) async fn handle_for_resume(
        &self,
        owner_thread_id: &str,
        run_id: &str,
    ) -> Result<SubAgentHandle, String> {
        let handles = self.handles.lock().await;
        let Some(handle) = handles.get(run_id) else {
            return Err(format!("Unknown run_id: {run_id}"));
        };
        if handle.owner_thread_id != owner_thread_id {
            return Err(format!("Unknown run_id: {run_id}"));
        }
        Ok(handle.clone())
    }
}

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

fn bind_parent_tool_call_scope(
    run_config: &mut crate::contracts::RunConfig,
    parent_tool_call_id: Option<&str>,
) -> Result<(), crate::contracts::RunConfigError> {
    let Some(parent_tool_call_id) = parent_tool_call_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
    else {
        return Ok(());
    };
    run_config.set(
        SCOPE_PARENT_TOOL_CALL_ID_KEY,
        parent_tool_call_id.to_string(),
    )
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
    if let Err(e) =
        bind_parent_tool_call_scope(&mut resolved.run_config, parent_tool_call_id.as_deref())
    {
        return SubAgentCompletion {
            status: SubAgentStatus::Failed,
            error: Some(e.to_string()),
        };
    }

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
    fn bind_parent_tool_call_scope_sets_scope_key_when_present() {
        let mut run_config = crate::contracts::RunConfig::default();
        bind_parent_tool_call_scope(&mut run_config, Some("call_parent_1"))
            .expect("set parent tool call id");
        assert_eq!(
            run_config
                .value(SCOPE_PARENT_TOOL_CALL_ID_KEY)
                .and_then(serde_json::Value::as_str),
            Some("call_parent_1")
        );
    }

    #[test]
    fn bind_parent_tool_call_scope_ignores_blank_values() {
        let mut run_config = crate::contracts::RunConfig::default();
        bind_parent_tool_call_scope(&mut run_config, Some("   ")).expect("ignore blank id");
        assert!(run_config.value(SCOPE_PARENT_TOOL_CALL_ID_KEY).is_none());
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

pub(super) fn collect_descendant_run_ids_by_parent(
    handles: &HashMap<String, SubAgentHandle>,
    owner_thread_id: &str,
    root_run_id: &str,
    include_root: bool,
) -> Vec<String> {
    if handles
        .get(root_run_id)
        .is_none_or(|h| h.owner_thread_id != owner_thread_id)
    {
        return Vec::new();
    }

    let mut children_by_parent: HashMap<String, Vec<String>> = HashMap::new();
    for (run_id, handle) in handles.iter() {
        if handle.owner_thread_id != owner_thread_id {
            continue;
        }
        if let Some(parent_run_id) = &handle.parent_run_id {
            children_by_parent
                .entry(parent_run_id.clone())
                .or_default()
                .push(run_id.clone());
        }
    }
    super::collect_descendant_run_ids(&children_by_parent, root_run_id, include_root)
}
