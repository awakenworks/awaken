//! The rich, typed domain event a run emits at its commit boundary.
//!
//! `RunEvent` is the authoritative producer-side vocabulary: each variant is a
//! past-tense fact carrying exactly its data, so events are constructed type-safe
//! rather than as hand-built `json!` blobs. It lowers to a persisted [`Draft`]
//! (`kind` + JSON `payload`) via `From` — the event log keeps a queryable `kind`
//! column plus a flexible payload, while producers speak the rich domain type.

use crate::agent::run::Phase;
use crate::event::draft::Draft;
use crate::event::kind::Kind;

/// A committed run fact, carrying its own typed data. Lowers to a [`Draft`] for
/// the durable event log.
#[derive(Debug, Clone, PartialEq)]
pub enum RunEvent {
    /// The run's phase transitioned (nothing→Running, Running→Ended/Waiting).
    RunPhaseChanged { phase: Phase },
    /// `commands` committed-state commands rode this checkpoint.
    StateChanged { commands: usize },
    /// The run parked, keyed by `run_id`.
    RunWaiting { run_id: String },
    /// A protected tool call passed the permission gate (ADR-0030 audit).
    PermissionDecided {
        tool_id: String,
        call_id: String,
        decision: String,
    },
    /// A run-end continuation guard decided one round; `detail` is the guard's
    /// opaque payload (the kernel does not interpret it).
    Continuation { detail: serde_json::Value },
}

impl From<RunEvent> for Draft {
    fn from(event: RunEvent) -> Self {
        let (kind, payload) = match event {
            RunEvent::RunPhaseChanged { phase } => {
                (Kind::RunPhaseChanged, serde_json::json!({ "phase": phase }))
            }
            RunEvent::StateChanged { commands } => (
                Kind::StateChanged,
                serde_json::json!({ "commands": commands }),
            ),
            RunEvent::RunWaiting { run_id } => {
                (Kind::RunWaiting, serde_json::json!({ "run_id": run_id }))
            }
            RunEvent::PermissionDecided {
                tool_id,
                call_id,
                decision,
            } => (
                Kind::PermissionDecided,
                serde_json::json!({ "tool_id": tool_id, "call_id": call_id, "decision": decision }),
            ),
            RunEvent::Continuation { detail } => (Kind::Continuation, detail),
        };
        Draft { kind, payload }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowering_preserves_the_persisted_kind_and_payload() {
        let d: Draft = RunEvent::StateChanged { commands: 3 }.into();
        assert_eq!(d.kind, Kind::StateChanged);
        assert_eq!(d.payload, serde_json::json!({ "commands": 3 }));

        let d: Draft = RunEvent::RunWaiting {
            run_id: "r1".into(),
        }
        .into();
        assert_eq!(d.kind, Kind::RunWaiting);
        assert_eq!(d.payload, serde_json::json!({ "run_id": "r1" }));

        let d: Draft = RunEvent::PermissionDecided {
            tool_id: "echo".into(),
            call_id: "c1".into(),
            decision: "allow".into(),
        }
        .into();
        assert_eq!(d.kind, Kind::PermissionDecided);
        assert_eq!(
            d.payload,
            serde_json::json!({ "tool_id": "echo", "call_id": "c1", "decision": "allow" })
        );

        // Continuation carries the guard's opaque detail verbatim as the payload.
        let detail = serde_json::json!({ "round": 2, "note": "keep going" });
        let d: Draft = RunEvent::Continuation {
            detail: detail.clone(),
        }
        .into();
        assert_eq!(d.kind, Kind::Continuation);
        assert_eq!(d.payload, detail);
    }

    #[test]
    fn run_phase_changed_lowers_with_the_phase_in_its_payload() {
        use crate::agent::run::{EndCause, Phase};
        let d: Draft = RunEvent::RunPhaseChanged {
            phase: Phase::Ended(EndCause::NaturalEnd),
        }
        .into();
        assert_eq!(d.kind, Kind::RunPhaseChanged);
        // The phase serializes under a "phase" key; a bare Running is the "Running"
        // string form pinned by the serde-boundary test.
        let d2: Draft = RunEvent::RunPhaseChanged {
            phase: Phase::Running,
        }
        .into();
        assert_eq!(d2.payload, serde_json::json!({ "phase": "Running" }));
    }
}
