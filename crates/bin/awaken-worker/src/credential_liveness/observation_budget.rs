//! Pure Worker observation publication budget; used directly by startup and renewal validation.

use std::time::Duration;

pub fn observation_budget_fits(
    probe: Duration,
    ttl: Duration,
    heartbeat: Duration,
    request: Duration,
) -> bool {
    !probe.is_zero()
        && probe
            .checked_add(heartbeat)
            .and_then(|budget| budget.checked_add(request))
            .is_some_and(|budget| ttl > budget)
}

#[cfg(kani)]
/// @obligation worker_credential.publication_budget_covers_probe_and_delivery
#[kani::proof]
fn observation_publication_budget_is_strict_and_overflow_safe() {
    // Universal scalar time-budget kernel, including Duration overflow. The
    // u128 oracle is independent of production's checked Duration additions.
    // Actual probe completion, network delay and OS scheduling are outside this
    // proof; lifecycle tests exercise startup/renegotiation withdrawal.
    let any_duration = || {
        let seconds = kani::any::<u64>();
        let nanos = kani::any::<u32>();
        kani::assume(nanos < 1_000_000_000);
        Duration::new(seconds, nanos)
    };
    let probe = any_duration();
    let ttl = any_duration();
    let heartbeat = any_duration();
    let request = any_duration();
    // Sum fractional parts in a wider integer, then normalize once. Avoid
    // multiplying every symbolic second by 10^9 in the independent oracle.
    let nanos = u64::from(probe.subsec_nanos())
        + u64::from(heartbeat.subsec_nanos())
        + u64::from(request.subsec_nanos());
    let carry = if nanos >= 2_000_000_000 {
        2
    } else if nanos >= 1_000_000_000 {
        1
    } else {
        0
    };
    let seconds = u128::from(probe.as_secs())
        + u128::from(heartbeat.as_secs())
        + u128::from(request.as_secs())
        + u128::from(carry as u64);
    let fraction = nanos - carry * 1_000_000_000;
    let expected = !probe.is_zero()
        && (u128::from(ttl.as_secs()) > seconds
            || (u128::from(ttl.as_secs()) == seconds && u64::from(ttl.subsec_nanos()) > fraction));
    assert_eq!(
        observation_budget_fits(probe, ttl, heartbeat, request),
        expected
    );
}
