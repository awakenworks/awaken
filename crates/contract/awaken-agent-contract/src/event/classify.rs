//! The single routing truth (ADR-0058, Axis 4).
//!
//! One pure function, [`classify`], decides for every [`AgentEvent`] which tier it
//! belongs to and which transport channels carry it. It replaces the hand-written
//! correspondences that used to be scattered across producers and encoders; a
//! conformance test asserts it total and self-consistent, so the "three
//! vocabularies drift" failure mode is removed structurally rather than by review.
//!
//! Channels are decided by the *transport contract* each satisfies, never by
//! content (Axis 5):
//!
//! - **live** — best-effort, lossy, pre-commit broadcast. Carries [`Progress`]
//!   increments plus the opening `RunStarted` (Axis 6).
//! - **audit** — the durable, not-truth event log (`audit::RunEvent` → `Draft`).
//!   Carries run-lifecycle facts. Content whole-units are canonical *message*
//!   truth, not audit, so they are not routed here.
//!
//! Canonical truth is the message commit itself (unchanged); it is not a channel
//! `classify` routes to — a query re-folds messages on demand.

use crate::event::agent::{AgentEvent, Committed, Progress};

/// Which producer-authority tier an event belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Authoritative whole-unit / lifecycle, from the fold.
    Committed,
    /// Best-effort increment, from the live stream.
    Progress,
}

/// Where a single event is routed. Decided once, here, for the whole system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Routing {
    /// The event's producer-authority tier.
    pub tier: Tier,
    /// Broadcast to the best-effort live channel (preview / stream prefix).
    pub live: bool,
    /// Recorded in the durable audit event log as a not-truth lifecycle fact.
    pub audit: bool,
}

/// The one routing decision. Total over `AgentEvent`; the compiler enforces
/// exhaustiveness and the conformance test enforces the invariants that couple the
/// fields (e.g. every `Progress` is live-only, never audited).
pub fn classify(event: &AgentEvent) -> Routing {
    match event {
        // Best-effort increments: live only, never truth, never audit.
        AgentEvent::Progress(p) => match p {
            Progress::TextDelta { .. }
            | Progress::ReasoningDelta { .. }
            | Progress::ToolCallDelta { .. } => Routing {
                tier: Tier::Progress,
                live: true,
                audit: false,
            },
        },
        AgentEvent::Committed(c) => match c {
            // The run boundary opens the live stream *and* is an audited phase fact.
            Committed::RunStarted => Routing {
                tier: Tier::Committed,
                live: true,
                audit: true,
            },
            // Content whole-units: canonical message truth, not audit; the live
            // prefix already carried them as increments, so not re-broadcast.
            Committed::AssistantMessage { .. }
            | Committed::ToolCall { .. }
            | Committed::ToolResult { .. } => Routing {
                tier: Tier::Committed,
                live: false,
                audit: false,
            },
            // Lifecycle facts: audited (phase/park/continuation), not live.
            Committed::Waiting { .. }
            | Committed::Continuation { .. }
            | Committed::RunFinished { .. }
            | Committed::RunFailed { .. } => Routing {
                tier: Tier::Committed,
                live: false,
                audit: true,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ToolDisposition;
    use serde_json::json;

    /// Every representative variant, so the conformance assertions cover the whole
    /// surface. When a variant is added, the compiler's exhaustiveness check in
    /// `classify` forces a row here too (the match won't compile without it).
    fn every_variant() -> Vec<AgentEvent> {
        vec![
            AgentEvent::Progress(Progress::TextDelta { delta: "x".into() }),
            AgentEvent::Progress(Progress::ReasoningDelta { delta: "x".into() }),
            AgentEvent::Progress(Progress::ToolCallDelta {
                id: "c1".into(),
                name: "t".into(),
                args_delta: "{}".into(),
            }),
            AgentEvent::Committed(Committed::RunStarted),
            AgentEvent::Committed(Committed::AssistantMessage {
                id: "a1".into(),
                content: vec![],
            }),
            AgentEvent::Committed(Committed::ToolCall {
                id: "c1".into(),
                name: "t".into(),
                input: json!({}),
                disposition: ToolDisposition::Executed,
            }),
            AgentEvent::Committed(Committed::ToolResult {
                id: "c1".into(),
                content: vec![],
                is_error: false,
            }),
            AgentEvent::Committed(Committed::Waiting {
                pending_tool_use_id: None,
            }),
            AgentEvent::Committed(Committed::Continuation {
                steered: false,
                detail: json!({}),
            }),
            AgentEvent::Committed(Committed::RunFinished { exhausted: false }),
            AgentEvent::Committed(Committed::RunFailed {
                code: "x".into(),
                message: "y".into(),
            }),
        ]
    }

    #[test]
    fn tier_matches_the_variant_wrapper() {
        for e in every_variant() {
            let r = classify(&e);
            match e {
                AgentEvent::Progress(_) => assert_eq!(r.tier, Tier::Progress),
                AgentEvent::Committed(_) => assert_eq!(r.tier, Tier::Committed),
            }
        }
    }

    // INVARIANT (Axis 6): the live stream is best-effort and carries no
    // authoritative terminus. So every `Progress` is live and never audited, and no
    // `Committed` lifecycle terminus is ever broadcast live.
    #[test]
    fn progress_is_live_only_and_never_audited() {
        for e in every_variant() {
            if let AgentEvent::Progress(_) = e {
                let r = classify(&e);
                assert!(r.live, "progress increments broadcast live: {e:?}");
                assert!(!r.audit, "progress increments are never audited: {e:?}");
            }
        }
    }

    // INVARIANT: only the run boundary is both committed and live; every other
    // committed variant stays off the best-effort live channel (its increments,
    // if any, already streamed as `Progress`).
    #[test]
    fn only_run_started_is_both_committed_and_live() {
        for e in every_variant() {
            let r = classify(&e);
            if r.tier == Tier::Committed && r.live {
                assert!(
                    matches!(e, AgentEvent::Committed(Committed::RunStarted)),
                    "unexpected committed+live variant: {e:?}"
                );
            }
        }
    }

    // INVARIANT: content whole-units are canonical message truth, not audit facts.
    #[test]
    fn committed_content_is_not_audited() {
        for e in [
            AgentEvent::Committed(Committed::AssistantMessage {
                id: "a".into(),
                content: vec![],
            }),
            AgentEvent::Committed(Committed::ToolCall {
                id: "c".into(),
                name: "n".into(),
                input: json!({}),
                disposition: ToolDisposition::Executed,
            }),
            AgentEvent::Committed(Committed::ToolResult {
                id: "c".into(),
                content: vec![],
                is_error: false,
            }),
        ] {
            assert!(
                !classify(&e).audit,
                "content is message truth, not audit: {e:?}"
            );
        }
    }
}
