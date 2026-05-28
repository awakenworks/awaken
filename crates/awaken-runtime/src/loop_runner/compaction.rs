//! Spawn-side wiring for non-blocking context compaction.
//!
//! `maybe_spawn_compaction` is called from the inference phase. When the
//! agent is configured with both a context summarizer and a background
//! task manager, and an in-flight compaction is not already running, it
//! plans the compaction and offloads the LLM summarization to a
//! background task. The completion event flows back via the inbox where
//! [`crate::context::try_consume_compaction_event`] performs the swap.

use std::sync::Arc;

use awaken_contract::StateError;
use awaken_contract::contract::inference::ContextWindowPolicy;
use serde_json::json;

use super::step::StepContext;
use crate::context::{
    COMPACTION_COMPLETED_EVENT, COMPACTION_FAILED_EVENT, COMPACTION_SKIP_REASON_MIN_SAVINGS_RATIO,
    COMPACTION_SKIPPED_EVENT, COMPACTION_STARTED_EVENT, CompactionConfigKey,
    CompactionExecutionMode, CompactionInFlight, CompactionStateKey, compaction_savings_ratio_ppm,
    plan_compaction, record_compaction_in_flight, summary_message_tokens,
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
    let compaction_config = ctx
        .agent
        .spec
        .config::<CompactionConfigKey>()
        .unwrap_or_default();
    if compaction_config.execution_mode == CompactionExecutionMode::Off {
        return false;
    }

    let Some(plan) = plan_compaction(ctx.messages, policy) else {
        return false;
    };

    let executor = Arc::clone(&ctx.agent.llm_executor);
    let min_savings_ratio = compaction_config.min_savings_ratio.clamp(0.0, 1.0);
    let min_savings_ratio_ppm = (min_savings_ratio * 1_000_000.0).round() as u32;
    let plan_for_task = plan.clone();
    let boundary_id_for_state = plan.boundary_message_id.clone();
    let pre_tokens = plan.pre_tokens;
    let task_id = manager.reserve_task_id();

    let in_flight = CompactionInFlight {
        task_id: task_id.clone(),
        boundary_message_id: boundary_id_for_state.clone(),
        started_at_ms: now_ms(),
    };
    match reserve_compaction_in_flight(store, in_flight) {
        Ok(true) => {}
        Ok(false) => return false,
        Err(error) => {
            tracing::warn!(
                error = %error,
                task_id = %task_id,
                "failed to reserve CompactionInFlight; skipping background compaction"
            );
            return false;
        }
    }

    if let Err(error) = manager
        .spawn_with_task_id(
            task_id.clone(),
            &owner_thread_id,
            COMPACTION_TASK_TYPE,
            None,
            COMPACTION_TASK_DESCRIPTION,
            TaskParentContext::default(),
            move |task_ctx| async move {
                task_ctx.emit(
                    COMPACTION_STARTED_EVENT,
                    json!({
                        "task_id": task_ctx.task_id.as_str(),
                        "boundary_message_id": plan_for_task.boundary_message_id.as_str(),
                        "pre_tokens": pre_tokens,
                    }),
                );
                let res = summarizer
                    .summarize(
                        &plan_for_task.transcript,
                        plan_for_task.previous_summary.as_deref(),
                        executor.as_ref(),
                    )
                    .await;
                match res {
                    Ok(summary) => {
                        let post_tokens = summary_message_tokens(&summary);
                        let savings_ratio_ppm =
                            compaction_savings_ratio_ppm(pre_tokens, post_tokens);
                        if savings_ratio_ppm < min_savings_ratio_ppm {
                            task_ctx.emit(
                                COMPACTION_SKIPPED_EVENT,
                                json!({
                                    "task_id": task_ctx.task_id.as_str(),
                                    "boundary_message_id": plan_for_task.boundary_message_id.as_str(),
                                    "reason": COMPACTION_SKIP_REASON_MIN_SAVINGS_RATIO,
                                    "pre_tokens": pre_tokens,
                                    "post_tokens": post_tokens,
                                    "savings_ratio_ppm": savings_ratio_ppm,
                                    "min_savings_ratio_ppm": min_savings_ratio_ppm,
                                    "savings_ratio": savings_ratio_ppm as f64 / 1_000_000.0,
                                    "min_savings_ratio": min_savings_ratio,
                                }),
                            );
                            return TaskResult::Success(serde_json::Value::Null);
                        }
                        task_ctx.emit(
                            COMPACTION_COMPLETED_EVENT,
                            json!({
                                "task_id": task_ctx.task_id.as_str(),
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
                                "task_id": task_ctx.task_id.as_str(),
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
        tracing::warn!(
            error = %error,
            "failed to spawn background compaction task; skipping this round"
        );
        clear_in_flight_after_spawn_failure(store, &task_id);
        return false;
    }
    true
}

fn reserve_compaction_in_flight(
    store: &crate::state::StateStore,
    in_flight: CompactionInFlight,
) -> Result<bool, StateError> {
    for _ in 0..8 {
        let snapshot = store.snapshot();
        if snapshot
            .get::<CompactionStateKey>()
            .is_some_and(|state| state.is_compacting())
        {
            return Ok(false);
        }
        let mut batch = MutationBatch::new().with_base_revision(snapshot.revision());
        batch.update::<CompactionStateKey>(record_compaction_in_flight(in_flight.clone()));
        match store.commit(batch) {
            Ok(_) => return Ok(true),
            Err(StateError::RevisionConflict { .. }) => continue,
            Err(error) => return Err(error),
        }
    }
    tracing::warn!(
        task_id = %in_flight.task_id,
        "failed to reserve CompactionInFlight after repeated revision conflicts"
    );
    Ok(false)
}

fn clear_in_flight_after_spawn_failure(store: &crate::state::StateStore, task_id: &str) {
    if store
        .read::<CompactionStateKey>()
        .and_then(|state| state.in_flight)
        .is_none_or(|in_flight| in_flight.task_id != task_id)
    {
        return;
    }
    let mut batch = MutationBatch::new();
    batch.update::<CompactionStateKey>(crate::context::clear_compaction_in_flight());
    if let Err(error) = store.commit(batch) {
        tracing::warn!(
            error = %error,
            task_id = %task_id,
            "failed to clear CompactionInFlight after spawn failure"
        );
    }
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
