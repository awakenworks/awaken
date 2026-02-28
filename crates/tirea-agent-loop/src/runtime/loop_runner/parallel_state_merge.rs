use super::AgentLoopError;
use crate::contracts::runtime::plugin::phase::{
    reduce_state_actions, AnyStateAction, CommutativeAction,
};
use crate::contracts::runtime::ToolExecutionResult;
use serde_json::Value;
use tirea_state::{apply_patch, Patch, PatchExt, TrackedPatch};

const COMMUTATIVE_PATCH_SOURCE: &str = "agent_loop:commutative";

struct ToolPatchBatch<'a> {
    call_id: &'a str,
    execution_patch: Option<&'a TrackedPatch>,
    pending_patches: &'a [TrackedPatch],
}

impl<'a> ToolPatchBatch<'a> {
    fn new(result: &'a ToolExecutionResult) -> Self {
        Self {
            call_id: &result.execution.call.id,
            execution_patch: result.execution.patch.as_ref(),
            pending_patches: &result.pending_patches,
        }
    }

    fn patches(&self) -> impl Iterator<Item = &'a TrackedPatch> + 'a {
        self.execution_patch
            .into_iter()
            .chain(self.pending_patches.iter())
    }

    fn is_empty(&self) -> bool {
        self.execution_patch.is_none() && self.pending_patches.is_empty()
    }
}

struct CommutativeActionItem<'a> {
    call_id: &'a str,
    action: &'a CommutativeAction,
}

fn commutative_action_path(action: &CommutativeAction) -> &str {
    match action {
        CommutativeAction::CounterAdd { path, .. }
        | CommutativeAction::SetUnion { path, .. }
        | CommutativeAction::MaxI64 { path, .. }
        | CommutativeAction::EnableFlag { path, .. } => path,
    }
}

fn actions_share_path_and_scope(left: &CommutativeAction, right: &CommutativeAction) -> bool {
    commutative_action_path(left) == commutative_action_path(right) && left.scope() == right.scope()
}

fn collect_commutative_action_items(
    results: &[ToolExecutionResult],
) -> Vec<CommutativeActionItem<'_>> {
    results
        .iter()
        .flat_map(|result| {
            result
                .commutative_state_actions
                .iter()
                .map(move |action| CommutativeActionItem {
                    call_id: &result.execution.call.id,
                    action,
                })
        })
        .collect()
}

