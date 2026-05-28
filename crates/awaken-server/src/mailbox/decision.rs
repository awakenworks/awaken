//! Live tool-call decision delivery for `Mailbox`.
//!
//! `send_decision` itself is a tiny sync helper that stays in `mailbox.rs`;
//! the async live-delivery path lives here so cancel.rs is not burdened with
//! decision-specific routing.

use awaken_server_contract::contract::lifecycle::RunStatus;
use awaken_server_contract::contract::mailbox::{
    LiveDeliveryOutcome, LiveRunCommand, LiveRunTarget, RunDispatchStatus,
};
use awaken_server_contract::contract::suspension::ToolCallResume;

use super::{Mailbox, MailboxError, live_target_for_dispatch, live_target_for_run};

impl Mailbox {
    /// Forward a tool-call decision locally or through targeted live delivery.
    ///
    /// Live delivery is at-least-once when the remote run accepts the decision
    /// but the ack is lost before the durable fallback is enqueued. Consumers
    /// must treat `(tool_call_id, decision_id)` as idempotent.
    pub async fn send_decision_live(
        &self,
        id: &str,
        tool_call_id: String,
        resume: ToolCallResume,
    ) -> Result<bool, MailboxError> {
        if self
            .executor
            .send_decision(id, tool_call_id.clone(), resume.clone())
        {
            self.record_mailbox_decision_received_for_id(id, &tool_call_id, &resume, "local_live")
                .await;
            return Ok(true);
        }

        if let Some(dispatch) = self.store.load_dispatch(id).await?
            && dispatch.status == RunDispatchStatus::Claimed
        {
            let delivered = self
                .deliver_live_decision(
                    &live_target_for_dispatch(&dispatch),
                    vec![(tool_call_id.clone(), resume.clone())],
                )
                .await?;
            if delivered {
                self.record_mailbox_decision_received_for_dispatch(
                    &dispatch,
                    &tool_call_id,
                    &resume,
                    "remote_live",
                )
                .await;
            }
            return Ok(delivered);
        }

        let run = if let Some(run) = self.run_store.load_run(id).await? {
            Some(run)
        } else {
            self.run_store.latest_run(id).await?
        };
        if let Some(run) = run
            && matches!(run.status, RunStatus::Running | RunStatus::Waiting)
        {
            let delivered = self
                .deliver_live_decision(
                    &live_target_for_run(&run),
                    vec![(tool_call_id.clone(), resume.clone())],
                )
                .await?;
            if delivered {
                self.record_mailbox_decision_received_for_run(
                    &run,
                    &tool_call_id,
                    &resume,
                    "remote_live",
                )
                .await;
            }
            return Ok(delivered);
        }

        Ok(false)
    }

    async fn deliver_live_decision(
        &self,
        target: &LiveRunTarget,
        decisions: Vec<(String, ToolCallResume)>,
    ) -> Result<bool, MailboxError> {
        match self
            .store
            .deliver_live_to(target, LiveRunCommand::Decision(decisions))
            .await?
        {
            LiveDeliveryOutcome::Delivered => Ok(true),
            LiveDeliveryOutcome::NoSubscriber => Ok(false),
        }
    }
}
