//! Attempt-local live-inbox discovery.
//!
//! Lifecycle belongs exclusively to Runtime's exact active-attempt scope.
//! [`LiveInboxes`] only projects a locally reachable, currently owned inbox; it
//! never owns a queue, carries messages across attempts, or materializes a
//! Session on lookup.

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_runtime_contract::live_inbox::{
    EditError, LiveInbox, LiveInboxMessageId, MessageOrigin, Offer,
};
use awaken_session_execution_contract::{LiveInboxEntry, LiveInboxError, LiveInboxSnapshot};

use crate::managed_input_projection::user_message;

/// The live inboxes of the attempts this process currently owns, one per
/// Thread with a resident running context. Named in the plural because it
/// locates the Runtime-owned [`LiveInbox`] of each Thread rather than being one.
/// Consumer: the Session application's `LiveInboxRuntime` role.
pub struct LiveInboxes {
    installed_sessions: Arc<crate::InstalledSessions>,
}

impl LiveInboxes {
    pub(crate) fn new(installed_sessions: Arc<crate::InstalledSessions>) -> Self {
        Self { installed_sessions }
    }

    /// The in-flight live inbox for `thread`, if an attempt is currently owned
    /// by this process. A pure lookup — never materializes a Session — so wire
    /// handlers can probe without side effects.
    ///
    /// | Rule | installed-Session registry | resident context | attempt inbox | result |
    /// |---|---|---|---|---|
    /// | L1 | readable | none               | -             | `Ok(None)` |
    /// | L2 | readable | present            | none / fenced | `Ok(None)` |
    /// | L3 | readable | present            | live          | `Ok(Some(inbox))` |
    /// | L4 | poisoned | -                  | -             | `Err(Unavailable)` |
    ///
    /// L4 is a fault, not an inactive queue: `Inactive` tells the caller to
    /// send a normal event instead, which a process that cannot read its own
    /// attempts must not claim.
    pub async fn live_inbox(&self, thread: &str) -> Result<Option<LiveInbox>, LiveInboxError> {
        let Some(ctx) = self
            .installed_sessions
            .read_run(thread, |run| run.context.clone())
            .map_err(|_| LiveInboxError::Unavailable)?
            .flatten()
        else {
            return Ok(None);
        };
        // Runtime execution and Worker RAII both install their exact attempt
        // bundle here. The generation and ownership fences make deployment
        // topology irrelevant and make remote, idle and stale attempts fail closed.
        Ok(ctx.runtime.active_attempt_live_inbox(&ctx.thread_id).await)
    }

    async fn active_inbox(&self, thread: &str) -> Result<LiveInbox, LiveInboxError> {
        self.live_inbox(thread)
            .await?
            .ok_or(LiveInboxError::Inactive)
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::LiveInboxRuntime for LiveInboxes {
    async fn live_inbox_snapshot(&self, thread: &str) -> Result<LiveInboxSnapshot, LiveInboxError> {
        Ok(match self.live_inbox(thread).await? {
            Some(inbox) => LiveInboxSnapshot {
                active: true,
                version: inbox.version(),
                messages: inbox
                    .list()
                    .into_iter()
                    .map(|entry| LiveInboxEntry {
                        id: entry.id.0,
                        content: entry.message.content,
                    })
                    .collect(),
            },
            None => LiveInboxSnapshot::inactive(),
        })
    }

    async fn live_inbox_queue(
        &self,
        thread: &str,
        content: Vec<ContentBlock>,
    ) -> Result<u64, LiveInboxError> {
        let inbox = self.active_inbox(thread).await?;
        // Queued through the wire surface — an out-of-band injection into a live
        // run, so it is tagged External (what a product maps operator steering onto).
        match inbox.offer_as(MessageOrigin::External, user_message(content)) {
            Offer::Accepted(id) => Ok(id.0),
            // The attempt closed between lookup and offer, or its finite live
            // identity space was exhausted: both route like no active inbox.
            Offer::Closed => Err(LiveInboxError::Inactive),
        }
    }

    async fn live_inbox_remove(&self, thread: &str, id: u64) -> Result<(), LiveInboxError> {
        let inbox = self.active_inbox(thread).await?;
        inbox
            .remove(LiveInboxMessageId(id))
            .map(|_| ())
            .map_err(to_live_inbox_error)
    }

    async fn live_inbox_replace(
        &self,
        thread: &str,
        id: u64,
        content: Vec<ContentBlock>,
    ) -> Result<(), LiveInboxError> {
        let inbox = self.active_inbox(thread).await?;
        inbox
            .replace(LiveInboxMessageId(id), user_message(content))
            .map_err(to_live_inbox_error)
    }

    async fn live_inbox_reorder(
        &self,
        thread: &str,
        order: Vec<u64>,
    ) -> Result<(), LiveInboxError> {
        let inbox = self.active_inbox(thread).await?;
        let order: Vec<LiveInboxMessageId> = order.into_iter().map(LiveInboxMessageId).collect();
        inbox.reorder(&order).map_err(to_live_inbox_error)
    }
}

fn to_live_inbox_error(error: EditError) -> LiveInboxError {
    match error {
        EditError::Closed => LiveInboxError::Inactive,
        EditError::UnknownMessage => LiveInboxError::UnknownMessage,
        EditError::StaleOrder => LiveInboxError::StaleOrder,
    }
}
