//! T28-C02: barrier between authorization and the physical effect.
//!
//! Production entrypoints: `pc::SandboxProvider::claim_for_effect` over the
//! real `ContainerProvider` (neutral and SessionSandbox ports) and
//! `ContainerSandbox::dispose_for_effect`. The fake is the container runtime
//! collaborator only (`FakeRuntime`), whose response gates pause real futures.
//! These rows avoid the Unix-only warm pool and paused inputs so they run on
//! every platform, including Windows.
//!
//! Decision table, creation (port x change):
//! | rule | barrier                                  | change            | expected                      |
//! | E1   | resolved preflight (last wait pre-create)| revoke/owner/epoch| Err, alive empty, no create   |
//! | E2   | same                                     | renew             | Ok, exactly one live object   |
//! Negative control (executed): the existing post-effect `create` response
//! gate with the same changes leaves a live object, so the pre-denial oracle
//! must report a violation there: a barrier after the effect cannot prove
//! pre-denial.
//!
//! Decision table, disposal (cached Done / unbound disposal):
//! | D1 | Done cached under owner A, then owner swapped or epoch replaced | Err, zero observation/removal I/O |
//! | D2 | unbound `dispose()` completed, then fenced dispose under stale authority | Err, zero new I/O |

use super::*;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Change {
    Revoke,
    SwapOwner,
    ReplaceEpoch,
    Renew,
}

fn changed(fence: &SandboxEffectFence, change: Change) -> Option<(SandboxEffectFence, Duration)> {
    match change {
        Change::Revoke => None,
        Change::SwapOwner => Some((
            fence
                .with_persisted(|w| w.owner = "other-owner".into())
                .unwrap(),
            Duration::from_secs(1),
        )),
        Change::ReplaceEpoch => {
            // The provisioning test-support reassignment advances through the
            // fence's one admission (`Epoch::next`), never a raw increment.
            use awaken_provisioning_contract::test_support::FenceTamper as _;
            let mut successor = fence.clone();
            successor.reassign();
            Some((successor, Duration::from_secs(1)))
        }
        Change::Renew => Some((
            fence
                .with_persisted(|w| w.expires_at_unix_ms = u64::MAX)
                .unwrap(),
            Duration::from_secs(2),
        )),
    }
}

struct CreationObservation {
    succeeded: bool,
    alive: usize,
    last_boundary: Option<&'static str>,
    removals: usize,
}

/// T28-C02 oracle 0 for a change that lands before the physical effect:
/// the nearest effect gate refuses, so no physical object exists and no
/// creation call follows the barrier.
fn check_denied_before_effect(
    observed: &CreationObservation,
    barrier: &'static str,
) -> Result<(), String> {
    if observed.succeeded {
        return Err("creation succeeded under stale authority".into());
    }
    if observed.alive != 0 {
        return Err(format!(
            "{} physical container(s) exist after pre-effect denial",
            observed.alive
        ));
    }
    if observed.last_boundary != Some(barrier) {
        return Err(format!(
            "a creation call followed the barrier: last boundary {:?}",
            observed.last_boundary
        ));
    }
    if observed.removals != 0 {
        return Err("denial manufactured a cleanup removal".into());
    }
    Ok(())
}

async fn create_with_change_at(
    port: &str,
    barrier: &'static str,
    change: Change,
) -> CreationObservation {
    let runtime = Arc::new(FakeRuntime::default());
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    runtime.st.lock().unwrap().creation_response_gate = Some((barrier, gate.clone()));
    let provider = ContainerProvider::for_test(runtime.clone(), "agent:test")
        .with_blob("file-1", b"creation input".to_vec());
    let mut requested = spec("effect-gate-window");
    requested.mounts.truncate(1);
    requested.env.clear();
    let fence = pc::SandboxEffectFence::new("create", "worker", "boot", 3, u64::MAX - 1).unwrap();
    let authority = pc::test_support::EffectAuthority::new(&fence);
    let mut pending = Box::pin(async {
        let effect = awaken_provisioning_contract::SandboxEffect::Fenced {
            fence: &fence,
            authority: &authority,
        };
        if port == "neutral" {
            pc::SandboxProvider::claim_for_effect(
                &provider,
                &requested,
                effect,
                awaken_provisioning_contract::SandboxRealizationIntent::Create,
            )
            .await
            .map(|_| ())
        } else {
            let port: &dyn SandboxProvider<
                Environment = dyn awaken_provisioning_contract::SessionSandbox,
            > = &provider;
            port.claim_for_effect(
                &requested,
                effect,
                awaken_provisioning_contract::SandboxRealizationIntent::Create,
            )
            .await
            .map(|_| ())
        }
    });
    assert!(
        matches!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
                pending.as_mut(),
                cx
            )))
            .await,
            std::task::Poll::Pending
        ),
        "{port}/{barrier}/{change:?}: creation is actually held at the barrier"
    );
    {
        let state = runtime.st.lock().unwrap();
        assert_eq!(
            state.creation_boundaries.last(),
            Some(&barrier),
            "{port}/{barrier}/{change:?}: barrier entered"
        );
    }
    authority.set(changed(&fence, change));
    gate.add_permits(1);
    let result = pending.await;
    let state = runtime.st.lock().unwrap();
    CreationObservation {
        succeeded: result.is_ok(),
        alive: state.alive.values().filter(|alive| **alive).count(),
        last_boundary: state.creation_boundaries.last().copied(),
        removals: state.removals.len(),
    }
}

