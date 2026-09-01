//! Attempt-local live-inbox discovery.
//!
//! Lifecycle belongs exclusively to Runtime's exact active-attempt scope. The
//! Host only projects a locally reachable, currently owned inbox; it never owns
//! a queue, carries messages across attempts, or materializes a Session on lookup.

use awaken_runtime_contract::live_inbox::LiveInbox;

use crate::host::SharedHost;

impl SharedHost {
    /// The in-flight live inbox for `thread`, if an attempt is currently owned
    /// by this process. A pure lookup — never materializes a Session — so wire
    /// handlers can probe without side effects.
    pub async fn live_inbox(&self, thread: &str) -> Option<LiveInbox> {
        let ctx = self
            .session_slots
            .read(thread, |slot| slot.runtime.clone())
            .flatten()?;
        // Runtime execution and Worker RAII both install their exact attempt
        // bundle here. The generation and ownership fences make deployment
        // topology irrelevant and make remote, idle and stale attempts fail closed.
        ctx.runtime.active_attempt_live_inbox(&ctx.thread_id).await
    }
}
