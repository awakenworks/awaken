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

#[test]
fn generation_close_accepts_only_the_original_phase_and_its_monotonic_renewal() {
    // Causes: C1 authority uninstalled/live/closed-with-lease/closed-without;
    // C2 expected lease relative to the current one (exact, older expiry of
    // the same generation, later expiry, foreign owner, other incarnation,
    // older or newer epoch); C3 the caller's phase is the original allocation.
    // Effects: E1 close and select the root; E2 leave it unchanged, no selection.
    // R1 live+exact+phase => E1; R2 live+renewed (expected expiry older)+phase
    // => E1; R3 closed(lease)+exact+phase => E1 (retry); every other row => E2:
    // future expiry, owner, incarnation, older epoch, newer epoch, replaced
    // phase, uninstalled, closed without a lease.
    let current = session_resource::SessionRealizationLease {
        owner: "local".into(),
        runtime_incarnation: "boot".into(),
        epoch: 2,
        expires_at_unix_ms: crate::test_support::runtime_unix_now_ms() + 300_000,
    };
    for rule in [
        "exact",
        "renewed",
        "closed-retry",
        "future-expiry",
        "owner",
        "incarnation",
        "older-epoch",
        "newer-epoch",
        "phase",
        "uninstalled",
        "closed-without-lease",
    ] {
        let mut realization = SessionRealization::default();
        match rule {
            "uninstalled" => {}
            "closed-without-lease" => assert!(realization.close_authority(None)),
            _ => assert!(realization.install_lease(test_observed_lease(current.clone()))),
        }
        if rule == "closed-retry" {
            assert!(realization.close_authority(Some(&current)));
        }
        let mut expected = current.clone();
        match rule {
            "renewed" => expected.expires_at_unix_ms -= 1,
            "future-expiry" => expected.expires_at_unix_ms += 1,
            "owner" => expected.owner = "foreign".into(),
            "incarnation" => expected.runtime_incarnation = "other-boot".into(),
            "older-epoch" => expected.epoch -= 1,
            "newer-epoch" => {
                expected.epoch = awaken_lease_contract::fence::Epoch::try_from(expected.epoch)
                    .expect("positive fixture epoch")
                    .next()
                    .expect("fixture epoch has a successor")
                    .get();
            }
            _ => {}
        }
        let phase = if rule == "phase" {
            Arc::new(tokio::sync::Mutex::new(()))
        } else {
            realization.execution.phase.clone()
        };
        let before = SessionRealizationRevocationSelection::from_realization(&realization);
        let selected = SessionRealizationRevocationSelection::close_generation(
            &mut realization,
            &expected,
            &phase,
        );
        let should_close = matches!(rule, "exact" | "renewed" | "closed-retry");
        assert_eq!(selected.is_some(), should_close, "decision row {rule}");
        match selected {
            Some(selected) => assert!(
                selected.matches(&realization) && realization.execution.authority.is_closed(),
                "{rule}: the selection names the root it closed"
            ),
            None => assert!(
                before.matches(&realization),
                "{rule}: a rejected input leaves the root unchanged"
            ),
        }
    }
}
