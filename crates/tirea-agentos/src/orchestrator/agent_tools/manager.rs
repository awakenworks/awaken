use super::*;
#[derive(Debug, Clone)]
pub struct AgentRunSummary {
    pub run_id: String,
    pub target_agent_id: String,
    pub status: DelegationStatus,
    pub assistant: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct AgentRunRecord {
    pub(super) epoch: u64,
    pub(super) owner_thread_id: String,
    pub(super) target_agent_id: String,
    pub(super) parent_run_id: Option<String>,
    pub(super) status: DelegationStatus,
    pub(super) thread: crate::contracts::thread::Thread,
    pub(super) assistant: Option<String>,
    pub(super) error: Option<String>,
    pub(super) run_cancellation_requested: bool,
    pub(super) cancellation_token: Option<RunCancellationToken>,
}

#[derive(Debug, Clone, Default)]
pub struct AgentRunManager {
    runs: Arc<Mutex<HashMap<String, AgentRunRecord>>>,
}

impl AgentRunManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn get_owned_summary(
        &self,
        owner_thread_id: &str,
        run_id: &str,
    ) -> Option<AgentRunSummary> {
        let runs = self.runs.lock().await;
        let rec = runs.get(run_id)?;
        if rec.owner_thread_id != owner_thread_id {
            return None;
        }
        Some(AgentRunSummary {
            run_id: run_id.to_string(),
            target_agent_id: rec.target_agent_id.clone(),
            status: rec.status,
            assistant: rec.assistant.clone(),
            error: rec.error.clone(),
        })
    }

    pub async fn running_or_stopped_for_owner(
        &self,
        owner_thread_id: &str,
    ) -> Vec<AgentRunSummary> {
        let runs = self.runs.lock().await;
        let mut out: Vec<AgentRunSummary> = runs
            .iter()
            .filter_map(|(run_id, rec)| {
                if rec.owner_thread_id != owner_thread_id {
                    return None;
                }
                match rec.status {
                    DelegationStatus::Running | DelegationStatus::Stopped => {
                        Some(AgentRunSummary {
                            run_id: run_id.clone(),
                            target_agent_id: rec.target_agent_id.clone(),
                            status: rec.status,
                            assistant: rec.assistant.clone(),
                            error: rec.error.clone(),
                        })
                    }
                    _ => None,
                }
            })
            .collect();
        out.sort_by(|a, b| a.run_id.cmp(&b.run_id));
        out
    }

    pub async fn all_for_owner(&self, owner_thread_id: &str) -> Vec<AgentRunSummary> {
        let runs = self.runs.lock().await;
        let mut out: Vec<AgentRunSummary> = runs
            .iter()
            .filter_map(|(run_id, rec)| {
                if rec.owner_thread_id != owner_thread_id {
                    return None;
                }
                Some(AgentRunSummary {
                    run_id: run_id.clone(),
                    target_agent_id: rec.target_agent_id.clone(),
                    status: rec.status,
                    assistant: rec.assistant.clone(),
                    error: rec.error.clone(),
                })
            })
            .collect();
        out.sort_by(|a, b| a.run_id.cmp(&b.run_id));
        out
    }

    pub(super) async fn owned_record(
        &self,
        owner_thread_id: &str,
        run_id: &str,
    ) -> Option<crate::contracts::thread::Thread> {
        let runs = self.runs.lock().await;
        let rec = runs.get(run_id)?;
        if rec.owner_thread_id != owner_thread_id {
            return None;
        }
        Some(rec.thread.clone())
    }