fn merge_commutative_actions(
    actions: &[CommutativeActionItem<'_>],
    check_conflicts: bool,
) -> Result<Vec<CommutativeAction>, AgentLoopError> {
    let mut merged: Vec<(CommutativeAction, &str)> = Vec::new();

    for item in actions {
        if let Some((existing, _)) = merged
            .iter_mut()
            .find(|(existing, _)| existing.merge(item.action).is_some())
        {
            *existing = existing
                .merge(item.action)
                .expect("merge compatibility checked before update");
            continue;
        }

        if check_conflicts {
            if let Some((existing, existing_call_id)) = merged.iter().find(|(existing, _)| {
                actions_share_path_and_scope(existing, item.action)
                    && existing.label() != item.action.label()
            }) {
                return Err(AgentLoopError::StateError(format!(
                    "incompatible commutative actions between '{}' ({}) and '{}' ({}) at {}",
                    existing_call_id,
                    existing.label(),
                    item.call_id,
                    item.action.label(),
                    commutative_action_path(item.action)
                )));
            }
        }

        merged.push((item.action.clone(), item.call_id));
    }

    Ok(merged.into_iter().map(|(action, _)| action).collect())
}

fn reduce_commutative_actions_to_patch(
    base_snapshot: &Value,
    actions: Vec<CommutativeAction>,
) -> Result<Option<TrackedPatch>, AgentLoopError> {
    if actions.is_empty() {
        return Ok(None);
    }

    let tracked = reduce_state_actions(
        actions
            .into_iter()
            .map(AnyStateAction::Commutative)
            .collect(),
        base_snapshot,
        COMMUTATIVE_PATCH_SOURCE,
    )
    .map_err(|e| {
        AgentLoopError::StateError(format!("failed to reduce commutative state actions: {e}"))
    })?;

    let mut merged_patch = Patch::new();
    for tracked in tracked {
        merged_patch.extend(tracked.patch().clone());
    }

    if merged_patch.is_empty() {
        Ok(None)
    } else {
        Ok(Some(
            TrackedPatch::new(merged_patch).with_source(COMMUTATIVE_PATCH_SOURCE),
        ))
    }
}

fn validate_parallel_state_patch_conflicts(
    batches: &[ToolPatchBatch<'_>],
) -> Result<(), AgentLoopError> {
    for (left_idx, left) in batches.iter().enumerate() {
        if left.is_empty() {
            continue;
        }
        for right in batches.iter().skip(left_idx + 1) {
            if right.is_empty() {
                continue;
            }
            for left_patch in left.patches() {
                for right_patch in right.patches() {
                    let conflicts = left_patch.patch().conflicts_with(right_patch.patch());
                    if let Some(conflict) = conflicts.first() {
                        return Err(AgentLoopError::StateError(format!(
                            "conflicting parallel state patches between '{}' and '{}' at {}",
                            left.call_id, right.call_id, conflict.path
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

fn validate_commutative_patch_conflicts(
    batches: &[ToolPatchBatch<'_>],
    commutative_patch: &TrackedPatch,
) -> Result<(), AgentLoopError> {
    for batch in batches {
        for patch in batch.patches() {
            let conflicts = patch.patch().conflicts_with(commutative_patch.patch());
            if let Some(conflict) = conflicts.first() {
                return Err(AgentLoopError::StateError(format!(
                    "conflicting parallel state patches between '{}' and 'commutative_actions' at {}",
                    batch.call_id, conflict.path
                )));
            }
        }
    }
    Ok(())
}

fn collect_execution_patches(results: &[ToolExecutionResult]) -> Vec<TrackedPatch> {
    results
        .iter()
        .filter_map(|result| result.execution.patch.clone())
        .collect()
}

fn merge_pending_patches(results: &[ToolExecutionResult]) -> Option<TrackedPatch> {
    let mut merged_pending_patch = Patch::new();
    for result in results {
        for pending in &result.pending_patches {
            merged_pending_patch.extend(pending.patch().clone());
        }
    }
    if merged_pending_patch.is_empty() {
        None
    } else {
        Some(TrackedPatch::new(merged_pending_patch).with_source("agent_loop"))
    }
}

fn merge_sequential_state_patches(
    base_snapshot: &Value,
    results: &[ToolExecutionResult],
) -> Result<Vec<TrackedPatch>, AgentLoopError> {
    let mut rolling = base_snapshot.clone();
    let mut patches = Vec::new();

    for result in results {
        if let Some(execution_patch) = result.execution.patch.clone() {
            rolling = apply_patch(&rolling, execution_patch.patch()).map_err(|e| {
                AgentLoopError::StateError(format!(
                    "failed to apply execution patch for call '{}': {}",
                    result.execution.call.id, e
                ))
            })?;
            patches.push(execution_patch);
        }

        for pending in &result.pending_patches {
            rolling = apply_patch(&rolling, pending.patch()).map_err(|e| {
                AgentLoopError::StateError(format!(
                    "failed to apply pending patch for call '{}': {}",
                    result.execution.call.id, e
                ))
            })?;
            patches.push(pending.clone());
        }

        if let Some(commutative_patch) =
            reduce_commutative_actions_to_patch(&rolling, result.commutative_state_actions.clone())?
        {
            rolling = apply_patch(&rolling, commutative_patch.patch()).map_err(|e| {
                AgentLoopError::StateError(format!(
                    "failed to apply commutative patch for call '{}': {}",
                    result.execution.call.id, e
                ))
            })?;
            patches.push(commutative_patch);
        }
    }

    Ok(patches)
}

/// Merge state patch outputs from parallel tool/plugin execution.
///
/// - execution patches are preserved as independent tracked patches
/// - pending plugin patches are flattened into one tracked patch
/// - deferred commutative actions are merged/reduced into one tracked patch
/// - optional cross-call conflict detection runs before merge
pub(super) fn merge_parallel_state_patches(
    base_snapshot: &Value,
    results: &[ToolExecutionResult],
    check_conflicts: bool,
) -> Result<Vec<TrackedPatch>, AgentLoopError> {
    if !check_conflicts {
        return merge_sequential_state_patches(base_snapshot, results);
    }

    let batches: Vec<ToolPatchBatch<'_>> = results.iter().map(ToolPatchBatch::new).collect();
    let commutative_actions =
        merge_commutative_actions(&collect_commutative_action_items(results), check_conflicts)?;
    let commutative_patch =
        reduce_commutative_actions_to_patch(base_snapshot, commutative_actions)?;

    validate_parallel_state_patch_conflicts(&batches)?;
    if let Some(ref patch) = commutative_patch {
        validate_commutative_patch_conflicts(&batches, patch)?;
    }

    let mut patches = collect_execution_patches(results);
    if let Some(pending_patch) = merge_pending_patches(results) {
        patches.push(pending_patch);
    }
    if let Some(commutative_patch) = commutative_patch {
        patches.push(commutative_patch);
    }

    Ok(patches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::runtime::plugin::phase::CommutativeAction;
    use crate::contracts::runtime::tool_call::ToolResult;
    use crate::contracts::runtime::{ToolCallOutcome, ToolExecution, ToolExecutionResult};
    use crate::contracts::thread::ToolCall;
    use serde_json::json;
    use tirea_state::{apply_patch, Op, Patch};

    fn result_with(
        call_id: &str,
        execution_patch: Option<TrackedPatch>,
        pending_patches: Vec<TrackedPatch>,
        commutative_state_actions: Vec<CommutativeAction>,
    ) -> ToolExecutionResult {
        ToolExecutionResult {
            execution: ToolExecution {
                call: ToolCall::new(call_id, "echo", json!({})),
                result: ToolResult::success("echo", json!({})),
                patch: execution_patch,
            },
            outcome: ToolCallOutcome::Succeeded,
            suspended_call: None,
            reminders: Vec::new(),
            user_messages: Vec::new(),
            pending_patches,
            commutative_state_actions,
        }
    }

    fn apply_all(base: &Value, patches: &[TrackedPatch]) -> Value {
        let mut state = base.clone();
        for patch in patches {
            state = apply_patch(&state, patch.patch()).expect("patch should apply");
        }
        state
    }

    #[test]
    fn merge_parallel_state_patches_preserves_existing_shape() {
        let left_exec =
            TrackedPatch::new(Patch::new().with_op(Op::set(tirea_state::path!("alpha"), json!(1))))
                .with_source("tool:left");
        let right_exec =
            TrackedPatch::new(Patch::new().with_op(Op::set(tirea_state::path!("beta"), json!(2))))
                .with_source("tool:right");
        let pending_left = TrackedPatch::new(
            Patch::new().with_op(Op::set(tirea_state::path!("pending", "left"), json!(3))),
        )
        .with_source("agent:left");
        let pending_right = TrackedPatch::new(
            Patch::new().with_op(Op::set(tirea_state::path!("pending", "right"), json!(4))),
        )
        .with_source("agent:right");

        let patches = merge_parallel_state_patches(
            &json!({}),
            &[
                result_with(
                    "call_left",
                    Some(left_exec.clone()),
                    vec![pending_left],
                    Vec::new(),
                ),
                result_with(
                    "call_right",
                    Some(right_exec.clone()),
                    vec![pending_right],
                    Vec::new(),
                ),
            ],
            true,
        )
        .expect("merge should succeed");

        assert_eq!(patches.len(), 3);
        assert_eq!(patches[0].source.as_deref(), left_exec.source.as_deref());
        assert_eq!(patches[1].source.as_deref(), right_exec.source.as_deref());
        assert_eq!(patches[2].source.as_deref(), Some("agent_loop"));
    }

    #[test]
    fn merge_parallel_state_patches_rejects_conflicts_when_enabled() {
        let left = result_with(
            "call_left",
            Some(TrackedPatch::new(
                Patch::new().with_op(Op::set(tirea_state::path!("shared"), json!(1))),
            )),
            Vec::new(),
            Vec::new(),
        );
        let right = result_with(
            "call_right",
            Some(TrackedPatch::new(
                Patch::new().with_op(Op::set(tirea_state::path!("shared"), json!(2))),
            )),
            Vec::new(),
            Vec::new(),
        );

        let err = merge_parallel_state_patches(&json!({}), &[left, right], true)
            .expect_err("must conflict");
        match err {
            AgentLoopError::StateError(message) => {
                assert!(message.contains("call_left"));
                assert!(message.contains("call_right"));
                assert!(message.contains("shared"));
            }
            other => panic!("expected state error, got {other:?}"),
        }
    }

    #[test]
    fn merge_parallel_state_patches_skips_conflict_check_when_disabled() {
        let left = result_with(
            "call_left",
            Some(TrackedPatch::new(
                Patch::new().with_op(Op::set(tirea_state::path!("shared"), json!(1))),
            )),
            Vec::new(),
            Vec::new(),
        );
        let right = result_with(
            "call_right",
            Some(TrackedPatch::new(
                Patch::new().with_op(Op::set(tirea_state::path!("shared"), json!(2))),
            )),
            Vec::new(),
            Vec::new(),
        );

        let patches = merge_parallel_state_patches(&json!({}), &[left, right], false)
            .expect("merge should succeed");
        assert_eq!(patches.len(), 2);
    }

    #[test]
    fn merge_parallel_state_patches_sequential_mode_preserves_result_order_for_commutative() {
        let left = result_with(
            "call_left",
            Some(TrackedPatch::new(
                Patch::new().with_op(Op::set(tirea_state::path!("counter"), json!(10))),
            )),
            Vec::new(),
            vec![CommutativeAction::CounterAdd {
                path: "counter".to_string(),
                delta: 1,
                scope: crate::contracts::runtime::plugin::phase::StateScope::Run,
            }],
        );
        let right = result_with(
            "call_right",
            Some(TrackedPatch::new(
                Patch::new().with_op(Op::set(tirea_state::path!("counter"), json!(100))),
            )),
            Vec::new(),
            Vec::new(),
        );

        let patches = merge_parallel_state_patches(&json!({"counter": 0}), &[left, right], false)
            .expect("merge should succeed");
        let merged = apply_all(&json!({"counter": 0}), &patches);
        assert_eq!(merged["counter"], json!(100));
    }

    #[test]
    fn merge_parallel_state_patches_merges_commutative_actions() {
        let base = json!({"metrics": {"tokens": 10}});
        let left = result_with(
            "call_left",
            None,
            Vec::new(),
            vec![CommutativeAction::CounterAdd {
                path: "metrics.tokens".to_string(),
                delta: 2,
                scope: crate::contracts::runtime::plugin::phase::StateScope::Run,
            }],
        );
        let right = result_with(
            "call_right",
            None,
            Vec::new(),
            vec![CommutativeAction::CounterAdd {
                path: "metrics.tokens".to_string(),
                delta: 3,
                scope: crate::contracts::runtime::plugin::phase::StateScope::Run,
            }],
        );

        let patches = merge_parallel_state_patches(&base, &[left, right], true)
            .expect("merge should succeed");
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].source.as_deref(), Some(COMMUTATIVE_PATCH_SOURCE));

        let merged = apply_all(&base, &patches);
        assert_eq!(merged["metrics"]["tokens"], json!(15));
    }

    #[test]
    fn merge_parallel_state_patches_rejects_incompatible_commutative_actions_when_enabled() {
        let left = result_with(
            "call_left",
            None,
            Vec::new(),
            vec![CommutativeAction::CounterAdd {
                path: "shared.metric".to_string(),
                delta: 1,
                scope: crate::contracts::runtime::plugin::phase::StateScope::Run,
            }],
        );
        let right = result_with(
            "call_right",
            None,
            Vec::new(),
            vec![CommutativeAction::MaxI64 {
                path: "shared.metric".to_string(),
                value: 5,
                scope: crate::contracts::runtime::plugin::phase::StateScope::Run,
            }],
        );

        let err = merge_parallel_state_patches(&json!({}), &[left, right], true)
            .expect_err("must reject incompatible commutative actions");
        match err {
            AgentLoopError::StateError(message) => {
                assert!(message.contains("call_left"));
                assert!(message.contains("call_right"));
                assert!(message.contains("shared.metric"));
            }
            other => panic!("expected state error, got {other:?}"),
        }
    }

    #[test]
    fn merge_parallel_state_patches_rejects_conflict_between_patch_and_commutative_action() {
        let left = result_with(
            "call_left",
            Some(TrackedPatch::new(Patch::new().with_op(Op::set(
                tirea_state::path!("shared", "counter"),
                json!(1),
            )))),
            Vec::new(),
            Vec::new(),
        );
        let right = result_with(
            "call_right",
            None,
            Vec::new(),
            vec![CommutativeAction::CounterAdd {
                path: "shared.counter".to_string(),
                delta: 1,
                scope: crate::contracts::runtime::plugin::phase::StateScope::Run,
            }],
        );

        let err =
            merge_parallel_state_patches(&json!({"shared": {"counter": 0}}), &[left, right], true)
                .expect_err("must reject strict-vs-commutative conflict");
        match err {
            AgentLoopError::StateError(message) => {
                assert!(message.contains("call_left"));
                assert!(message.contains("commutative_actions"));
                assert!(message.contains("shared.counter"));
            }
            other => panic!("expected state error, got {other:?}"),
        }
    }
}
