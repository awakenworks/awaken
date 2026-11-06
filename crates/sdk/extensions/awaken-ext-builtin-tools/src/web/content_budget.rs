// @formal surface requirement=runtime.builtin_tools.tool_registration
// @formal reason: The Web-owned text projection uses an explicit approximate token budget;
// @formal reason: native tests separate its Unicode prefix behavior from network byte fences.
//! Cheap, model-independent content projection; never a billing or context-window count.

/// A coarse estimate: four ASCII scalars or one non-ASCII scalar per token.
/// Fractional ASCII costs accumulate across the complete prefix. This is a
/// stable content preference, not a tokenizer or a hard model-context guarantee.
pub(super) struct EstimatedTokenBudget(u64);

impl EstimatedTokenBudget {
    pub(super) fn new(tokens: u64) -> Self {
        Self(tokens)
    }

    pub(super) fn truncate(self, text: &mut String) {
        // Widen before multiplying so even u64::MAX is an unlimited-sized
        // preference for realizable strings; never overflow into a small cap.
        let mut quarters = u128::from(self.0) * 4;
        let boundary = text.char_indices().find_map(|(index, character)| {
            let cost = if character.is_ascii() { 1 } else { 4 };
            if quarters < cost {
                Some(index)
            } else {
                quarters -= cost;
                None
            }
        });
        if let Some(boundary) = boundary {
            text.truncate(boundary);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimated_token_budget_has_explicit_multilingual_and_zero_boundaries() {
        // C1 ASCII/CJK/astral/combining/mixed; C2 below/exact/above budget;
        // C3 empty/zero/u64::MAX. Each expected literal independently witnesses
        // unit choice and a valid Unicode scalar prefix (not grapheme counting).
        for (input, limit, expected) in [
            ("", 0, ""),
            ("abc", 0, ""),
            ("中文", 0, ""),
            ("abc", 1, "abc"),
            ("abcd", 1, "abcd"),
            ("abcde", 1, "abcd"),
            ("abcdefghijk", 2, "abcdefgh"),
            ("中文测试", 1, "中"),
            ("中文测试", 2, "中文"),
            ("🙂🚀", 1, "🙂"),
            ("🙂🚀", 2, "🙂🚀"),
            ("e\u{301}z", 1, "e"),
            ("e\u{301}z", 2, "e\u{301}z"),
            ("ab中🙂cdef", 1, "ab"),
            ("ab中🙂cdef", 2, "ab中"),
            ("ab中🙂cdef", 3, "ab中🙂cd"),
            ("ab中🙂cdef", u64::MAX, "ab中🙂cdef"),
        ] {
            let mut actual = input.to_owned();
            EstimatedTokenBudget::new(limit).truncate(&mut actual);
            assert_eq!(actual, expected, "input={input:?} budget={limit}");
        }
    }

    #[test]
    fn estimated_token_budget_is_a_monotone_idempotent_prefix_projection() {
        // Independent prefix/idempotence/monotonic laws detect suffix skipping,
        // byte-boundary cuts and accidental growth without mirroring the loop.
        for input in ["abcdefghijklmnop", "中a文🙂e\u{301}🚀tail", "🙂中", ""] {
            let mut previous = String::new();
            for limit in 0..=20 {
                let mut actual = input.to_owned();
                EstimatedTokenBudget::new(limit).truncate(&mut actual);
                assert!(input.starts_with(&actual));
                assert!(actual.starts_with(&previous));
                let original = actual.clone();
                EstimatedTokenBudget::new(limit).truncate(&mut actual);
                assert_eq!(actual, original);
                previous = actual;
            }
            assert_eq!(previous, input, "large-budget positive control");
        }
    }
}
