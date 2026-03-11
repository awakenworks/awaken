use super::*;
use crate::composition::InMemoryAgentRegistry;
use crate::contracts::runtime::behavior::ReadOnlyContext;
use crate::contracts::runtime::phase::{ActionSet, BeforeInferenceAction};
use crate::contracts::runtime::phase::{Phase, StepContext};
use crate::contracts::runtime::state::{reduce_state_actions, ScopeContext};
use crate::contracts::runtime::tool_call::{CallerContext, Tool, ToolStatus};
use crate::contracts::runtime::RunIdentity;
use crate::contracts::storage::RunOrigin;
use crate::contracts::AgentBehavior;
use crate::runtime::background_tasks::BackgroundTaskManager;
use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tirea_contract::testing::{
    apply_after_inference_for_test, apply_after_tool_for_test, apply_before_inference_for_test,
    apply_before_tool_for_test, apply_lifecycle_for_test, TestFixture,
};

#[async_trait]
pub(super) trait AgentBehaviorTestDispatch {
    async fn run_phase(&self, phase: Phase, step: &mut StepContext<'_>);
}

#[async_trait]
impl<T> AgentBehaviorTestDispatch for T
where
    T: AgentBehavior + ?Sized,
{
    async fn run_phase(&self, phase: Phase, step: &mut StepContext<'_>) {
        let ctx = ReadOnlyContext::new(
            phase,
            step.thread_id(),
            step.messages(),
            step.run_policy(),
            step.ctx().doc(),
        );
        match phase {
            Phase::RunStart => apply_lifecycle_for_test(step, self.run_start(&ctx).await),
            Phase::StepStart => apply_lifecycle_for_test(step, self.step_start(&ctx).await),
            Phase::BeforeInference => {
                apply_before_inference_for_test(step, self.before_inference(&ctx).await)
            }
            Phase::AfterInference => {
                apply_after_inference_for_test(step, self.after_inference(&ctx).await)
            }
            Phase::BeforeToolExecute => {
                apply_before_tool_for_test(step, self.before_tool_execute(&ctx).await)
            }
            Phase::AfterToolExecute => {
                apply_after_tool_for_test(step, self.after_tool_execute(&ctx).await)
            }
            Phase::StepEnd => apply_lifecycle_for_test(step, self.step_end(&ctx).await),
            Phase::RunEnd => apply_lifecycle_for_test(step, self.run_end(&ctx).await),
        }
        // Reduce any pending state actions
        if !step.pending_state_actions.is_empty() {
            let state_actions = std::mem::take(&mut step.pending_state_actions);
            let snapshot = step.snapshot();
            let patches =
                reduce_state_actions(state_actions, &snapshot, "test", &ScopeContext::run())
                    .expect("state actions should reduce");
            for patch in patches {
                let doc = step.ctx().doc();
                for op in patch.patch().ops() {
                    doc.apply(op).expect("state action patch op should apply");
                }
                step.emit_patch(patch);
            }
        }
    }
}

#[derive(Debug)]
pub(super) struct SlowTerminatePlugin;

#[async_trait]
impl AgentBehavior for SlowTerminatePlugin {
    fn id(&self) -> &str {
        "slow_terminate_behavior_requested"
    }

    async fn before_inference(
        &self,
        _ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        tokio::time::sleep(Duration::from_millis(120)).await;
        ActionSet::single(BeforeInferenceAction::Terminate(
            crate::contracts::TerminationReason::BehaviorRequested,
        ))
    }
}

pub(super) fn apply_caller_context_with_state_and_run(
    fix: &mut TestFixture,
    state: serde_json::Value,
    run_id: &str,
) {
    apply_caller_context_with_state_run_and_messages(
        fix,
        state,
        run_id,
        vec![crate::contracts::thread::Message::user("seed message")],
    )
}

pub(super) fn apply_caller_context_with_state_run_and_messages(
    fix: &mut TestFixture,
    _state: serde_json::Value,
    run_id: &str,
    messages: Vec<crate::contracts::thread::Message>,
) {
    fix.run_identity = RunIdentity::new(
        "owner-thread".to_string(),
        None,
        run_id.to_string(),
        None,
        "caller".to_string(),
        RunOrigin::User,
    );
    fix.caller_context = CallerContext::new(
        Some("owner-thread".to_string()),
        Some(run_id.to_string()),
        Some("caller".to_string()),
        messages.into_iter().map(Arc::new).collect(),
    );
}

pub(super) fn apply_caller_context_with_state(fix: &mut TestFixture, state: serde_json::Value) {
    apply_caller_context_with_state_and_run(fix, state, "parent-run-default")
}

pub(super) fn apply_caller_context(fix: &mut TestFixture) {
    apply_caller_context_with_state(fix, json!({"forked": true}))
}

mod integration;
mod parallel;
mod plugins;
mod recovery;
mod run_tool;
