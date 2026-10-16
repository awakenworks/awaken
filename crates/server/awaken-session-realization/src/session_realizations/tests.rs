use super::*;
use crate::test_support::test_observed_lease;
use awaken_session_configuration_contract::resource as session_resource;

#[test]
fn realization_authority_close_and_renewal_have_one_projection() {
    // Component split: the authority and phase lock live in ExecutionAuthority,
    // cancellation lives in ResourceStaging, and the original slot mutex still
    // linearizes their joint close/renewal. These rows cover that cross-role
    // boundary without another lease, lock, or cached ownership decision.
    // Decision table: C1 absent/live/closed; C2 expected exact/stale/none;
    // C3 renewal snapshot exact/stale; C4 renewed same-generation expiry
    // shorter/equal/longer. Effects: close hides authority and cancels I/O;
    // stale close changes nothing; only exact live monotonic renewal wins;
    // closed replay cannot regain authority, a fresh generation can.
    // Q1 absent+exact rejects; Q2 absent+none closes; Q3 live+stale rejects;
    // Q4 live+exact closes; Q5 closed+exact retry succeeds; Q6-Q8 exact
    // renewal crosses the three expiry partitions; Q9 stale reply rejects.
    let base = crate::test_support::runtime_unix_now_ms() + 60_000;
    let lease = session_resource::SessionRealizationLease {
        owner: "worker".into(),
        runtime_incarnation: "worker/1".into(),
        epoch: 1,
        expires_at_unix_ms: base + 100,
    };
    let mut realization = SessionRealization::default();
    assert!(!realization.close_authority(Some(&lease)), "Q1");
    assert!(realization.close_authority(None), "Q2");
    assert!(
        !realization.resource_preparation_authorized(&lease),
        "Q2 no-preparation fallback denied"
    );
    realization.install_lease(test_observed_lease(lease.clone()));
    let mut stale = lease.clone();
    stale.expires_at_unix_ms -= 1;
    let cancellation = Arc::new(awaken_runtime_contract::CancellationToken::new());
    realization.staging.preparation = Some(cancellation.clone());
    assert!(!realization.close_authority(Some(&stale)), "Q3");
    assert!(!cancellation.is_cancelled(), "Q3");
    assert!(realization.close_authority(Some(&lease)), "Q4");
    assert!(cancellation.is_cancelled(), "Q4");
    assert!(realization.close_authority(Some(&lease)), "Q5");
    for expiry in [base + 99, base + 100, base + 101] {
        let mut renewed = lease.clone();
        renewed.expires_at_unix_ms = expiry;
        assert!(
            !realization.renew_lease(&lease, test_observed_lease(renewed.clone())),
            "Q5 closed reply"
        );
        realization.install_lease(test_observed_lease(renewed.clone()));
        assert!(
            realization.execution.authority.lease().is_none(),
            "Q5 closed assertion"
        );
        let mut live = SessionRealization::default();
        live.install_lease(test_observed_lease(lease.clone()));
        assert_eq!(
            live.renew_lease(&lease, test_observed_lease(renewed.clone())),
            expiry >= base + 100,
            "Q6-Q8"
        );
        assert_eq!(
            live.execution.authority.lease(),
            Some(if expiry >= base + 100 {
                &renewed
            } else {
                &lease
            })
        );
        assert!(
            !live.renew_lease(&stale, test_observed_lease(renewed)),
            "Q9"
        );
    }
    let mut next = lease;
    next.epoch += 1;
    realization.install_lease(test_observed_lease(next.clone()));
    assert_eq!(
        realization.execution.authority.lease(),
        Some(&next),
        "fresh Control generation"
    );
}
