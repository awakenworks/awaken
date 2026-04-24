use crate::nats_keys::{decode_segment, encode_segment};

pub fn thread_subject(thread_id: &str) -> String {
    format!("thread.{}", encode_segment(thread_id))
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn thread_id_from_thread_subject(subject: &str) -> Option<String> {
    decode_segment(subject.strip_prefix("thread.")?)
}

pub fn hot_meta_key(thread_id: &str) -> String {
    format!("meta.{}", encode_segment(thread_id))
}

pub fn thread_id_from_hot_meta_key(key: &str) -> Option<String> {
    decode_segment(key.strip_prefix("meta.")?)
}

pub fn hot_run_key(run_id: &str) -> String {
    format!("run.{}", encode_segment(run_id))
}

pub fn flushed_seq_key(thread_id: &str) -> String {
    format!("flushed.{}", encode_segment(thread_id))
}

pub fn wal_state_key(thread_id: &str, thread_seq: u64) -> String {
    format!("wal.{}.{}", encode_segment(thread_id), thread_seq)
}

pub fn wal_state_prefix(thread_id: &str) -> String {
    format!("wal.{}.", encode_segment(thread_id))
}

pub fn thread_id_from_wal_state_key(key: &str) -> Option<String> {
    let remainder = key.strip_prefix("wal.")?;
    let (encoded_thread_id, _) = remainder.rsplit_once('.')?;
    decode_segment(encoded_thread_id)
}

pub fn thread_seq_from_wal_state_key(key: &str) -> Option<u64> {
    key.rsplit_once('.')?.1.parse().ok()
}

pub fn hierarchy_lock_key() -> &'static str {
    "hierarchy.lock"
}

pub fn flush_lock_key(thread_id: &str) -> String {
    format!("flush.lock.{}", encode_segment(thread_id))
}

pub fn poison_wal_stream_key(stream_seq: u64) -> String {
    format!("poison.seq.{stream_seq}")
}

pub fn poison_wal_hash_key(payload_hash: u64) -> String {
    format!("poison.hash.{payload_hash:016x}")
}

pub fn poison_wal_prefix() -> &'static str {
    "poison."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_encode_user_controlled_segments() {
        assert_eq!(thread_subject("t1"), "thread.h7431");
        assert_eq!(
            thread_id_from_thread_subject("thread.h7431").as_deref(),
            Some("t1")
        );
        assert_eq!(hot_meta_key("t1"), "meta.h7431");
        assert_eq!(
            thread_id_from_hot_meta_key("meta.h7431").as_deref(),
            Some("t1")
        );
        assert_eq!(hot_run_key("r1"), "run.h7231");
        assert_eq!(flushed_seq_key("t1"), "flushed.h7431");
        assert_eq!(wal_state_key("t1", 42), "wal.h7431.42");
        assert_eq!(wal_state_prefix("t1"), "wal.h7431.");
        assert_eq!(
            thread_id_from_wal_state_key("wal.h7431.42").as_deref(),
            Some("t1")
        );
        assert_eq!(thread_seq_from_wal_state_key("wal.h7431.42"), Some(42));
        assert_eq!(hierarchy_lock_key(), "hierarchy.lock");
        assert_eq!(flush_lock_key("t1"), "flush.lock.h7431");
        assert_eq!(poison_wal_stream_key(7), "poison.seq.7");
        assert_eq!(
            poison_wal_hash_key(0xfeed_beef),
            "poison.hash.00000000feedbeef"
        );
        assert_eq!(poison_wal_prefix(), "poison.");
    }

    #[test]
    fn subjects_do_not_expose_wildcards_or_extra_tokens() {
        let subject = thread_subject("thread.*.>");

        assert_eq!(subject.matches('.').count(), 1);
        assert!(!subject.contains('*'));
        assert!(!subject.contains('>'));
    }
}
