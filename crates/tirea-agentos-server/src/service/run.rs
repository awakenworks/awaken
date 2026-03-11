use std::sync::Arc;
use tirea_agentos::contracts::storage::{ThreadReader, ThreadStore};
use tirea_agentos::contracts::RunRequest;
use tirea_agentos::contracts::ToolCallDecision;
use tirea_agentos::runtime::{AgentOs, AgentOsRunError, ForwardedDecision, ResolvedRun};
use tirea_contract::storage::RunRecord;
use tirea_contract::RuntimeInput;
use tokio::sync::mpsc;

use crate::transport::runtime_endpoint::RunStarter;

use super::mailbox_service::MailboxService;
use super::ApiError;
use super::EnqueueOptions;

pub async fn current_run_id_for_thread(
    os: &Arc<AgentOs>,
    agent_id: &str,
    thread_id: &str,
    read_store: &dyn ThreadReader,
) -> Result<Option<String>, ApiError> {
    match os.current_run_id_for_thread(agent_id, thread_id).await {
        Ok(found) => Ok(found),
        Err(AgentOsRunError::AgentStateStoreNotConfigured) => {
            let Some(record) = read_store
                .active_run_for_thread(thread_id)
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?
            else {
                return Ok(None);
            };
            if !record.agent_id.is_empty() && record.agent_id != agent_id {
                return Ok(None);
            }
            Ok(Some(record.run_id))
        }
        Err(other) => Err(ApiError::Internal(other.to_string())),
    }
}

async fn try_forward_decisions_by_thread(
    os: &Arc<AgentOs>,
    agent_id: &str,
    thread_id: &str,
    decisions: &[ToolCallDecision],
) -> Option<ForwardedDecision> {
    os.forward_decisions_by_thread(agent_id, thread_id, decisions)
        .await
}

pub async fn forward_dialog_decisions_by_thread(
    os: &Arc<AgentOs>,
    agent_id: &str,
    thread_id: &str,
    has_user_input: bool,
    frontend_run_id: Option<&str>,
    decisions: &[ToolCallDecision],
) -> Result<Option<ForwardedDecision>, ApiError> {
    if has_user_input || decisions.is_empty() {
        return Ok(None);
    }

    if let Some(forwarded) =
        try_forward_decisions_by_thread(os, agent_id, thread_id, decisions).await
    {
        return Ok(Some(forwarded));
    }

    Err(ApiError::BadRequest(format!(
        "no active run found for thread '{thread_id}'{suffix}; cannot apply decisions",
        suffix = frontend_run_id_suffix(frontend_run_id),
    )))
}

fn frontend_run_id_suffix(frontend_run_id: Option<&str>) -> String {
    frontend_run_id
        .map(|run_id| format!(", runId: {run_id}"))
        .unwrap_or_default()
}

pub async fn try_forward_decisions_to_active_run_by_id(
    os: &Arc<AgentOs>,
    read_store: &dyn ThreadReader,
    run_id: &str,
    decisions: Vec<ToolCallDecision>,
) -> Result<ForwardedDecision, ApiError> {
    if decisions.is_empty() {
        return Err(ApiError::BadRequest(
            "decisions cannot be empty".to_string(),
        ));
    }

    if let Some(forwarded) = os.forward_decisions_by_run_id(run_id, &decisions).await {
        return Ok(forwarded);
    }

    Err(match check_run_liveness(read_store, run_id).await? {
        RunLookup::ExistsButInactive => ApiError::BadRequest("run is not active".to_string()),
        RunLookup::NotFound => ApiError::RunNotFound(run_id.to_string()),
    })
}

pub async fn try_cancel_active_run_by_id(
    os: &Arc<AgentOs>,
    run_id: &str,
) -> Result<bool, ApiError> {
    Ok(os.cancel_active_run_by_id(run_id).await)
}

pub fn require_agent_state_store(os: &Arc<AgentOs>) -> Result<Arc<dyn ThreadStore>, ApiError> {
    os.agent_state_store()
        .cloned()
        .ok_or_else(|| ApiError::Internal("agent state store not configured".to_string()))
}

/// Shared HTTP run bootstrap result.
///
/// The run is already started via AgentOS lifecycle API; this payload only
/// adapts it to transport wiring (`RunStarter` + ingress channel).
pub struct PreparedHttpRun {
    pub starter: RunStarter,
    pub thread_id: String,
    pub run_id: String,
    pub ingress_rx: mpsc::UnboundedReceiver<RuntimeInput>,
}

pub async fn start_http_run(
    os: &Arc<AgentOs>,
    resolved: ResolvedRun,
    run_request: RunRequest,
    agent_id: &str,
) -> Result<PreparedHttpRun, ApiError> {
    start_http_run_with_persistence(os, resolved, run_request, agent_id, true).await
}

