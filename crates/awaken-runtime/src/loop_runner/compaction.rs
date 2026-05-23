//! Spawn-side wiring for non-blocking context compaction.
//!
//! `maybe_spawn_compaction` is called from the inference phase. When the
//! agent is configured with both a context summarizer and a background
//! task manager, and an in-flight compaction is not already running, it
//! plans the compaction and offloads the LLM summarization to a
//! background task. The completion event flows back via the inbox where
//! [`crate::context::try_consume_compaction_event`] performs the swap.

use std::sync::Arc;

use awaken_contract::contract::inference::ContextWindowPolicy;
use serde_json::json;

use super::step::StepContext;
use crate::context::{
    COMPACTION_COMPLETED_EVENT, COMPACTION_FAILED_EVENT, CompactionInFlight, CompactionStateKey,
    plan_compaction, record_compaction_in_flight,
};
use crate::extensions::background::{TaskParentContext, TaskResult};
use crate::state::MutationBatch;

/// Background task type used for the spawned compaction.
pub const COMPACTION_TASK_TYPE: &str = "context_compaction";

/// Background task description used for telemetry.
const COMPACTION_TASK_DESCRIPTION: &str = "background context compaction";

/// Spawn a background compaction pass when the conditions are met.
///
/// Returns `true` when a new background task was queued, `false` when the
/// call was a no-op (no manager, no summarizer, no thread id, already
/// compacting, or no useful boundary). Never blocks on the LLM call —
/// the summarization runs in `tokio::spawn` and signals back through the
/// owner inbox.
pub(super) async fn maybe_spawn_compaction(
    ctx: &mut StepContext<'_>,
    policy: &ContextWindowPolicy,
) -> bool {
    let Some(manager) = ctx.agent.background_manager.clone() else {
        return false;
    };
    let Some(summarizer) = ctx.agent.context_summarizer.clone() else {
        return false;
    };
    let Some(thread_id) = ctx.run_identity.thread_id_opt() else {
        return false;
    };
    let owner_thread_id = thread_id.to_string();

    let store = ctx.runtime.store();
    if store
        .read::<CompactionStateKey>()
        .is_some_and(|s| s.is_compacting())
    {
        return false;
    }

    let Some(plan) = plan_compaction(ctx.messages, policy) else {
        return false;
    };

    let executor = Arc::clone(&ctx.agent.llm_executor);
    let plan_for_task = plan.clone();
    let boundary_id_for_state = plan.boundary_message_id.clone();
    let pre_tokens = plan.pre_tokens;

    let task_id = match manager
        .spawn(
            &owner_thread_id,
            COMPACTION_TASK_TYPE,
            None,
            COMPACTION_TASK_DESCRIPTION,
            TaskParentContext::default(),
            move |task_ctx| async move {
                let res = summarizer
                    .summarize(
                        &plan_for_task.transcript,
                        plan_for_task.previous_summary.as_deref(),
                        executor.as_ref(),
                    )
                    .await;
                match res {
                    Ok(summary) => {
                        task_ctx.emit(
                            COMPACTION_COMPLETED_EVENT,
                            json!({
                                "boundary_message_id": plan_for_task.boundary_message_id,
                                "summary": summary,
                                "pre_tokens": pre_tokens,
                            }),
                        );
                        TaskResult::Success(serde_json::Value::Null)
                    }
                    Err(error) => {
                        let error_text = error.to_string();
                        task_ctx.emit(
                            COMPACTION_FAILED_EVENT,
                            json!({
                                "boundary_message_id": plan_for_task.boundary_message_id,
                                "error": error_text,
                            }),
                        );
                        TaskResult::Failed(error.to_string())
                    }
                }
            },
        )
        .await
    {
        Ok(id) => id,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "failed to spawn background compaction task; skipping this round"
            );
            return false;
        }
    };

    let mut batch = MutationBatch::new();
    batch.update::<CompactionStateKey>(record_compaction_in_flight(CompactionInFlight {
        task_id: task_id.clone(),
        boundary_message_id: boundary_id_for_state,
        started_at_ms: now_ms(),
    }));
    if let Err(error) = store.commit(batch) {
        tracing::warn!(
            error = %error,
            task_id = %task_id,
            "failed to record CompactionInFlight; another spawn may race"
        );
    }
    true
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "compaction_tests.rs"]
mod tests;
