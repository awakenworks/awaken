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
//! - **live** — best-effort, lossy, pre-commit broadcast. Carries [`Delta`]
//!   increments plus the opening `RunStarted` (Axis 6).
//! - **audit** — the durable, not-truth event log (`audit::RunEvent` → `Draft`).
//!   Carries run-lifecycle facts. Content whole-units are canonical *message*
//!   truth, not audit, so they are not routed here.
//!
//! Canonical truth is the message commit itself (unchanged); it is not a channel
//! `classify` routes to — a query re-folds messages on demand.

use crate::event::agent::{AgentEvent, Delta, Fact};

/// Which producer-authority tier an event belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// A discrete complete event, from the fold.
    Fact,
    /// A streaming fragment, from the live stream.
    Delta,
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
/// fields (e.g. every `Delta` is live-only, never audited).
pub fn classify(event: &AgentEvent) -> Routing {
    match event {
        // Best-effort increments: live only, never truth, never audit.
        AgentEvent::Delta(p) => match p {
            Delta::TextDelta { .. }
            | Delta::ReasoningDelta { .. }
            | Delta::ToolCallDelta { .. } => Routing {
                tier: Tier::Delta,
                live: true,
                audit: false,
            },
        },
        AgentEvent::Fact(c) => match c {
            // The run boundary opens the live stream *and* is an audited phase fact.
            Fact::RunStarted => Routing {
                tier: Tier::Fact,
                live: true,
                audit: true,
            },
            // Content whole-units: canonical message truth, not audit; the live
            // prefix already carried them as increments, so not re-broadcast.
            Fact::AssistantMessage { .. } | Fact::ToolCall { .. } | Fact::ToolResult { .. } => {
                Routing {
                    tier: Tier::Fact,
                    live: false,
                    audit: false,
                }
            }
            // Lifecycle facts: audited (phase/park/continuation), not live.
            Fact::Waiting { .. }
            | Fact::Continuation { .. }
            | Fact::RunFinished { .. }
            | Fact::RunFailed { .. } => Routing {
                tier: Tier::Fact,
                live: false,
                audit: true,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::agent::ToolDisposition;
    use serde_json::json;

    /// Every representative variant, so the conformance assertions cover the whole
    /// surface. When a variant is added, the compiler's exhaustiveness check in
    /// `classify` forces a row here too (the match won't compile without it).
    fn every_variant() -> Vec<AgentEvent> {
        vec![
            AgentEvent::Delta(Delta::TextDelta { delta: "x".into() }),
            AgentEvent::Delta(Delta::ReasoningDelta { delta: "x".into() }),
            AgentEvent::Delta(Delta::ToolCallDelta {
                id: "c1".into(),
                name: "t".into(),
                args_delta: "{}".into(),
            }),
            AgentEvent::Fact(Fact::RunStarted),
            AgentEvent::Fact(Fact::AssistantMessage {
                id: "a1".into(),
                content: vec![],
            }),
            AgentEvent::Fact(Fact::ToolCall {
                id: "c1".into(),
                name: "t".into(),
                input: json!({}),
                disposition: ToolDisposition::Executed,
            }),
            AgentEvent::Fact(Fact::ToolResult {
                id: "c1".into(),
                content: vec![],
                is_error: false,
            }),
            AgentEvent::Fact(Fact::Waiting {
                pending_tool_use_id: None,
            }),
            AgentEvent::Fact(Fact::Continuation {
                steered: false,
                detail: json!({}),
            }),
            AgentEvent::Fact(Fact::RunFinished { exhausted: false }),
            AgentEvent::Fact(Fact::RunFailed {
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
                AgentEvent::Delta(_) => assert_eq!(r.tier, Tier::Delta),
                AgentEvent::Fact(_) => assert_eq!(r.tier, Tier::Fact),
            }
        }
    }

    // INVARIANT (Axis 6): the live stream is best-effort and carries no
    // authoritative terminus. So every `Delta` is live and never audited, and no
    // `Fact` lifecycle terminus is ever broadcast live.
    #[test]
    fn live_tier_is_broadcast_and_never_audited() {
        for e in every_variant() {
            if let AgentEvent::Delta(_) = e {
                let r = classify(&e);
                assert!(r.live, "live increments broadcast live: {e:?}");
                assert!(!r.audit, "live increments are never audited: {e:?}");
            }
        }
    }

    // INVARIANT: only the run boundary is both committed and live; every other
    // committed variant stays off the best-effort live channel (its increments,
    // if any, already streamed as `Delta`).
    #[test]
    fn only_run_started_is_both_committed_and_live() {
        for e in every_variant() {
            let r = classify(&e);
            if r.tier == Tier::Fact && r.live {
                assert!(
                    matches!(e, AgentEvent::Fact(Fact::RunStarted)),
                    "unexpected committed+live variant: {e:?}"
                );
            }
        }
    }

    // INVARIANT: content whole-units are canonical message truth, not audit facts.
    #[test]
    fn committed_content_is_not_audited() {
        for e in [
            AgentEvent::Fact(Fact::AssistantMessage {
                id: "a".into(),
                content: vec![],
            }),
            AgentEvent::Fact(Fact::ToolCall {
                id: "c".into(),
                name: "n".into(),
                input: json!({}),
                disposition: ToolDisposition::Executed,
            }),
            AgentEvent::Fact(Fact::ToolResult {
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
