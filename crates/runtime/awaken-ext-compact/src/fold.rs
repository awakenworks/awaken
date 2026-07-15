//! The fold decision: which prefix of a conversation to summarize.
//!
//! Pure policy — no messages, no model. Given the committed length, a threshold,
//! and how many recent messages to keep verbatim, decide how many leading messages
//! to fold into a summary (or that none should be folded yet).

/// How many leading messages to summarize, or `None` when the conversation is too
/// short to compact. Folds everything but the last `keep_last` messages, and only
/// once the length passes `threshold` (message-count mode).
pub fn fold_point(committed_len: usize, threshold: usize, keep_last: usize) -> Option<usize> {
    fold_prefix(committed_len, keep_last, committed_len > threshold)
}

/// The token-aware fold point: fold everything but the last `keep_last` messages
/// once the estimated context reaches `trigger_ratio` of the model's `max_tokens`
/// window (the "auto-compact at N% of the window" trigger).
pub fn token_fold_point(
    est_tokens: u64,
    max_tokens: u32,
    trigger_ratio: f64,
    committed_len: usize,
    keep_last: usize,
) -> Option<usize> {
    let budget = trigger_ratio * f64::from(max_tokens);
    fold_prefix(committed_len, keep_last, est_tokens as f64 >= budget)
}

/// Shared prefix decision: once `triggered`, fold everything but the last
/// `keep_last` messages (or nothing when that leaves an empty prefix).
fn fold_prefix(committed_len: usize, keep_last: usize, triggered: bool) -> Option<usize> {
    if !triggered {
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

    #[test]
    fn token_trigger_folds_only_at_the_ratio() {
        // max 1000, ratio 0.8 → budget 800 tokens.
        assert_eq!(token_fold_point(799, 1000, 0.8, 10, 2), None);
        assert_eq!(token_fold_point(800, 1000, 0.8, 10, 2), Some(8));
        assert_eq!(token_fold_point(5000, 1000, 0.8, 12, 4), Some(8));
    }

    #[test]
    fn token_trigger_respects_keep_last() {
        // Over budget, but keep_last covers the whole (short) conversation.
        assert_eq!(token_fold_point(9999, 1000, 0.8, 3, 5), None);
    }

    #[test]
    fn keep_last_zero_folds_the_whole_conversation() {
        // keep_last 0 → once triggered, everything is folded.
        assert_eq!(fold_point(5, 4, 0), Some(5));
    }

    #[test]
    fn folds_exactly_one_leading_message_at_the_lower_edge() {
        // keep_last == committed_len - 1 → the smallest non-empty fold.
        assert_eq!(fold_point(5, 4, 4), Some(1));
    }

    #[test]
    fn keep_last_wider_than_the_conversation_folds_nothing() {
        // Triggered (3 > 1), but saturating_sub floors the prefix at 0 → None.
        assert_eq!(fold_point(3, 1, 10), None);
    }

    #[test]
    fn token_budget_is_a_fractional_threshold_compared_inclusively() {
        // ratio 0.75, max 10 → budget 7.5; 7 < 7.5 (no fold), 8 >= 7.5 (fold).
        assert_eq!(token_fold_point(7, 10, 0.75, 10, 2), None);
        assert_eq!(token_fold_point(8, 10, 0.75, 10, 2), Some(8));
    }

    #[test]
    fn full_ratio_folds_only_at_the_window_edge() {
        // ratio 1.0 → budget == max; fold only once est reaches the whole window.
        assert_eq!(token_fold_point(999, 1000, 1.0, 10, 2), None);
        assert_eq!(token_fold_point(1000, 1000, 1.0, 10, 2), Some(8));
    }
}
