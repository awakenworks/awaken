//! The fold decision: which prefix of a conversation to summarize.
//!
//! Pure policy — no messages, no model. Given the publication-owned trigger
//! decision and how many recent messages to keep verbatim, decide how many
//! leading messages to fold into a summary (or that none should be folded yet).

/// How many leading messages to summarize after the one frozen trigger fires.
pub fn fold_point(committed_len: usize, keep_last: usize, triggered: bool) -> Option<usize> {
    if !triggered {
        return None;
    }
    let fold_to = committed_len.saturating_sub(keep_last);
    if fold_to == 0 { None } else { Some(fold_to) }
}

/// The token-aware fold point. `max_tokens` is already the effective trigger
/// derived and frozen by Config publication; Runtime makes no second ratio or
/// message-count decision.
pub fn token_fold_point(
    est_tokens: u64,
    max_tokens: u32,
    committed_len: usize,
    keep_last: usize,
) -> Option<usize> {
    fold_point(
        committed_len,
        keep_last,
        est_tokens >= u64::from(max_tokens),
    )
}

/// Prefix eligible for background precomputation before the hard trigger.
/// `prefetch_ratio` is relative to the same frozen token window and affects only
/// best-effort preparation, never the hard compaction decision.
pub fn prefetch_fold_point(
    committed_len: usize,
    estimated_tokens: u64,
    max_tokens: u32,
    prefetch_ratio: f64,
    keep_last: usize,
) -> Option<usize> {
    fold_point(
        committed_len,
        keep_last,
        estimated_tokens as f64 >= f64::from(max_tokens) * prefetch_ratio,
    )
}

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    fn fold_point_preserves_the_requested_suffix() {
        let committed_len = kani::any::<usize>();
        let keep_last = kani::any::<usize>();
        let triggered = kani::any::<bool>();
        if let Some(prefix) = fold_point(committed_len, keep_last, triggered) {
            assert!(triggered);
            assert!(prefix > 0);
            assert!(prefix <= committed_len);
            assert_eq!(committed_len - prefix, keep_last.min(committed_len));
        }
    }

    #[kani::proof]
    fn fold_point_is_present_exactly_when_triggered_with_nonempty_prefix() {
        let committed_len = kani::any::<usize>();
        let keep_last = kani::any::<usize>();
        let triggered = kani::any::<bool>();
        assert_eq!(
            fold_point(committed_len, keep_last, triggered).is_some(),
            triggered && committed_len > keep_last
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unfired_trigger_never_folds() {
        // Cause/effect rule: C1 the publication-owned token window has not fired;
        // E1 no prefix is folded, regardless of the conversation length.
        assert_eq!(fold_point(3, 2, false), None);
        assert_eq!(fold_point(400, 2, false), None);
    }

    #[test]
    fn a_fired_trigger_folds_all_but_keep_last() {
        assert_eq!(fold_point(5, 2, true), Some(3));
        assert_eq!(fold_point(10, 2, true), Some(8));
    }

    #[test]
    fn keep_last_covering_everything_folds_nothing() {
        assert_eq!(fold_point(5, 5, true), None);
    }

    #[test]
    fn token_trigger_folds_only_at_the_frozen_window() {
        // Cause/effect table: below the one frozen effective window -> no fold;
        // at/above it -> fold the exact old prefix. No Runtime ratio exists.
        assert_eq!(token_fold_point(999, 1000, 10, 2), None);
        assert_eq!(token_fold_point(1000, 1000, 10, 2), Some(8));
        assert_eq!(token_fold_point(5000, 1000, 12, 4), Some(8));
    }

    #[test]
    fn token_trigger_respects_keep_last() {
        // Over budget, but keep_last covers the whole (short) conversation.
        assert_eq!(token_fold_point(9999, 1000, 3, 5), None);
    }

    #[test]
    fn keep_last_zero_folds_the_whole_conversation() {
        // keep_last 0 → once triggered, everything is folded.
        assert_eq!(fold_point(5, 0, true), Some(5));
    }

    #[test]
    fn folds_exactly_one_leading_message_at_the_lower_edge() {
        // keep_last == committed_len - 1 → the smallest non-empty fold.
        assert_eq!(fold_point(5, 4, true), Some(1));
    }

    #[test]
    fn keep_last_wider_than_the_conversation_folds_nothing() {
        assert_eq!(fold_point(3, 10, true), None);
    }

    #[test]
    fn prefetch_precedes_the_one_hard_token_trigger() {
        // C1 a 1000-token frozen trigger; C2 prefetch ratio 0.5. E1 500 tokens
        // may prepare the prefix; E2 499 may not. This never changes the hard
        // trigger at 1000 tokens.
        assert_eq!(prefetch_fold_point(10, 500, 1000, 0.5, 2), Some(8));
        assert_eq!(prefetch_fold_point(10, 499, 1000, 0.5, 2), None);
    }
}
