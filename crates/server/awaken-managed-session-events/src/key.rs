//! The durable coordinate that identifies one timeline fact.
//!
//! A fact is identified by the logical Thread that owns it, its role, and the
//! committed provenance it was derived from. Every value in a provenance is
//! already owned by the Session root or a Thread's committed prefix, so warm
//! replay, cold rebuild and an active-active peer derive the same key.
//!
//! Consumers: the protocol adapter hashes a key into its public event id (the
//! serialized provenance is part of that hash, so its shape is wire-frozen),
//! and the canonical order resolves a key to its [`crate::CanonicalOrder`].

use crate::FactRole;

/// The committed source a fact was derived from.
///
/// Serialization is part of every public id minted for a fact: variant tags,
/// field names, field order and integer widths may never change.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum FactProvenance {
    /// The operation that created a coordinated Thread link.
    LinkOperation { operation_id: String },
    /// A parent coordination call, by its public tool-use event id.
    CoordinationCall { event_id: String },
    /// One fold of a committed transcript Message.
    Message { message_id: String, ordinal: usize },
    /// One assistant response of a Run step; live previews share it.
    AssistantResponse {
        run_id: String,
        step: usize,
        response: usize,
    },
    /// The report a coordinated Run returned to its parent.
    AgentReport { run_id: String },
    /// One Run lifecycle commit.
    Lifecycle { cursor: u64 },
    /// The committed lifecycle prefix ending at one boundary.
    LifecyclePrefix { cursor: u64 },
    /// One admitted Session runtime interval.
    RuntimeInterval { interval_id: String },
    /// One budget-reach transition.
    BudgetReach { generation: u64 },
    /// One committed Run audit row.
    Audit { run_id: String, sequence: u64 },
    /// A per-Run committed state cell.
    RunState { run_id: String },
    /// One evaluation iteration of an Outcome.
    OutcomeEvaluation { outcome_id: String, iteration: u32 },
    /// A terminal Outcome.
    Outcome { outcome_id: String },
    /// The absorbing archive disposition of a Thread.
    ArchivedDisposition,
    /// The absorbing terminal of the parent Session.
    ParentTerminal,
}

/// The identity of one timeline fact: owning logical Thread, role, provenance.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FactKey {
    /// The logical (internal) Thread id; the Session id names the root.
    pub thread: String,
    pub role: FactRole,
    pub provenance: FactProvenance,
}

impl FactKey {
    #[must_use]
    pub fn new(thread: impl Into<String>, role: FactRole, provenance: FactProvenance) -> Self {
        Self {
            thread: thread.into(),
            role,
            provenance,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provenance_serialization_is_the_frozen_id_input() {
        // Each variant's serialized form is hashed into public ids. Pin every
        // tag, field name and field order against its former borrowed shape.
        let cases = [
            (
                FactProvenance::LinkOperation {
                    operation_id: "op".into(),
                },
                r#"{"source":"link_operation","operation_id":"op"}"#,
            ),
            (
                FactProvenance::CoordinationCall {
                    event_id: "e".into(),
                },
                r#"{"source":"coordination_call","event_id":"e"}"#,
            ),
            (
                FactProvenance::Message {
                    message_id: "m".into(),
                    ordinal: 2,
                },
                r#"{"source":"message","message_id":"m","ordinal":2}"#,
            ),
            (
                FactProvenance::AssistantResponse {
                    run_id: "r".into(),
                    step: 1,
                    response: 3,
                },
                r#"{"source":"assistant_response","run_id":"r","step":1,"response":3}"#,
            ),
            (
                FactProvenance::AgentReport { run_id: "r".into() },
                r#"{"source":"agent_report","run_id":"r"}"#,
            ),
            (
                FactProvenance::Lifecycle { cursor: 7 },
                r#"{"source":"lifecycle","cursor":7}"#,
            ),
            (
                FactProvenance::LifecyclePrefix { cursor: 7 },
                r#"{"source":"lifecycle_prefix","cursor":7}"#,
            ),
            (
                FactProvenance::RuntimeInterval {
                    interval_id: "i".into(),
                },
                r#"{"source":"runtime_interval","interval_id":"i"}"#,
            ),
            (
                FactProvenance::BudgetReach { generation: 4 },
                r#"{"source":"budget_reach","generation":4}"#,
            ),
            (
                FactProvenance::Audit {
                    run_id: "r".into(),
                    sequence: 9,
                },
                r#"{"source":"audit","run_id":"r","sequence":9}"#,
            ),
            (
                FactProvenance::RunState { run_id: "r".into() },
                r#"{"source":"run_state","run_id":"r"}"#,
            ),
            (
                FactProvenance::OutcomeEvaluation {
                    outcome_id: "o".into(),
                    iteration: 1,
                },
                r#"{"source":"outcome_evaluation","outcome_id":"o","iteration":1}"#,
            ),
            (
                FactProvenance::Outcome {
                    outcome_id: "o".into(),
                },
                r#"{"source":"outcome","outcome_id":"o"}"#,
            ),
            (
                FactProvenance::ArchivedDisposition,
                r#"{"source":"archived_disposition"}"#,
            ),
            (
                FactProvenance::ParentTerminal,
                r#"{"source":"parent_terminal"}"#,
            ),
        ];
        for (provenance, expected) in cases {
            assert_eq!(serde_json::to_string(&provenance).unwrap(), expected);
        }
    }
}
