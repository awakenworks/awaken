use super::errors::AgentOsRunError;
use super::thread_run;

use crate::contracts::runtime::state::{
    reduce_state_actions, AnyStateAction, ScopeContext, StateScopeRegistry,
};
use crate::contracts::runtime::{RunLifecycleAction, RunLifecycleState, RunStatus};
use crate::contracts::thread::{Message, Thread};
use crate::loop_runtime::loop_runner::AgentLoopError;
use std::sync::Arc;
use tirea_contract::runtime::suspended_calls_from_state;
use tirea_state::{Op, Patch, TrackedPatch};

pub(super) fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().min(u128::from(u64::MAX)) as u64)
}

pub(super) struct ActiveRunCleanupGuard {
    run_id: String,
    registry: Arc<thread_run::ActiveThreadRunRegistry>,
    armed: bool,
}

impl ActiveRunCleanupGuard {
    pub(super) fn new(run_id: String, registry: Arc<thread_run::ActiveThreadRunRegistry>) -> Self {
        Self {
            run_id,
            registry,
            armed: true,
        }
    }

    pub(super) async fn cleanup_now(&mut self) {
        if !self.armed {
            return;
        }
        self.registry.remove_by_run_id(&self.run_id).await;
        self.armed = false;
    }
}

impl Drop for ActiveRunCleanupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let run_id = self.run_id.clone();
        let registry = self.registry.clone();
        tokio::spawn(async move {
            registry.remove_by_run_id(&run_id).await;
        });
    }
}

/// Generate delete patches for all Run-scoped state paths that exist in the
/// current state. This ensures stale run-scoped state from a previous run is
/// cleaned up before the new run begins.
pub(super) fn run_scope_cleanup_patches(
    base_state: &serde_json::Value,
    scope_registry: &StateScopeRegistry,
) -> Vec<TrackedPatch> {
    let paths = scope_registry.run_scoped_paths();
    let mut patches = Vec::new();
    for path in paths {
        let parsed = tirea_state::parse_path(path);
        if tirea_state::get_at_path(base_state, &parsed).is_some() {
            let patch = Patch::with_ops(vec![Op::delete(parsed)]);
            patches.push(TrackedPatch::new(patch).with_source("prepare_run:scope_cleanup"));
        }
    }
    patches
}

pub(super) fn run_lifecycle_running_patch(
    base_state: &serde_json::Value,
    run_id: &str,
) -> Result<TrackedPatch, AgentOsRunError> {
    let updated_at = now_unix_millis();
    let actions = vec![AnyStateAction::new::<RunLifecycleState>(
        RunLifecycleAction::Set {
            id: run_id.to_string(),
            status: RunStatus::Running,
            done_reason: None,
            updated_at,
        },
    )];
    let mut patches = reduce_state_actions(
        actions,
        base_state,
        "agentos_prepare_run",
        &ScopeContext::run(),
    )
    .map_err(|e| AgentOsRunError::Loop(AgentLoopError::StateError(e.to_string())))?;
    let Some(patch) = patches.pop() else {
        return Err(AgentOsRunError::Loop(AgentLoopError::StateError(
            "failed to emit run lifecycle running patch: reducer produced no patch".to_string(),
        )));
    };
    Ok(patch)
}

pub(super) fn set_or_validate_parent_thread_id(
    thread: &mut Thread,
    thread_id: &str,
    requested_parent_thread_id: Option<&str>,
) -> Result<bool, AgentOsRunError> {
    let Some(requested_parent_thread_id) = requested_parent_thread_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(false);
    };

    if let Some(existing) = thread.parent_thread_id.as_deref() {
        if existing != requested_parent_thread_id {
            return Err(AgentOsRunError::Loop(AgentLoopError::StateError(format!(
                "parent_thread_id mismatch for thread '{thread_id}': existing='{existing}', requested='{requested_parent_thread_id}'",
            ))));
        }
        return Ok(false);
    }

    thread.parent_thread_id = Some(requested_parent_thread_id.to_string());
    Ok(true)
}

pub(super) fn request_has_user_input(messages: &[Message]) -> bool {
    messages.iter().any(|message| {
        message.role == crate::contracts::thread::Role::User && !message.content.trim().is_empty()
    })
}

pub(super) fn clear_suspended_tool_call_state(
    state: &serde_json::Value,
) -> Option<serde_json::Value> {
    let suspended = suspended_calls_from_state(state);
    if suspended.is_empty() {
        return None;
    }

    let mut cleaned_state = state.clone();
    let root = cleaned_state.as_object_mut()?;
    let scope_obj = root
        .get_mut("__tool_call_scope")
        .and_then(serde_json::Value::as_object_mut)?;

    let mut removed_any = false;
    for call_id in suspended.keys() {
        if scope_obj.remove(call_id).is_some() {
            removed_any = true;
        }
    }

    if removed_any && scope_obj.is_empty() {
        root.remove("__tool_call_scope");
    }

    removed_any.then_some(cleaned_state)
}
