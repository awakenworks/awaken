// @formal entrypoint worker_credential.publication_budget_covers_probe_and_delivery
//! Pure Worker observation publication budget; used directly by startup and renewal validation.
//!
//! It is part of the Worker contract because the Worker bootstrap is resolved
//! by every process that reads a Worker's settings (the standalone Worker and
//! the AllInOne startup) and the Worker's renewal loop checks the same budget.

use std::time::Duration;

/// Admit a credential observation window: the TTL must exceed the non-zero
/// probe interval and, once the registry publication timing is known, the
/// probe plus one heartbeat interval and one request budget.
pub fn validate_observation_timing(
    probe_interval: Duration,
    observation_ttl: Duration,
    publication: Option<awaken_lease_contract::authority::AuthorityLeaseTiming>,
) -> Result<(), String> {
    if probe_interval.is_zero() || observation_ttl <= probe_interval {
        return Err("credential observation TTL must exceed the non-zero probe interval".into());
    }
    if let Some(timing) = publication
        && !observation_budget_fits(
            probe_interval,
            observation_ttl,
            timing.renew_interval(),
            timing.request_timeout(),
        )
    {
        return Err(format!(
            "credential observation TTL {:?} must exceed probe interval {:?} + heartbeat interval {:?} + request budget {:?}",
            observation_ttl,
            probe_interval,
            timing.renew_interval(),
            timing.request_timeout(),
        ));
    }
    Ok(())
}

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

#[cfg(test)]
mod tests {
    use super::validate_observation_timing;
    use std::time::Duration;

    #[test]
    fn observation_timing_decision_table() {
        // Causes: P=probe interval, T=observation TTL, H=negotiated renewal,
        // R=request budget. Effects: reject invalid authored settings before I/O;
        // reject T<=P+H+R before admission (strict expiry); accept a positive
        // margin. R1 zero P; R2 T<=P; R3 valid pre-negotiation; R4 T<=sum;
        // R5 T>sum; R6 checked overflow. Independent boundary rows prevent a
        // check against max(P,H), or omission of transport R, passing this table.
        use awaken_lease_contract::authority::AuthorityLeaseTiming;
        let negotiated = Some(AuthorityLeaseTiming::from_ttl_ms(30_000));
        for (rule, probe, ttl, publication, accepted) in [
            ("R1", 0, 30_000, None, false),
            ("R2 below", 10_000, 9_999, None, false),
            ("R2 equal", 10_000, 10_000, None, false),
            ("R3", 1_000, 5_000, None, true),
            ("R4 old fixture", 1_000, 5_000, negotiated, false),
            ("R4 sum below", 10_000, 24_999, negotiated, false),
            ("R4 sum equal", 10_000, 25_000, negotiated, false),
            ("R5 margin", 10_000, 25_001, negotiated, true),
            ("R5 default", 10_000, 30_000, negotiated, true),
        ] {
            assert_eq!(
                validate_observation_timing(
                    Duration::from_millis(probe),
                    Duration::from_millis(ttl),
                    publication,
                )
                .is_ok(),
                accepted,
                "{rule}"
            );
        }
        assert!(
            validate_observation_timing(
                Duration::MAX - Duration::from_nanos(1),
                Duration::MAX,
                negotiated,
            )
            .is_err(),
            "R6"
        );
    }
}
