use super::*;
use crate::realization_error::{RealizationError, RealizationLock};
use crate::test_support::test_observed_lease;
use awaken_session_configuration_contract::resource as session_resource;

#[test]
fn realization_authority_close_and_renewal_have_one_projection() {
    // Component split: the authority and phase lock live in ExecutionAuthority,
    // cancellation lives in ResourceStaging, and the original installed Session lock still
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
    let base = crate::test_support::wall_clock_now_ms() + 60_000;
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
    realization.staging_mut().preparation = Some(cancellation.clone());
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
            realization.execution().authority.lease().is_none(),
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
            live.execution().authority.lease(),
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
        realization.execution().authority.lease(),
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
        expires_at_unix_ms: crate::test_support::wall_clock_now_ms() + 300_000,
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
            realization.execution().phase.clone()
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
                selected.matches(&realization) && realization.execution().authority.is_closed(),
                "{rule}: the selection names the root it closed"
            ),
            None => assert!(
                before.matches(&realization),
                "{rule}: a rejected input leaves the root unchanged"
            ),
        }
    }
}

fn in_memory_view() -> (
    Arc<crate::test_support::InMemorySessionRealizations>,
    SessionRealizations,
) {
    let source = Arc::new(crate::test_support::InMemorySessionRealizations::default());
    let view = SessionRealizations::new(source.clone());
    (source, view)
}

fn is_session_absent(error: &RealizationError) -> bool {
    matches!(
        error,
        RealizationError::AuthorityLost {
            cause: crate::realization_error::AuthorityLoss::SessionAbsent,
            ..
        }
    )
}

fn is_poisoned(error: &RealizationError) -> bool {
    matches!(
        error,
        RealizationError::Poisoned(RealizationLock::InstalledSessions)
    )
}

fn denied<T>(result: Result<T, RealizationError>) -> RealizationError {
    match result {
        Err(error) => error,
        Ok(_) => panic!("the operation must be denied"),
    }
}

fn compiled() -> crate::staged_resources::CompiledEffectiveInputs {
    crate::staged_resources::CompiledEffectiveInputs {
        staged: Default::default(),
        memory_bindings: Default::default(),
    }
}

#[tokio::test]
async fn absent_session_read_write_and_lock_paths_never_install_it() {
    // Decision table: cause = a view operation on a Session not installed in
    // this process; effect = its typed absent outcome, and in every row the
    // registry still holds no Session afterwards (only the install path
    // installs, so terminal cleanup after retirement cannot resurrect one).
    // | Row | operation                        | absent outcome            |
    // | A1  | read/modify realization, session | Ok(None)                  |
    // | A2  | update realization, session      | Err(AuthorityLost Absent) |
    // | A3  | lock lifecycle/resource lock     | Err(AuthorityLost Absent) |
    // | A4  | lock lifecycle if installed      | Ok(None)                  |
    // | A5  | drop context, end disposal       | Ok(()) nothing to end     |
    // | A6  | enter dispatch                   | Err(AuthorityLost Absent) |
    // | A7  | frozen-input reads, claim, prompt| empty projection          |
    // | A8  | publish resource requirements    | Err(AuthorityLost Absent) |
    let (_source, view) = in_memory_view();
    let absent = "absent";
    assert!(
        view.read_realization(absent, |_| ()).unwrap().is_none(),
        "A1"
    );
    assert!(
        view.modify_realization(absent, |_| ()).unwrap().is_none(),
        "A1"
    );
    assert!(view.modify_session(absent, |_| ()).unwrap().is_none(), "A1");
    assert!(
        is_session_absent(&view.update_realization(absent, |_| ()).unwrap_err()),
        "A2"
    );
    assert!(
        is_session_absent(&view.update_session(absent, |_| ()).unwrap_err()),
        "A2"
    );
    assert!(
        is_session_absent(&view.lock_lifecycle(absent).await.unwrap_err()),
        "A3"
    );
    assert!(
        is_session_absent(&view.lock_resource_projection(absent).await.unwrap_err()),
        "A3"
    );
    assert!(
        view.lock_lifecycle_if_installed(absent)
            .await
            .unwrap()
            .is_none(),
        "A4"
    );
    view.drop_run_context(absent).unwrap();
    view.end_terminal_disposal(absent).unwrap();
    assert!(
        is_session_absent(&view.enter_session_dispatch(absent).unwrap_err()),
        "A6"
    );
    assert!(view.frozen(absent).unwrap().workspace.is_none(), "A7");
    assert!(view.published_snapshot(absent).unwrap().is_none(), "A7");
    assert!(view.registered_workspace(absent).unwrap().is_none(), "A7");
    assert!(view.credential_realization(absent).unwrap().is_none(), "A7");
    assert_eq!(
        view.workspace(absent, "fallback").unwrap(),
        "fallback",
        "A7"
    );
    assert!(view.dispatch_claim(absent).unwrap().is_none(), "A7");
    assert!(
        view.prompts(absent, None, false, true).unwrap().is_empty(),
        "A7"
    );
    assert!(
        is_session_absent(
            &view
                .publish_resource_requirements(
                    absent,
                    "workspace",
                    None,
                    compiled(),
                    Vec::new(),
                    "key".into()
                )
                .unwrap_err()
        ),
        "A8"
    );
    assert!(
        view.session_ids().unwrap().is_empty(),
        "no row installed a Session"
    );
}

