use std::sync::Arc;

use async_trait::async_trait;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::suspension::ToolCallResume;
use awaken_runtime::RunActivation;

use awaken_server::mailbox::{MailboxDispatchStatus, RunDispatchExecutor};
use awaken_server::services::run_control_service::InputMode;

struct OldExecutorShape;

#[async_trait]
impl RunDispatchExecutor for OldExecutorShape {
    async fn run(
        &self,
        _request: RunActivation,
        _sink: Arc<dyn EventSink>,
    ) -> Result<
        awaken_runtime::loop_runner::AgentRunResult,
        awaken_runtime::loop_runner::AgentLoopError,
    > {
        unreachable!("compat test only checks trait implementation shape")
    }

    fn cancel(&self, _id: &str) -> bool {
        false
    }

    async fn cancel_and_wait_by_thread(&self, _thread_id: &str) -> bool {
        false
    }

    fn send_decision(&self, _id: &str, _tool_call_id: String, _resume: ToolCallResume) -> bool {
        false
    }
}

#[test]
fn run_dispatch_executor_old_impl_shape_still_compiles() {
    let _executor: Arc<dyn RunDispatchExecutor> = Arc::new(OldExecutorShape);
}

#[test]
fn mailbox_dispatch_status_exhaustive_match_keeps_0_2_variants() {
    fn label(status: MailboxDispatchStatus) -> &'static str {
        match status {
            MailboxDispatchStatus::Running => "running",
            MailboxDispatchStatus::Queued => "queued",
        }
    }

    assert_eq!(label(MailboxDispatchStatus::Running), "running");
    assert_eq!(label(MailboxDispatchStatus::Queued), "queued");
}

#[test]
fn input_mode_exhaustive_match_keeps_0_2_variants() {
    fn label(mode: InputMode) -> &'static str {
        match mode {
            InputMode::Queue => "queue",
            InputMode::InterruptThenQueue => "interrupt_then_queue",
            InputMode::ResumeOpenRun => "resume_open_run",
        }
    }

    assert_eq!(label(InputMode::Queue), "queue");
    assert_eq!(label(InputMode::InterruptThenQueue), "interrupt_then_queue");
    assert_eq!(label(InputMode::ResumeOpenRun), "resume_open_run");
}
