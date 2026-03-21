use super::AgentLoopError;
use crate::contracts::runtime::ToolExecutionResult;
use serde_json::Value;
use tirea_state::{
    apply_patch_with_registry, conflicts_with_registry, LatticeRegistry, Patch, TrackedPatch,
};

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

fn validate_parallel_state_patch_conflicts(
    batches: &[ToolPatchBatch<'_>],
    registry: &LatticeRegistry,
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
                    let conflicts =
                        conflicts_with_registry(left_patch.patch(), right_patch.patch(), registry);
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
    registry: &LatticeRegistry,
) -> Result<Vec<TrackedPatch>, AgentLoopError> {
    let mut rolling = base_snapshot.clone();
    let mut patches = Vec::new();

    for result in results {
        if let Some(execution_patch) = result.execution.patch.clone() {
            rolling = apply_patch_with_registry(&rolling, execution_patch.patch(), registry)
                .map_err(|e| {
                    AgentLoopError::StateError(format!(
                        "failed to apply execution patch for call '{}': {}",
                        result.execution.call.id, e
                    ))
                })?;
            patches.push(execution_patch);
        }

        for pending in &result.pending_patches {
            rolling =
                apply_patch_with_registry(&rolling, pending.patch(), registry).map_err(|e| {
                    AgentLoopError::StateError(format!(
                        "failed to apply pending patch for call '{}': {}",
                        result.execution.call.id, e
                    ))
                })?;
            patches.push(pending.clone());
        }
    }

    Ok(patches)
}

/// Merge state patch outputs from parallel tool/plugin execution.
///
/// - execution patches are preserved as independent tracked patches
/// - pending plugin patches are flattened into one tracked patch
/// - optional cross-call conflict detection runs before merge
pub(super) fn merge_parallel_state_patches(
    base_snapshot: &Value,
    results: &[ToolExecutionResult],
    check_conflicts: bool,
    registry: &LatticeRegistry,
) -> Result<Vec<TrackedPatch>, AgentLoopError> {
    if !check_conflicts {
        return merge_sequential_state_patches(base_snapshot, results, registry);
    }

    let batches: Vec<ToolPatchBatch<'_>> = results.iter().map(ToolPatchBatch::new).collect();
    validate_parallel_state_patch_conflicts(&batches, registry)?;

    let mut patches = collect_execution_patches(results);
    if let Some(pending_patch) = merge_pending_patches(results) {
        patches.push(pending_patch);
    }

    Ok(patches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::runtime::tool_call::ToolResult;
    use crate::contracts::runtime::{ToolCallOutcome, ToolExecution, ToolExecutionResult};
    use crate::contracts::thread::ToolCall;
    use serde_json::json;
    use tirea_state::{apply_patch, LatticeRegistry, Op, Patch};

    fn result_with(
        call_id: &str,
        execution_patch: Option<TrackedPatch>,
        pending_patches: Vec<TrackedPatch>,
    ) -> ToolExecutionResult {
        ToolExecutionResult {
            execution: ToolExecution {
                call: ToolCall::new(call_id, "echo", json!({})),
                result: ToolResult::success("echo", json!({})),
                patch: execution_patch,
            },
            outcome: ToolCallOutcome::Succeeded,
            suspended_call: None,
            messages: Vec::new(),
            reminders: Vec::new(),
            user_messages: Vec::new(),
            pending_patches,
            serialized_state_actions: vec![],
        }
    }

    fn empty_registry() -> LatticeRegistry {
        LatticeRegistry::new()
    }

    #[allow(dead_code)]
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
                result_with("call_left", Some(left_exec.clone()), vec![pending_left]),
                result_with("call_right", Some(right_exec.clone()), vec![pending_right]),
            ],
            true,
            &empty_registry(),
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
        );
        let right = result_with(
            "call_right",
            Some(TrackedPatch::new(
                Patch::new().with_op(Op::set(tirea_state::path!("shared"), json!(2))),
            )),
            Vec::new(),
        );

        let err = merge_parallel_state_patches(&json!({}), &[left, right], true, &empty_registry())
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
        );
        let right = result_with(
            "call_right",
            Some(TrackedPatch::new(
                Patch::new().with_op(Op::set(tirea_state::path!("shared"), json!(2))),
            )),
            Vec::new(),
        );

        let patches =
            merge_parallel_state_patches(&json!({}), &[left, right], false, &empty_registry())
                .expect("merge should succeed");
        assert_eq!(patches.len(), 2);
    }
}
