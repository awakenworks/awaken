//! The safe loop boundary as a shared kernel policy (ADR-0054).
//!
//! A *safe boundary* is the point in an execution loop where the current turn
//! produced no tool calls and the run may continue, park, or end without
//! breaking the commit-at-boundary invariant. Historically only the native
//! engine reached it (and only there did live-inbox steer take effect). This
//! module makes the boundary decision a policy every executor shares — the
//! native engine and the ACP/external-CLI executor both call [`evaluate_boundary`]
//! and act on its verdict, so steer and operator-pause reach every run.
//!
//! The decision is over neutral [`Message`]s and a neutral [`LiveInbox`]; it names
//! no protocol or backend. It *consumes* the inbox (a deterministic boundary
//! effect) but never commits or parks — each executor owns its commit mechanism.

use awaken_agent_contract::agent::message::{Id as MessageId, Message};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::waiting::WaitingReason;

use crate::runtime_context::RuntimeRunContext;

/// What to do at a safe loop boundary. The caller executes it with its own commit
/// mechanism (in-process for the native engine, a fresh CLI launch for ACP).
#[derive(Debug)]
pub enum BoundaryOutcome {
    /// Fold these (already re-identified) messages into the next turn and continue.
    Continue { fold: Vec<Message> },
    /// Commit these messages, then park durably with this reason (no next turn).
    Park {
        fold: Vec<Message>,
        reason: WaitingReason,
    },
    /// No queued input, no pause — the caller consults its run-end guard.
    Idle,
}

/// The id prefix shared by a run's live-inbox injections — same discipline the
/// native engine has always used.
fn inbox_id_prefix(run_id: &RunId) -> String {
    format!("{}-inbox-", run_id.0)
}

/// Drain the attempt's live inbox and re-identify each message for the committed
/// transcript. Content and role pass through untouched; only the id is replaced,
/// because caller-supplied ids carry no uniqueness promise inside the committed
/// thread. Empty (or absent) inbox means no messages.
fn drain_and_reidentify(
    ctx: &RuntimeRunContext,
    run_id: &RunId,
    transcript: &[Message],
) -> Vec<Message> {
    let Some(inbox) = ctx.live_inbox.as_ref() else {
        return Vec::new();
    };
    let drained = inbox.drain_at_boundary();
    if drained.is_empty() {
        return Vec::new();
    }
    let prefix = inbox_id_prefix(run_id);
    let base = transcript
        .iter()
        .filter(|message| message.id.0.starts_with(&prefix))
        .count();
    drained
        .into_iter()
        .enumerate()
        .map(|(nth, entry)| Message {
            id: MessageId(format!("{prefix}{}", base + nth)),
            role: entry.message.role,
            content: entry.message.content,
        })
        .collect()
}

/// Decide the boundary action. Priority: **pause preempts queued input preempts
/// idle.** The inbox is always drained first (so no queued input is lost); on a
/// pause the drained messages ride out with `Park { fold, .. }` to be committed
/// before parking, so a steer message in flight when an operator pauses is not
/// dropped.
pub fn evaluate_boundary(
    ctx: &RuntimeRunContext,
    run_id: &RunId,
    transcript: &[Message],
) -> BoundaryOutcome {
    let fold = drain_and_reidentify(ctx, run_id, transcript);
    if ctx.is_pause_requested() {
        return BoundaryOutcome::Park {
            fold,
            reason: WaitingReason::ManualPause,
        };
    }
    if !fold.is_empty() {
        return BoundaryOutcome::Continue { fold };
    }
    BoundaryOutcome::Idle
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live_inbox::{LiveInbox, MessageOrigin};
    use crate::pause::PauseSignal;
    use awaken_agent_contract::agent::message::Role;

    fn msg(text: &str) -> Message {
        Message::text(MessageId("wire-id".into()), Role::User, text)
    }
    fn run() -> RunId {
        RunId("r1".into())
    }

    #[test]
    fn no_inbox_no_pause_is_idle() {
        let ctx = RuntimeRunContext::new();
        assert!(matches!(
            evaluate_boundary(&ctx, &run(), &[]),
            BoundaryOutcome::Idle
        ));
    }

    #[test]
    fn queued_input_continues_and_is_reidentified() {
        let inbox = LiveInbox::new();
        let _ = inbox.offer_as(MessageOrigin::External, msg("steer"));
        let ctx = RuntimeRunContext::new().with_live_inbox(inbox);
        match evaluate_boundary(&ctx, &run(), &[]) {
            BoundaryOutcome::Continue { fold } => {
                assert_eq!(fold.len(), 1);
                // Caller-supplied id replaced by the run-scoped prefix.
                assert_eq!(fold[0].id.0, "r1-inbox-0");
                assert_eq!(fold[0].text_content(), "steer");
            }
            other => panic!("expected Continue, got {other:?}"),
        }
    }

    #[test]
    fn reidentify_counts_existing_inbox_messages_in_the_transcript() {
        let inbox = LiveInbox::new();
        let _ = inbox.offer(msg("next"));
        let ctx = RuntimeRunContext::new().with_live_inbox(inbox);
        // Two prior inbox messages already committed on the thread.
        let transcript = vec![
            Message::text(MessageId("r1-inbox-0".into()), Role::User, "a"),
            Message::text(MessageId("r1-inbox-1".into()), Role::User, "b"),
        ];
        match evaluate_boundary(&ctx, &run(), &transcript) {
            BoundaryOutcome::Continue { fold } => assert_eq!(fold[0].id.0, "r1-inbox-2"),
            other => panic!("expected Continue, got {other:?}"),
        }
    }

    #[test]
    fn pause_preempts_and_still_drains_the_inbox() {
        let inbox = LiveInbox::new();
        let _ = inbox.offer_as(MessageOrigin::External, msg("steer"));
        let pause = PauseSignal::new();
        pause.request();
        let ctx = RuntimeRunContext::new()
            .with_live_inbox(inbox)
            .with_pause(pause);
        match evaluate_boundary(&ctx, &run(), &[]) {
            BoundaryOutcome::Park { fold, reason } => {
                assert_eq!(reason, WaitingReason::ManualPause);
                // The in-flight steer is not lost: it rides out to be committed.
                assert_eq!(fold.len(), 1);
                assert_eq!(fold[0].id.0, "r1-inbox-0");
            }
            other => panic!("expected Park, got {other:?}"),
        }
    }

    #[test]
    fn pause_with_empty_inbox_parks_with_no_fold() {
        let pause = PauseSignal::new();
        pause.request();
        let ctx = RuntimeRunContext::new().with_pause(pause);
        match evaluate_boundary(&ctx, &run(), &[]) {
            BoundaryOutcome::Park { fold, reason } => {
                assert!(fold.is_empty());
                assert_eq!(reason, WaitingReason::ManualPause);
            }
            other => panic!("expected Park, got {other:?}"),
        }
    }
}