pub async fn start_http_dialog_run(
    os: &Arc<AgentOs>,
    resolved: ResolvedRun,
    run_request: RunRequest,
    agent_id: &str,
) -> Result<PreparedHttpRun, ApiError> {
    start_http_run_with_persistence(os, resolved, run_request, agent_id, false).await
}

async fn start_http_run_with_persistence(
    os: &Arc<AgentOs>,
    resolved: ResolvedRun,
    run_request: RunRequest,
    agent_id: &str,
    persist_run: bool,
) -> Result<PreparedHttpRun, ApiError> {
    let run_request_for_ingress = run_request.clone();
    let run = os
        .start_active_run_with_persistence(
            agent_id,
            run_request,
            resolved,
            persist_run,
            !persist_run,
        )
        .await
        .map_err(ApiError::from)?;
    let thread_id = run.thread_id.clone();
    let run_id = run.run_id.clone();

    let (ingress_tx, ingress_rx) = mpsc::unbounded_channel::<RuntimeInput>();
    ingress_tx
        .send(RuntimeInput::Run(run_request_for_ingress))
        .expect("ingress channel just created");

    let starter: RunStarter = Box::new(move |_request| Box::pin(async move { Ok(run) }));

    Ok(PreparedHttpRun {
        starter,
        thread_id,
        run_id,
        ingress_rx,
    })
}

/// Load the full [`RunRecord`] for a given run id.
pub async fn load_run_record(
    read_store: &dyn ThreadReader,
    run_id: &str,
) -> Result<Option<RunRecord>, ApiError> {
    read_store
        .load_run(run_id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))
}

/// Resolve the thread id that a run belongs to.
pub async fn resolve_thread_id_from_run(
    read_store: &dyn ThreadReader,
    run_id: &str,
) -> Result<Option<String>, ApiError> {
    Ok(read_store
        .load_run(run_id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .map(|r| r.thread_id))
}

/// Result of checking whether a run is currently active.
pub enum RunLookup {
    ExistsButInactive,
    NotFound,
}

/// After an active-run operation (cancel/forward) fails, check if the run
/// exists in the persistent store. Returns [`RunLookup::ExistsButInactive`]
/// or [`RunLookup::NotFound`].
pub async fn check_run_liveness(
    read_store: &dyn ThreadReader,
    run_id: &str,
) -> Result<RunLookup, ApiError> {
    if read_store
        .load_run(run_id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .is_some()
    {
        Ok(RunLookup::ExistsButInactive)
    } else {
        Ok(RunLookup::NotFound)
    }
}

/// Returns `(thread_id, run_id, entry_id)`.
pub async fn start_background_run(
    mailbox_service: &Arc<MailboxService>,
    agent_id: &str,
    run_request: RunRequest,
    options: EnqueueOptions,
) -> Result<(String, String, String), ApiError> {
    mailbox_service.submit(agent_id, run_request, options).await
}

/// Truncate a stored thread so it includes messages up to and including `message_id`.
pub async fn truncate_thread_at_message(
    os: &Arc<AgentOs>,
    thread_id: &str,
    message_id: &str,
) -> Result<(), ApiError> {
    let store = require_agent_state_store(os)?;
    let mut thread = store
        .load(thread_id)
        .await
        .map_err(|err| ApiError::Internal(err.to_string()))?
        .ok_or_else(|| ApiError::BadRequest("thread not found for regenerate-message".to_string()))?
        .thread;
    let position = thread
        .messages
        .iter()
        .position(|m| m.id.as_deref() == Some(message_id))
        .ok_or_else(|| {
            ApiError::BadRequest("messageId does not reference a stored message".to_string())
        })?;
    thread.messages.truncate(position + 1);
    store
        .save(&thread)
        .await
        .map_err(|err| ApiError::Internal(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_mode_preserves_run_lineage() {
        let request = RunRequest {
            agent_id: "agent".to_string(),
            thread_id: Some("thread-1".to_string()),
            run_id: Some("run-1".to_string()),
            parent_run_id: Some("parent-run-1".to_string()),
            parent_thread_id: Some("parent-thread-1".to_string()),
            resource_id: Some("resource-1".to_string()),
            origin: Default::default(),
            state: None,
            messages: vec![],
            initial_decisions: vec![],
            source_mailbox_entry_id: None,
        };

        let preserved = request;

        assert_eq!(preserved.run_id.as_deref(), Some("run-1"));
        assert_eq!(preserved.parent_run_id.as_deref(), Some("parent-run-1"));
        assert_eq!(
            preserved.parent_thread_id.as_deref(),
            Some("parent-thread-1")
        );
        assert_eq!(preserved.thread_id.as_deref(), Some("thread-1"));
        assert_eq!(preserved.resource_id.as_deref(), Some("resource-1"));
    }
}
