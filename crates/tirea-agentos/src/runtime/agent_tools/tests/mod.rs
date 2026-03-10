use super::*;
use crate::composition::InMemoryAgentRegistry;
use crate::contracts::runtime::behavior::ReadOnlyContext;
use crate::contracts::runtime::phase::{ActionSet, BeforeInferenceAction};
use crate::contracts::runtime::phase::{Phase, StepContext};
use crate::contracts::runtime::state::{reduce_state_actions, ScopeContext};
use crate::contracts::runtime::tool_call::{Tool, ToolStatus};
use crate::contracts::storage::RunOrigin;
use crate::contracts::thread::Thread;
use crate::contracts::AgentBehavior;
use crate::loop_runtime::loop_runner::{
    TOOL_SCOPE_CALLER_AGENT_ID_KEY, TOOL_SCOPE_CALLER_MESSAGES_KEY, TOOL_SCOPE_CALLER_STATE_KEY,
    TOOL_SCOPE_CALLER_THREAD_ID_KEY,
};
use crate::runtime::background_tasks::BackgroundTaskManager;
use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
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
            step.run_config(),
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

pub(super) fn caller_scope_with_state_and_run(
    state: serde_json::Value,
    run_id: &str,
) -> tirea_contract::RunConfig {
    caller_scope_with_state_run_and_messages(
        state,
        run_id,
        vec![crate::contracts::thread::Message::user("seed message")],
    )
}

pub(super) fn caller_scope_with_state_run_and_messages(
    state: serde_json::Value,
    run_id: &str,
    messages: Vec<crate::contracts::thread::Message>,
) -> tirea_contract::RunConfig {
    let mut rt = tirea_contract::RunConfig::new();
    rt.set(TOOL_SCOPE_CALLER_THREAD_ID_KEY, "owner-thread")
        .unwrap();
    rt.set(TOOL_SCOPE_CALLER_AGENT_ID_KEY, "caller").unwrap();
    rt.set(SCOPE_RUN_ID_KEY, run_id).unwrap();
    rt.set(TOOL_SCOPE_CALLER_STATE_KEY, state).unwrap();
    rt.set(TOOL_SCOPE_CALLER_MESSAGES_KEY, messages).unwrap();
    rt
}

pub(super) fn caller_scope_with_state(state: serde_json::Value) -> tirea_contract::RunConfig {
    caller_scope_with_state_and_run(state, "parent-run-default")
}

pub(super) fn caller_scope() -> tirea_contract::RunConfig {
    caller_scope_with_state(json!({"forked": true}))
}

mod integration;
mod parallel;
mod plugins;
mod recovery;
mod run_tool;
