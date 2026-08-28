//! Process-scoped fallback identities shared by runtime and protocol adapters.
//!
//! These ids guarantee restart-unique values inside one deployment process.
//! They do not turn a request without a caller-supplied idempotency key into a
//! retry-safe mutation.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// Mint one restart-unique process identity.
#[must_use]
pub fn fresh_process_id(prefix: &str) -> String {
    static PROCESS_NAMESPACE: OnceLock<String> = OnceLock::new();
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let namespace = PROCESS_NAMESPACE.get_or_init(|| {
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        process_namespace(std::process::id(), started)
    });
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{namespace}-{sequence}")
}

fn process_namespace(process_id: u32, started_at_unix_nanos: u128) -> String {
    format!("{process_id}-{started_at_unix_nanos:x}")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{fresh_process_id, process_namespace};

    #[test]
    fn fallback_ids_follow_the_process_namespace_decision_table() {
        // Cause/effect graph: process identity + start instant define a restart
        // namespace; one atomic sequence orders every fallback identity kind.
        // Rules: same namespace concurrent calls are unique; different prefixes,
        // processes, or start instants cannot alias.
        let mut workers = Vec::new();
        for _ in 0..8 {
            workers.push(std::thread::spawn(|| {
                (0..32).map(|_| fresh_process_id("msg")).collect::<Vec<_>>()
            }));
        }
        let ids = workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("id worker"))
            .collect::<Vec<_>>();
        assert_eq!(ids.iter().collect::<HashSet<_>>().len(), ids.len(), "I1");
        assert_ne!(fresh_process_id("msg"), fresh_process_id("run"), "I2");
        assert_ne!(process_namespace(1, 7), process_namespace(2, 7), "I3");
        assert_ne!(process_namespace(1, 7), process_namespace(1, 8), "I4");
    }
}
