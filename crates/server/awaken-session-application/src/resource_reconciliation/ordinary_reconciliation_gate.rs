// @formal entrypoint session_resources.terminal_session_never_reactivates
//! Closed admission gate for ordinary Runtime Resource reconciliation.
//!
//! Recovery deliberately scans terminal Session roots so their cleanup can
//! converge. This module owns the smaller decision that those roots can never
//! be reinterpreted as ordinary live Resource activation work.

/// The only gate into ordinary Runtime Resource installation.
pub(super) const fn ordinary_resource_reconciliation_allowed(
    is_terminal: bool,
    is_idle: bool,
) -> bool {
    !is_terminal && is_idle
}

#[cfg(kani)]
/// @obligation session_resources.terminal_session_never_reactivates
#[kani::proof]
fn terminal_session_never_enters_ordinary_resource_reconciliation() {
    // Cause/effect graph: C1 the durable root is terminal; C2 execution is
    // Idle. Effects: E1 ordinary Runtime apply is allowed only for live+Idle;
    // E2 every terminal root is denied regardless of its execution projection;
    // E3 live non-Idle remains deferred.
    //
    // | Rule | terminal | idle | ordinary apply | Effect |
    // |---|---|---|---|---|
    // | R1 | no | yes | yes | E1 |
    // | R2 | no | no | no | E3 |
    // | R3 | yes | yes | no | E2 |
    // | R4 | yes | no | no | E2 |
    let terminal: bool = kani::any();
    let idle: bool = kani::any();
    let allowed = ordinary_resource_reconciliation_allowed(terminal, idle);

    assert_eq!(allowed, !terminal && idle);
    if terminal {
        assert!(!allowed);
    }
}