#[tokio::test]
async fn t28_c02_change_at_pre_effect_barrier_is_denied_at_the_nearest_effect_gate() {
    for port in ["neutral", "environment"] {
        for change in [Change::Revoke, Change::SwapOwner, Change::ReplaceEpoch] {
            let observed = create_with_change_at(port, "resolved-preflight", change).await;
            check_denied_before_effect(&observed, "resolved-preflight")
                .unwrap_or_else(|violation| panic!("E1 {port}/{change:?}: {violation}"));
        }
        // E2: a same-effect renewal is still current and creates exactly once.
        let renewed = create_with_change_at(port, "resolved-preflight", Change::Renew).await;
        assert!(renewed.succeeded, "E2 {port}");
        assert_eq!(renewed.alive, 1, "E2 {port}: one physical owner");
    }
}

#[tokio::test]
async fn t28_c02_post_effect_barrier_cannot_prove_pre_effect_denial() {
    // Negative control, executed: move the barrier to the existing
    // post-mutation `create` response gate. Production still rejects the
    // stale authority there, but the effect already happened, so the same
    // pre-denial oracle must report a violation for every change.
    for port in ["neutral", "environment"] {
        for change in [Change::Revoke, Change::SwapOwner, Change::ReplaceEpoch] {
            let observed = create_with_change_at(port, "create", change).await;
            assert!(!observed.succeeded, "NC {port}/{change:?}: still rejected");
            assert_eq!(
                observed.alive, 1,
                "NC {port}/{change:?}: effect already applied"
            );
            let violation = check_denied_before_effect(&observed, "create")
                .expect_err("NC: a post-effect barrier must not pass the pre-denial oracle");
            assert!(violation.contains("physical container"), "NC: {violation}");
        }
    }
}

#[tokio::test]
async fn t28_c02_cached_or_unbound_disposal_never_widens_authority() {
    use pc::Sandbox as _;
    // D1: Done cached under the prepared owner, then the owner is swapped or
    // the epoch replaced.
    for change in [Change::SwapOwner, Change::ReplaceEpoch, Change::Revoke] {
        let runtime = Arc::new(FakeRuntime::default());
        let sandbox = provider(runtime.clone())
            .create_container(&spec("cached-done-authority"))
            .await
            .unwrap();
        let prepared =
            pc::SandboxEffectFence::new("prepared", "owner", "boot", 1, u64::MAX - 1).unwrap();
        sandbox
            .prepare_disposal_for_effect(
                &prepared,
                &pc::test_support::EffectAuthority::new(&prepared),
            )
            .await
            .unwrap();
        let authorization = disposal_authorization(&prepared);
        let authority = pc::test_support::EffectAuthority::new(authorization.effect_fence());
        sandbox
            .dispose_for_effect(&authorization, &authority)
            .await
            .expect("D1 current authority disposes once");
        assert!(
            *sandbox.lifecycle.remove_done.lock().await,
            "D1 Done cached"
        );
        let (observations, removals) = {
            let state = runtime.st.lock().unwrap();
            (state.observations, state.removals.len())
        };
        assert_eq!(removals, 1, "D1 one physical removal");
        authority.set(changed(authorization.effect_fence(), change));
        let replay = sandbox.dispose_for_effect(&authorization, &authority).await;
        assert!(
            replay.is_err(),
            "D1 {change:?}: cached Done must not answer for stale authority"
        );
        let state = runtime.st.lock().unwrap();
        assert_eq!(
            state.observations, observations,
            "D1 {change:?}: zero cached-path I/O"
        );
        assert_eq!(state.removals.len(), removals, "D1 {change:?}");
    }

    // D2: an unbound (unfenced) disposal completed first; a later fenced
    // disposal under stale authority is still refused.
    for change in [Change::SwapOwner, Change::ReplaceEpoch, Change::Revoke] {
        let runtime = Arc::new(FakeRuntime::default());
        let sandbox = provider(runtime.clone())
            .create_container(&spec("unbound-disposal-authority"))
            .await
            .unwrap();
        let prepared =
            pc::SandboxEffectFence::new("prepared", "owner", "boot", 1, u64::MAX - 1).unwrap();
        sandbox
            .prepare_disposal_for_effect(
                &prepared,
                &pc::test_support::EffectAuthority::new(&prepared),
            )
            .await
            .unwrap();
        let authorization = disposal_authorization(&prepared);
        sandbox.dispose().await.expect("D2 unbound disposal");
        let (observations, removals) = {
            let state = runtime.st.lock().unwrap();
            (state.observations, state.removals.len())
        };
        let authority = pc::test_support::EffectAuthority::new(authorization.effect_fence());
        authority.set(changed(authorization.effect_fence(), change));
        assert!(
            sandbox
                .dispose_for_effect(&authorization, &authority)
                .await
                .is_err(),
            "D2 {change:?}: unbound Done must not authorize a fenced disposal"
        );
        let state = runtime.st.lock().unwrap();
        assert_eq!(state.observations, observations, "D2 {change:?}");
        assert_eq!(state.removals.len(), removals, "D2 {change:?}");
    }
}