#[tokio::test]
async fn only_the_install_path_installs_a_session() {
    // Decision table: I1 frozen-input replacement installs; I2 retaining a
    // publication installs; I3 every realization write on an installed Session
    // then succeeds; I4 removal makes it absent again and writes fail typed.
    let (_source, view) = in_memory_view();
    view.replace_frozen_with("s", |inputs| inputs.workspace = Some("w".into()))
        .unwrap();
    view.retain_publication("t", None).unwrap();
    assert_eq!(view.session_ids().unwrap(), ["s", "t"], "I1/I2");
    view.update_realization("s", |r| r.staging_mut().effect_key = Some("k".into()))
        .unwrap();
    view.enter_session_dispatch("s").unwrap();
    drop(view.lock_lifecycle("s").await.unwrap());
    drop(view.lock_resource_projection("s").await.unwrap());
    assert!(
        view.lock_lifecycle_if_installed("s")
            .await
            .unwrap()
            .is_some(),
        "I3"
    );
    assert_eq!(
        view.registered_workspace("s").unwrap().as_deref(),
        Some("w"),
        "I3"
    );
    assert!(view.remove_session("s").unwrap(), "I4");
    assert!(
        is_session_absent(&view.update_realization("s", |_| ()).unwrap_err()),
        "I4"
    );
    assert_eq!(view.session_ids().unwrap(), ["t"], "I4 no resurrection");
}

#[tokio::test]
async fn poisoned_registry_denies_every_view_path_without_panicking() {
    // Decision table: cause = the registry lock is poisoned (a writer panicked
    // while holding it); effect = every read, write, lock and install path
    // returns the typed Poisoned denial, which retries on a healthy owner,
    // and no path unwinds.
    let (source, view) = in_memory_view();
    view.replace_frozen_with("s", |_| ()).unwrap();
    source.poison();
    let refusals = [
        denied(view.read_realization("s", |_| ())),
        denied(view.modify_realization("s", |_| ())),
        denied(view.update_realization("s", |_| ())),
        denied(view.modify_session("s", |_| ())),
        denied(view.update_session("s", |_| ())),
        denied(view.lock_lifecycle("s").await),
        denied(view.lock_resource_projection("s").await),
        denied(view.lock_lifecycle_if_installed("s").await),
        denied(view.drop_run_context("s")),
        denied(view.end_terminal_disposal("s")),
        denied(view.enter_session_dispatch("s")),
        denied(view.frozen("s")),
        denied(view.workspace("s", "fallback")),
        denied(view.dispatch_claim("s")),
        denied(view.prompts("s", None, false, true)),
        denied(view.session_ids()),
        denied(view.remove_session("s")),
        denied(view.replace_frozen_with("s", |_| ())),
        denied(view.retain_publication("s", None)),
        denied(view.publish_resource_requirements(
            "s",
            "workspace",
            None,
            compiled(),
            Vec::new(),
            "key".into(),
        )),
    ];
    for error in &refusals {
        assert!(is_poisoned(error), "denied with the typed lock: {error}");
        assert_eq!(
            error.disposition(),
            crate::realization_error::Disposition::Retry
        );
    }
}
