//! The fold decision: which prefix of a conversation to summarize.
//!
//! Pure policy — no messages, no model. Given the committed length, a threshold,
//! and how many recent messages to keep verbatim, decide how many leading messages
//! to fold into a summary (or that none should be folded yet).

/// How many leading messages to summarize, or `None` when the conversation is too
/// short to compact. Folds everything but the last `keep_last` messages, and only
/// once the length passes `threshold`.
pub fn fold_point(committed_len: usize, threshold: usize, keep_last: usize) -> Option<usize> {
    if committed_len <= threshold {
        return None;
    }
    let fold_to = committed_len.saturating_sub(keep_last);
    if fold_to == 0 { None } else { Some(fold_to) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_conversations_are_not_folded() {
        assert_eq!(fold_point(3, 4, 2), None);
        assert_eq!(fold_point(4, 4, 2), None);
    }

    #[test]
    fn folds_all_but_keep_last_once_over_threshold() {
        assert_eq!(fold_point(5, 4, 2), Some(3));
        assert_eq!(fold_point(10, 4, 2), Some(8));
    }

    #[test]
    fn keep_last_covering_everything_folds_nothing() {
        assert_eq!(fold_point(5, 1, 5), None);
    }
}