    pub async fn stop_owned_tree(
        &self,
        owner_thread_id: &str,
        run_id: &str,
    ) -> Result<Vec<AgentRunSummary>, String> {
        let mut runs = self.runs.lock().await;
        let Some(root_status) = runs.get(run_id).map(|r| r.status) else {
            return Err(format!("Unknown run_id: {run_id}"));
        };
        if runs
            .get(run_id)
            .is_some_and(|r| r.owner_thread_id != owner_thread_id)
        {
            return Err(format!("Unknown run_id: {run_id}"));
        }

        let run_ids = collect_descendant_run_ids_by_parent(&runs, owner_thread_id, run_id, true);
        if run_ids.is_empty() {
            return Err(format!(
                "Run '{run_id}' is not running (current status: {})",
                root_status.as_str()
            ));
        }

        let mut stopped = false;
        let mut out = Vec::with_capacity(run_ids.len());
        for id in run_ids {
            if let Some(rec) = runs.get_mut(&id) {
                if rec.status == DelegationStatus::Running {
                    rec.run_cancellation_requested = true;
                    rec.status = DelegationStatus::Stopped;
                    stopped = true;
                    if let Some(token) = rec.cancellation_token.take() {
                        token.cancel();
                    }
                }
                out.push(AgentRunSummary {
                    run_id: id,
                    target_agent_id: rec.target_agent_id.clone(),
                    status: rec.status,
                    assistant: rec.assistant.clone(),
                    error: rec.error.clone(),
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
        target_agent_id: String,
        parent_run_id: Option<String>,
        thread: crate::contracts::thread::Thread,
        cancellation_token: Option<RunCancellationToken>,
    ) -> u64 {
        let mut runs = self.runs.lock().await;
        let epoch = runs.get(run_id).map(|r| r.epoch + 1).unwrap_or(1);
        runs.insert(
            run_id.to_string(),
            AgentRunRecord {
                epoch,
                owner_thread_id,
                target_agent_id,
                parent_run_id,
                status: DelegationStatus::Running,
                thread,
                assistant: None,
                error: None,
                run_cancellation_requested: false,
                cancellation_token,
            },
        );
        epoch
    }

    pub(super) async fn update_after_completion(
        &self,
        run_id: &str,
        epoch: u64,
        completion: AgentRunCompletion,
    ) -> Option<AgentRunSummary> {
        let mut runs = self.runs.lock().await;
        let rec = runs.get_mut(run_id)?;
        if rec.epoch != epoch {
            // Stale completion from a previous generation (e.g. stopped run that was resumed).
            return None;
        }
        rec.thread = completion.thread;
        rec.assistant = completion.assistant;
        rec.error = completion.error;

        // Explicit run-cancellation request wins over terminal status from executor.
        rec.status = if rec.run_cancellation_requested {
            DelegationStatus::Stopped
        } else {
            completion.status
        };
        rec.cancellation_token = None;

        Some(AgentRunSummary {
            run_id: run_id.to_string(),
            target_agent_id: rec.target_agent_id.clone(),
            status: rec.status,
            assistant: rec.assistant.clone(),
            error: rec.error.clone(),
        })
    }

    pub(super) async fn record_for_resume(
        &self,
        owner_thread_id: &str,
        run_id: &str,
    ) -> Result<AgentRunRecord, String> {
        let runs = self.runs.lock().await;
        let Some(rec) = runs.get(run_id) else {
            return Err(format!("Unknown run_id: {run_id}"));
        };
        if rec.owner_thread_id != owner_thread_id {
            return Err(format!("Unknown run_id: {run_id}"));
        }
        Ok(rec.clone())
    }
}

#[derive(Debug)]
pub(super) struct AgentRunCompletion {
    pub(super) thread: crate::contracts::thread::Thread,
    pub(super) status: DelegationStatus,
    pub(super) assistant: Option<String>,
    pub(super) error: Option<String>,
}

fn last_assistant_message(thread: &crate::contracts::thread::Thread) -> Option<String> {
    thread
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| m.content.clone())
}

pub(super) async fn execute_target_agent(
    os: AgentOs,
    target_agent_id: String,
    thread: crate::contracts::thread::Thread,
    cancellation_token: Option<RunCancellationToken>,
) -> AgentRunCompletion {
    let (checkpoint_tx, mut checkpoints) = tokio::sync::mpsc::unbounded_channel();
    let state_committer: Option<Arc<dyn crate::runtime::loop_runner::StateCommitter>> =
        Some(Arc::new(ChannelStateCommitter::new(checkpoint_tx)));
    let mut events = match os.run_stream_with_context(
        &target_agent_id,
        thread.clone(),
        cancellation_token,
        state_committer,
    ) {
        Ok(stream) => stream,
        Err(e) => {
            return AgentRunCompletion {
                thread,
                status: DelegationStatus::Failed,
                assistant: None,
                error: Some(e.to_string()),
            };
        }
    };

    let mut saw_error: Option<String> = None;
    let mut termination: Option<crate::contracts::TerminationReason> = None;
    let mut final_thread = thread.clone();
    let mut checkpoints_open = true;

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

        while let Ok(changeset) = checkpoints.try_recv() {
            changeset.apply_to(&mut final_thread);
        }
    }

    while checkpoints_open {
        match checkpoints.recv().await {
            Some(changeset) => changeset.apply_to(&mut final_thread),
            None => checkpoints_open = false,
        }
    }

    let assistant = last_assistant_message(&final_thread);

    if saw_error.is_some() {
        return AgentRunCompletion {
            thread: final_thread,
            status: DelegationStatus::Failed,
            assistant,
            error: saw_error,
        };
    }

    let status = match termination {
        Some(crate::contracts::TerminationReason::Cancelled) => DelegationStatus::Stopped,
        _ => DelegationStatus::Completed,
    };

    AgentRunCompletion {
        thread: final_thread,
        status,
        assistant,
        error: None,
    }
}

pub(super) fn collect_descendant_run_ids_by_parent(
    runs: &HashMap<String, AgentRunRecord>,
    owner_thread_id: &str,
    root_run_id: &str,
    include_root: bool,
) -> Vec<String> {
    if runs
        .get(root_run_id)
        .is_none_or(|rec| rec.owner_thread_id != owner_thread_id)
    {
        return Vec::new();
    }

    let mut children_by_parent: HashMap<String, Vec<String>> = HashMap::new();
    for (run_id, rec) in runs.iter() {
        if rec.owner_thread_id != owner_thread_id {
            continue;
        }
        if let Some(parent_run_id) = &rec.parent_run_id {
            children_by_parent
                .entry(parent_run_id.clone())
                .or_default()
                .push(run_id.clone());
        }
    }
    super::collect_descendant_run_ids(&children_by_parent, root_run_id, include_root)
}
