//! Managed Agent lifecycle identities and timestamps.

use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

static AGENT_ID_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(super) fn new_agent_id(workspace_id: &str) -> String {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let sequence = AGENT_ID_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let entropy = format!(
        "{workspace_id}:{}:{timestamp}:{sequence}",
        std::process::id()
    );
    let digest = Sha256::digest(entropy.as_bytes());
    let encoded = format!("{digest:x}");
    format!("agent_{}", &encoded[..32])
}

pub(super) fn lifecycle_timestamp() -> String {
    let milliseconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default();
    awaken_session_contract::epoch_millis_to_rfc3339(milliseconds)
}
