use super::*;

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

pub(super) async fn execute_sub_agent(
    os: AgentOs,
    agent_id: String,
    child_thread_id: String,
    run_id: String,
    parent_run_id: Option<String>,
    parent_thread_id: String,
    messages: Vec<crate::contracts::thread::Message>,
    initial_state: Option<serde_json::Value>,
    cancellation_token: Option<RunCancellationToken>,
) -> SubAgentCompletion {
    let request = crate::contracts::io::RunRequest {
        agent_id,
        thread_id: Some(child_thread_id),
        run_id: Some(run_id),
        parent_run_id,
        parent_thread_id: Some(parent_thread_id),
        resource_id: None,
        state: initial_state,
        messages,
        initial_decisions: Vec::new(),
    };

    let resolved = match os.resolve(&request.agent_id) {
        Ok(r) => r,
        Err(e) => {
            return SubAgentCompletion {
                status: SubAgentStatus::Failed,
                error: Some(e.to_string()),
            };
        }
    };

    let mut prepared = match os.prepare_run(request, resolved).await {
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

    let mut events = run_stream.events;
    let mut saw_error: Option<String> = None;
    let mut termination: Option<crate::contracts::TerminationReason> = None;

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
            _ => {}
        }
    }

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
