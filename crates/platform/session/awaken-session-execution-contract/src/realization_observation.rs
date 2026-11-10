// @formal surface exclude=test_support_observation_port
// @formal owner crates/platform/session/awaken-session-execution-contract/src/session_realization.rs
// @formal owner crates/devtools/awaken-scenario-host/src/realization_observation.rs
// @formal owner e2e/managed_mcp_recovery_e2e.ts
// @formal reason: The parent module exports this port only under the test-support feature. It forwards
// @formal reason: checkpoints to the scenario executable without owning product transitions or
// @formal reason: persistence; the scenario adapter and real HTTP crash tests validate observation
// @formal reason: behavior. This exclusion is not a production proof claim.
//! Test-support observation port; no product transitions or persistence owner.
//! Installed once by the scenario executable. Production builds omit the port.
use std::sync::{Arc, OnceLock};

use crate::SessionId;
use crate::{McpGeneration, McpGenerationRef};

/// The canonical key that names one complete MCP generation in an observation
/// trace.
///
/// A generation's identity is the whole witness — attachment, monotonic
/// version *and* the digest of the content that version bound — so the key a
/// trace consumer compares must carry all three. Emitting it here is the point:
/// a consumer that rebuilt the key from the record's parts could (and did) drop
/// an axis, and the resulting model then read two different generations as the
/// same one. Nothing outside this function decides what names a generation.
#[must_use]
pub fn generation_observation_key(generation: &McpGeneration) -> String {
    serde_json::json!([
        generation.attachment_id().0,
        generation.version(),
        generation.digest().as_str(),
    ])
    .to_string()
}

/// One generation reference as observed: its canonical key plus the ownership
/// fence axes carried beside it. The structured witness is deliberately not
/// re-emitted — one key, derived once, on both the root and every reference.
#[must_use]
pub fn observed_generation(reference: &McpGenerationRef) -> serde_json::Value {
    serde_json::json!({
        "key": generation_observation_key(reference.generation()),
        "session_id": reference.session_id(),
        "runtime_incarnation": reference.runtime_incarnation(),
        "lease_epoch": reference.lease_epoch(),
        "lease_expires_at_unix_ms": reference.lease_expires_at_unix_ms(),
    })
}

#[async_trait::async_trait]
pub trait RealizationObserver: Send + Sync {
    async fn observe(&self, point: &'static str, session_id: &SessionId, data: serde_json::Value);
}
static OBSERVER: OnceLock<Arc<dyn RealizationObserver>> = OnceLock::new();
pub fn install(observer: Arc<dyn RealizationObserver>) {
    assert!(
        OBSERVER.set(observer).is_ok(),
        "one realization observation owner"
    );
}
pub fn enabled() -> bool {
    OBSERVER.get().is_some()
}
pub async fn checkpoint(point: &'static str, session_id: &SessionId, data: serde_json::Value) {
    if let Some(observer) = OBSERVER.get() {
        observer.observe(point, session_id, data).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generation(attachment: &str, version: u64, digest: &str) -> McpGeneration {
        // The digest is minted from real content everywhere in production; this
        // table needs two distinguishable digests for the same version, which
        // only the wire round-trip can supply.
        serde_json::from_value(serde_json::json!({
            "attachment_id": attachment,
            "version": version,
            "content_digest": digest,
        }))
        .expect("generation witness")
    }

    #[test]
    fn the_key_names_every_generation_axis_and_only_that() {
        // Cause/effect decision table: C1 same attachment; C2 same version;
        // C3 same content digest. E1 the same key; E2 a different key.
        // Rules: R1=C1+C2+C3=>E1; R2=!C1=>E2; R3=!C2=>E2; R4=!C3=>E2.
        let base = generation("mcp_a", 2, "fnv1a64:1");
        assert_eq!(
            generation_observation_key(&base),
            generation_observation_key(&generation("mcp_a", 2, "fnv1a64:1")),
            "R1/E1"
        );
        assert_eq!(
            generation_observation_key(&base),
            r#"["mcp_a",2,"fnv1a64:1"]"#,
            "R1/E1 canonical spelling"
        );
        for (rule, other) in [
            ("R2", generation("mcp_b", 2, "fnv1a64:1")),
            ("R3", generation("mcp_a", 3, "fnv1a64:1")),
            ("R4", generation("mcp_a", 2, "fnv1a64:2")),
        ] {
            assert_ne!(
                generation_observation_key(&base),
                generation_observation_key(&other),
                "{rule}/E2"
            );
        }
    }

    #[test]
    fn an_observed_reference_carries_the_key_and_its_fence_axes() {
        // Cause/effect graph: C1 a complete reference is observed. E1 the wire
        // record is exactly the canonical key plus Session/incarnation/epoch/
        // expiry; E2 no structured generation record rides along for a consumer
        // to re-derive a second, disagreeing key from. Rule: C1=>E1+E2.
        let reference = McpGenerationRef::from_persisted(
            "session-a".into(),
            generation("mcp_a", 2, "fnv1a64:1"),
            "incarnation-a".into(),
            7,
            9,
        )
        .expect("MCP generation fence epoch is nonzero");
        assert_eq!(
            observed_generation(&reference),
            serde_json::json!({
                "key": r#"["mcp_a",2,"fnv1a64:1"]"#,
                "session_id": "session-a",
                "runtime_incarnation": "incarnation-a",
                "lease_epoch": 7,
                "lease_expires_at_unix_ms": 9,
            }),
            "E1+E2"
        );
    }
}

/// One exact directive trace; formatting remains with the observation owner.
pub async fn observe_directive(
    session_id: &SessionId,
    directive: &crate::SessionRealizationDirective,
) {
    use crate::SessionRealizationAction;
    // Observe the existing directive's complete generation coordinates;
    // policy/credential payloads are outside this trace contract.
    let action = match &directive.action {
        SessionRealizationAction::Continue { instruction } => {
            serde_json::json!({"phase":"continue","instruction":instruction})
        }
        SessionRealizationAction::Released => serde_json::json!({"phase":"released"}),
        SessionRealizationAction::Stage { mcp_stages, .. } => serde_json::json!({
                    "phase": "stage",
                    "generations": mcp_stages.iter().map(|r| observed_generation(&r.generation)).collect::<Vec<_>>() }),
        SessionRealizationAction::Publish { publish, drain } => serde_json::json!({
                    "phase": "publish",
                    "publish": publish.iter().map(observed_generation).collect::<Vec<_>>(),
                    "drain": drain.iter().map(observed_generation).collect::<Vec<_>>() }),
        SessionRealizationAction::Complete => serde_json::json!({"phase": "complete"}),
    };
    checkpoint(
        "projection",
        session_id,
        serde_json::json!({"lease": directive.authority.lease, "action": action}),
    )
    .await;
}

pub async fn observe_completed(session_id: &SessionId, lease: &crate::SessionRealizationLease) {
    checkpoint("complete", session_id, serde_json::json!({"lease": lease})).await;
}
