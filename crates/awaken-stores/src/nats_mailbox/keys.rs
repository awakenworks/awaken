//! KV key builders.

use crate::nats_keys::{decode_segment, encode_segment};

pub fn dispatch_key(dispatch_id: &str) -> String {
    format!("dispatch.{}", encode_segment(dispatch_id))
}

pub fn epoch_key(thread_id: &str) -> String {
    format!("epoch.{}", encode_segment(thread_id))
}

pub fn thread_index_key(thread_id: &str) -> String {
    format!("thread.{}", encode_segment(thread_id))
}

pub fn dispatch_subject(thread_id: &str) -> String {
    format!("dispatch.{}", encode_segment(thread_id))
}

pub fn thread_claim_key(thread_id: &str) -> String {
    format!("claim.{}", encode_segment(thread_id))
}

pub fn live_subject(thread_id: &str) -> String {
    format!("live.thread.{}", encode_segment(thread_id))
}

pub fn live_target_subject(thread_id: &str, run_id: &str, dispatch_id: Option<&str>) -> String {
    match dispatch_id {
        Some(dispatch_id) => format!(
            "live.thread.{}.run.{}.dispatch.{}",
            encode_segment(thread_id),
            encode_segment(run_id),
            encode_segment(dispatch_id)
        ),
        None => format!(
            "live.thread.{}.run.{}",
            encode_segment(thread_id),
            encode_segment(run_id)
        ),
    }
}

pub fn dedupe_msg_id(thread_id: &str, dedupe_key: &str) -> String {
    format!(
        "{}:{}",
        encode_segment(thread_id),
        encode_segment(dedupe_key)
    )
}

/// KV key for the authoritative dedupe lock (one per `(thread, dedupe_key)`).
/// `kv.create()` on this key is the race-free admission check.
pub fn dedupe_lock_key(thread_id: &str, dedupe_key: &str) -> String {
    format!(
        "dedupe.{}.{}",
        encode_segment(thread_id),
        encode_segment(dedupe_key)
    )
}

pub fn dispatch_id_from_key(key: &str) -> Option<String> {
    key.strip_prefix("dispatch.").and_then(decode_segment)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_encode_user_controlled_segments() {
        assert_eq!(dispatch_key("d1"), "dispatch.h6431");
        assert_eq!(epoch_key("t1"), "epoch.h7431");
        assert_eq!(thread_index_key("t1"), "thread.h7431");
        assert_eq!(dispatch_subject("t1"), "dispatch.h7431");
        assert_eq!(thread_claim_key("t1"), "claim.h7431");
        assert_eq!(live_subject("t1"), "live.thread.h7431");
        assert_eq!(
            live_target_subject("t1", "r1", Some("d1")),
            "live.thread.h7431.run.h7231.dispatch.h6431"
        );
    }

    #[test]
    fn keys_do_not_expose_wildcards_or_extra_tokens() {
        let thread_id = "tenant.*.>";
        let subject = dispatch_subject(thread_id);
        let live = live_subject(thread_id);
        let targeted_live = live_target_subject(thread_id, "run.*.>", Some("dispatch.*.>"));

        assert_eq!(subject.matches('.').count(), 1);
        assert_eq!(live.matches('.').count(), 2);
        assert_eq!(targeted_live.matches('.').count(), 6);
        assert!(!subject.contains('*'));
        assert!(!subject.contains('>'));
        assert!(!live.contains('*'));
        assert!(!live.contains('>'));
        assert!(!targeted_live.contains('*'));
        assert!(!targeted_live.contains('>'));
    }

    #[test]
    fn dispatch_id_decodes_from_dispatch_key() {
        let key = dispatch_key("dispatch.*.1");
        assert_eq!(dispatch_id_from_key(&key).as_deref(), Some("dispatch.*.1"));
        assert!(dispatch_id_from_key("thread.h7431").is_none());
    }
}
