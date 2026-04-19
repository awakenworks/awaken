use crate::nats_keys::encode_segment;

pub fn thread_subject(thread_id: &str) -> String {
    format!("thread.{}", encode_segment(thread_id))
}

pub fn hot_meta_key(thread_id: &str) -> String {
    format!("meta.{}", encode_segment(thread_id))
}

pub fn hot_run_key(run_id: &str) -> String {
    format!("run.{}", encode_segment(run_id))
}

pub fn flushed_seq_key(thread_id: &str) -> String {
    format!("flushed.{}", encode_segment(thread_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_encode_user_controlled_segments() {
        assert_eq!(thread_subject("t1"), "thread.h7431");
        assert_eq!(hot_meta_key("t1"), "meta.h7431");
        assert_eq!(hot_run_key("r1"), "run.h7231");
        assert_eq!(flushed_seq_key("t1"), "flushed.h7431");
    }

    #[test]
    fn subjects_do_not_expose_wildcards_or_extra_tokens() {
        let subject = thread_subject("thread.*.>");

        assert_eq!(subject.matches('.').count(), 1);
        assert!(!subject.contains('*'));
        assert!(!subject.contains('>'));
    }
}
