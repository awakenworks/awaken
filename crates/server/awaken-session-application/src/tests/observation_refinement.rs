//! Actual durable root observations consumed by the existing Rust-to-TLA gate.
use super::*;

pub(super) fn root_observation(
    before: Option<&PersistedSession>,
    after: Option<&PersistedSession>,
    now: u64,
    rule: &str,
) -> serde_json::Value {
    let state = |root: Option<&PersistedSession>| serde_json::json!({"exists": root.is_some(), "revision": root.map_or(0, |s| s.revision.0), "created": root.and_then(|s| s.created_at_unix_ms), "updated": root.and_then(|s| s.updated_at_unix_ms)});
    serde_json::json!({"rule": rule, "before": state(before), "after": state(after), "now": now})
}

pub(super) fn root_step(name: &str, before: &PersistedSession, after: &PersistedSession, now: u64) {
    awaken_reliability_testkit::write_observation_trace(
        name,
        "root",
        serde_json::json!([root_observation(Some(before), Some(after), now, name)]),
    );
}
