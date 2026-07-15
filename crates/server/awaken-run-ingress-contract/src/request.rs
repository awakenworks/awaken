//! The serializable durable-run instruction.
//!
//! [`RunExecutionRequest`] is exactly the data a durable dispatch queue persists
//! and replays — it carries no live handles (G3/G4), so a crash loses nothing the
//! queue cannot rebuild. The per-attempt live wiring (`RunExecutionContext`) stays
//! in the `awaken-run-ingress` host.

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime_contract::activation::RunActivation;
use serde::{Deserialize, Serialize};

/// The durable, serializable record of an accepted run. It holds no `Arc<dyn ...>`,
/// registry, or live handle (G3); the runtime builds live execution objects from
/// the activation's pinned snapshot on each attempt (G4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunExecutionRequest {
    pub activation: RunActivation,
    /// W3C `traceparent` captured when the run was admitted, so a durably-dispatched
    /// execution continues the admitting request's distributed trace across the
    /// queue boundary. Absent when admitted without an active trace (or by an older
    /// writer): a pre-existing queue row simply deserializes it as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,
}

impl RunExecutionRequest {
    pub fn new(activation: RunActivation) -> Self {
        Self {
            activation,
            traceparent: None,
        }
    }

    /// Attach the admitting request's W3C `traceparent` (see the field docs).
    pub fn with_traceparent(mut self, traceparent: Option<String>) -> Self {
        self.traceparent = traceparent;
        self
    }

    pub fn run_id(&self) -> &RunId {
        &self.activation.run_id
    }

    pub fn thread_id(&self) -> &ThreadId {
        &self.activation.thread_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
    use awaken_runtime_contract::snapshot::{
        AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
    };

    fn activation() -> RunActivation {
        RunActivation::new(
            RunId("run-1".into()),
            ThreadId("thrd-1".into()),
            ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("snap".into()),
                root_agent_id: AgentId("agent".into()),
                resolved_spec: ResolvedSpec {
                    model_candidates: Vec::new(),
                    catalog_fingerprint: CatalogFingerprint("fp".into()),
                    instructions: "be helpful".into(),
                    max_steps: 8,
                    model_binding: ModelBinding::new("prov", "model", "acp:test"),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint: CatalogFingerprint("fp".into()),
            },
            vec![Message::text(MessageId("u1".into()), Role::User, "go")],
        )
    }

    /// The request a durable queue persists and replays must survive a
    /// serialize→deserialize round-trip unchanged — G3's whole point (it carries no
    /// live handle), and the accessors read the same ids back out.
    #[test]
    fn round_trips_through_serde_with_its_accessors_intact() {
        let req = RunExecutionRequest::new(activation()).with_traceparent(Some("00-abc-01".into()));
        let json = serde_json::to_string(&req).expect("serializes");
        let back: RunExecutionRequest = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back, req, "round-trip is lossless");
        assert_eq!(back.run_id(), req.run_id());
        assert_eq!(back.thread_id(), req.thread_id());
        assert_eq!(back.traceparent.as_deref(), Some("00-abc-01"));
    }

    /// A `None` traceparent is omitted on the wire (`skip_serializing_if`), so a row
    /// written by an older writer (no trace) is byte-identical and deserializes back
    /// to `None` rather than dead-lettering — the documented forward/back-compat
    /// guarantee.
    #[test]
    fn a_none_traceparent_is_omitted_and_a_legacy_row_loads_as_none() {
        let req = RunExecutionRequest::new(activation());
        assert!(req.traceparent.is_none());
        let json = serde_json::to_string(&req).expect("serializes");
        assert!(
            !json.contains("traceparent"),
            "a None traceparent is not written: {json}"
        );
        // The same row (no traceparent key) is exactly what a pre-field writer
        // produced; it must load as None.
        let back: RunExecutionRequest = serde_json::from_str(&json).expect("legacy row loads");
        assert!(back.traceparent.is_none());
    }

    /// A durable queue row written by an OLDER peer — no `traceparent`, no
    /// `model_ref_override`. Frozen as bytes on purpose: unlike a self-round-trip
    /// (where both sides move together), this pins the exact nested wire shape, so a
    /// rename or retype anywhere down the tree (`model_binding`, `context_policy`'s
    /// `kind`, a content block's `type`) breaks this test instead of silently
    /// stranding every in-flight row a running deployment already persisted. Editing
    /// it means acknowledging a queue-format break.
    const LEGACY_QUEUE_ROW: &str = r#"{
      "activation": {
        "run_id": "run-1",
        "thread_id": "thrd-1",
        "snapshot": {
          "id": "snap",
          "root_agent_id": "agent",
          "resolved_spec": {
            "catalog_fingerprint": "fp",
            "instructions": "be helpful",
            "max_steps": 8,
            "model_binding": {
              "provider_identity_ref": "prov",
              "model_ref": "model",
              "backend_ref": "acp:test"
            },
            "model_candidates": [],
            "tool_descriptors": [],
            "plugin_ids": [],
            "plugin_config": {},
            "context_policy": { "kind": "keep_all" },
            "tool_presentation": {}
          },
          "fingerprint": "fp"
        },
        "input": [
          { "id": "u1", "role": "User", "content": [ { "type": "text", "text": "go" } ] }
        ]
      }
    }"#;

    #[test]
    fn a_frozen_legacy_queue_row_still_deserializes_intact() {
        let back: RunExecutionRequest =
            serde_json::from_str(LEGACY_QUEUE_ROW).expect("a persisted legacy row must still load");
        // The envelope's own newer field defaults.
        assert!(back.traceparent.is_none());
        // The activation's newer field defaults, and the run resolves to its pinned
        // binding — exactly how a run admitted before per-turn overrides behaves.
        assert!(back.activation.model_ref_override.is_none());
        assert_eq!(back.activation.effective_model_ref(), "model");
        assert_eq!(back.run_id().0, "run-1");
        assert_eq!(back.thread_id().0, "thrd-1");
    }

    /// The golden bytes are not stale: today's writer, with both newer fields at their
    /// defaults, still emits exactly the frozen legacy shape (compared as normalized
    /// JSON, so whitespace in the literal does not matter). If this fails, the wire
    /// shape moved and `LEGACY_QUEUE_ROW` — plus every deployed peer — is now behind.
    #[test]
    fn todays_default_writer_still_emits_the_frozen_legacy_shape() {
        let today = serde_json::to_value(RunExecutionRequest::new(activation())).expect("value");
        let frozen: serde_json::Value =
            serde_json::from_str(LEGACY_QUEUE_ROW).expect("frozen parses");
        assert_eq!(
            today, frozen,
            "the default wire shape drifted from the frozen legacy row"
        );
    }
}
