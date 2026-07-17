//! Formal (property-based) verification of `session_action` — the HTTP-method → action
//! mapping that decides whether a request needs `agent.read` or `agent.write` authority
//! (ADR-0059 verification pass). The unit tests sample GET/HEAD/POST/lowercase; this
//! asserts the FAIL-SAFE law over ALL method strings: only an exactly-uppercase safe
//! method maps to the read (less-privileged) action; everything else — unknown verbs,
//! lowercase, garbage — maps to write (more-privileged), so a mis-cased or novel method
//! can never be under-authorized.

use awaken_authz_enforce::session_action;
use proptest::prelude::*;

const READ: &str = "agent.read";
const WRITE: &str = "agent.write";

proptest! {
    /// FAIL-SAFE LAW: `session_action(m)` is `agent.read` iff `m` is exactly "GET" or
    /// "HEAD"; every other string is `agent.write`. So the action is always one of the two
    /// known keys (no third outcome), and the read (weaker) action is reachable ONLY by
    /// the two exact safe methods — an unknown/mis-cased method fails safe to write.
    #[test]
    fn read_only_for_exact_safe_methods_else_write(method in "\\PC{0,16}") {
        let action = session_action(&method).0;
        let is_safe = method == "GET" || method == "HEAD";
        prop_assert_eq!(action.as_str(), if is_safe { READ } else { WRITE },
            "session_action mis-mapped {:?}", method);
    }

    /// NO THIRD OUTCOME: whatever the input, the action is one of exactly the two known
    /// keys — the mapping is total into a two-element codomain.
    #[test]
    fn the_action_is_always_one_of_the_two_known_keys(method in "\\PC{0,16}") {
        let action = session_action(&method).0;
        prop_assert!(action == READ || action == WRITE, "unexpected action key {:?}", action);
    }
}
