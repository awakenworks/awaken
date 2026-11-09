//! Exact restore target acquisition under the canonical Container provider.
use super::*;

/*
 * Exact container-target cause/effect decision table (R1-R4, R6-R7).
 * Causes: C1 target absent; C2 exact tuple already present; C3 fresh provider
 * object sharing only the physical runtime while creation Blob inputs are no
 * longer available; C4 concurrent callers; C5 same effect with mismatched
 * generation/checkpoint evidence; C6 handle evidence is stripped while
 * physical evidence remains.
 * Effects: E1 one physical creation; E2 one identical durable handle; E3 one
 * Created plus one Recovered disposition; E4 recover without provider-local
 * memory; E5 mismatch preserves target and fails closed; E6 request-owned
 * handle verification rejects stripped evidence while accepting the exact
 * handle.
 * C7 terminal cleanup carries the original request. E7 delete only the exact
 * target, with absent replay succeeding. Rules: R1=C1=>E1; R2=C2=>E2;
 * R3=C2+C3=>E2+E4; R4=C1+C4=>E1+E2+E3; R6=C5=>E5;
 * R7=C6=>E6; R8=C7=>E7.
 */
#[tokio::test]
async fn exact_restore_target_is_shared_by_concurrent_and_fresh_container_providers() {
    let runtime = Arc::new(FakeRuntime::default());
    let scope = "exact-restore";
    let sandbox_spec = spec(scope);
    let request = restore_request(&sandbox_spec);
    let left_provider = provider(runtime.clone());
    let right_provider = provider(runtime.clone());

    let (left, right) = tokio::join!(
        left_provider.acquire_restore_environment(
            &sandbox_spec,
            &request,
            awaken_provisioning_contract::SandboxEffect::Unattached
        ),
        right_provider.acquire_restore_environment(
            &sandbox_spec,
            &request,
            awaken_provisioning_contract::SandboxEffect::Unattached
        ),
    );
    let left = left.unwrap();
    let right = right.unwrap();
    assert_eq!(
        [left.disposition(), right.disposition()]
            .into_iter()
            .filter(|disposition| { *disposition == pc::SandboxRestoreTargetDisposition::Created })
            .count(),
        1,
        "R4/E3 exactly one caller creates"
    );
    assert_eq!(left.target().handle(), right.target().handle(), "R4/E2");
    assert_eq!(runtime.st.lock().unwrap().restore_creates, 1, "R1/E1");
    let handle = left.target().handle();

    // The fresh provider retains the same immutable provider composition but no
    // BlobSource, seed, or SecretBroker: R3 proves physical lookup happens before
    // resolving the two required mount sources used only for creation.
    let fresh_provider = base_provider(runtime.clone());
    let replay = fresh_provider
        .acquire_restore_environment(
            &sandbox_spec,
            &request,
            awaken_provisioning_contract::SandboxEffect::Unattached,
        )
        .await
        .unwrap();
    assert_eq!(
        replay.disposition(),
        pc::SandboxRestoreTargetDisposition::Recovered,
        "R3/E4"
    );
    assert_eq!(replay.target().handle(), handle, "R3/E2");

    let mut mismatched = request.clone();
    mismatched.generation_id.push_str("-different");
    assert!(
        fresh_provider
            .acquire_restore_environment(
                &sandbox_spec,
                &mismatched,
                awaken_provisioning_contract::SandboxEffect::Unattached
            )
            .await
            .is_err(),
        "R6/E5"
    );
    assert_eq!(runtime.st.lock().unwrap().restore_creates, 1, "R6/E5");

    request
        .verify_handle(&sandbox_spec, &handle)
        .expect("R7 exact evidence verifies");
    let stripped = pc::SandboxHandle::container(
        handle.sandbox_id.clone(),
        handle.container_payload().unwrap().clone(),
    );
    assert!(
        request.verify_handle(&sandbox_spec, &stripped).is_err(),
        "R7/E6 physical restore evidence cannot be stripped"
    );

    drop(left);
    drop(right);
    drop(replay);
    let authorization = disposal_authorization(
        &pc::SandboxEffectFence::new(
            "restore-terminal",
            "terminal-owner",
            "terminal-runtime",
            2,
            u64::MAX,
        )
        .unwrap(),
    );
    fresh_provider
        .dispose_restored_environment(
            &sandbox_spec,
            &request,
            &authorization,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                authorization.effect_fence(),
            ),
        )
        .await
        .expect("R8 exact terminal disposal");
    fresh_provider
        .dispose_restored_environment(
            &sandbox_spec,
            &request,
            &authorization,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                authorization.effect_fence(),
            ),
        )
        .await
        .expect("R8 absent replay");
    let state = runtime.st.lock().unwrap();
    assert_eq!(state.restore_disposals, 1, "R8/E7");
    // R8A: both the first physical effect and response-loss replay forward
    // exactly the caller's typed preparation/successor authority.
    assert_eq!(
        state.restore_disposal_authorizations,
        vec![authorization.clone(), authorization]
    );
    assert!(state.restore_targets.is_empty(), "R8/E7");
}

#[tokio::test]
async fn malformed_restore_effect_is_rejected_before_every_substrate_effect() {
    // Pre-effect admission table R9: C1 the Session operation id is a legacy
    // opaque FNV value or malformed BLAKE3 syntax; C2 Phase-B acquisition is
    // requested. C1+C2 => E1 reject before recover/create/inspect/remove/spawn
    // and E2 leave the physical runtime empty. Existing durable operations may
    // replay in the aggregate, but rollout admission cannot activate them.
    for effect_id in [
        "fnv1a64:0123456789abcdef",
        "blake3:00",
        "blake3:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    ] {
        let runtime = Arc::new(FakeRuntime::default());
        let container_provider = provider(runtime.clone());
        let sandbox_spec = spec("malformed-restore");
        let mut request = restore_request(&sandbox_spec);
        request.effect_id = effect_id.into();
        assert!(
            container_provider
                .acquire_restore_environment(
                    &sandbox_spec,
                    &request,
                    awaken_provisioning_contract::SandboxEffect::Unattached
                )
                .await
                .is_err(),
            "R9/E1 {effect_id}"
        );
        let state = runtime.st.lock().unwrap();
        assert_eq!(state.restore_creates, 0, "R9/E1 {effect_id}");
        assert_eq!(state.restore_disposals, 0, "R9/E1 {effect_id}");
        assert!(state.restore_targets.is_empty(), "R9/E2 {effect_id}");
        assert!(state.alive.is_empty(), "R9/E2 {effect_id}");
        assert_eq!(state.inspections, 0, "R9/E1 {effect_id}");
        assert!(state.spawned.is_empty(), "R9/E1 {effect_id}");
        assert!(state.removals.is_empty(), "R9/E1 {effect_id}");
    }
}

#[tokio::test]
async fn restored_target_delivery_refuses_revoked_response_without_disposing_physical_state() {
    // C1 response cut={create,handle,evidence,plan}; C2 grant={revoked,renewed}.
    // R1 revoked => no success, no subsequent query, no physical disposal;
    // R2 renewed same tenure => exact target succeeds. These are provider-boundary
    // observations, not a claim that a pending remote Create has been joined.
    for boundary in ["create", "handle", "evidence", "plan"] {
        for renew in [false, true] {
            let runtime = Arc::new(FakeRuntime::default());
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            if boundary == "create" {
                runtime.st.lock().unwrap().creation_response_gate =
                    Some(("restore-created", gate.clone()));
            } else {
                runtime.st.lock().unwrap().restore_query_gate = Some((boundary, gate.clone()));
            }
            let provider = provider(runtime.clone());
            let spec = spec("restore-response-authority");
            let request = restore_request(&spec);
            let fence =
                pc::SandboxEffectFence::new(&request.effect_id, "worker", "boot", 1, u64::MAX - 1)
                    .unwrap();
            let authority = pc::test_support::EffectAuthority::new(&fence);
            let mut pending = Box::pin(provider.acquire_restore_environment(
                &spec,
                &request,
                pc::SandboxEffect::Fenced {
                    fence: &fence,
                    authority: &authority,
                },
            ));
            std::future::poll_fn(|cx| {
                assert!(std::future::Future::poll(pending.as_mut(), cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            authority.set(if renew {
                Some((
                    fence.with_persisted(|w| w.expires_at_unix_ms += 1).unwrap(),
                    std::time::Duration::from_secs(1),
                ))
            } else {
                None
            });
            gate.add_permits(1);
            assert_eq!(pending.await.is_ok(), renew, "R1/R2 {boundary}/{renew}");
            let state = runtime.st.lock().unwrap();
            assert_eq!(state.restore_creates, 1);
            assert_eq!(state.restore_disposals, 0);
            assert_eq!(state.alive.values().filter(|alive| **alive).count(), 1);
            if !renew {
                let next = match boundary {
                    "create" => "handle",
                    "handle" => "evidence",
                    "evidence" => "plan",
                    _ => "no-next-phase",
                };
                assert!(
                    !state.restore_queries.iter().any(|(q, _)| *q == next),
                    "R1 no next query"
                );
            }
        }
    }
}

#[tokio::test]
async fn revoked_restore_create_response_retains_backend_exposed_host_staging() {
    // R3: C1 host-bind target has physically consumed a staging locator; C2
    // response is actually Pending when its current grant is revoked. Effects:
    // return refusal before handle lookup; retain that exact directory and
    // bytes; do not remove the physical target or invent a cleanup receipt.
    let runtime = Arc::new(FakeRuntime::default());
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    {
        let mut state = runtime.st.lock().unwrap();
        state.host_live_input_bind = true;
        state.creation_response_gate = Some(("restore-created", gate.clone()));
    }
    let provider = provider(runtime.clone());
    let spec = spec("restore-retains-staging");
    let request = restore_request(&spec);
    let (fence, authority) = pc::test_support::fixture_effect(&request.effect_id);
    let mut pending = Box::pin(provider.acquire_restore_environment(
        &spec,
        &request,
        pc::SandboxEffect::Fenced {
            fence: &fence,
            authority: &authority,
        },
    ));
    std::future::poll_fn(|cx| {
        assert!(std::future::Future::poll(pending.as_mut(), cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    let physical = {
        let state = runtime.st.lock().unwrap();
        let bind = state
            .created_binds
            .values()
            .next()
            .unwrap()
            .iter()
            .find(|bind| bind.mount_path == LIVE_INPUTS_ROOT)
            .unwrap();
        std::path::PathBuf::from(&bind.source_ref)
    };
    assert!(physical.is_dir());
    std::fs::write(physical.join("retained"), b"physical owner").unwrap();
    authority.set(None);
    gate.add_permits(1);
    assert!(pending.await.is_err());
    assert_eq!(
        std::fs::read(physical.join("retained")).unwrap(),
        b"physical owner"
    );
    let state = runtime.st.lock().unwrap();
    assert_eq!(state.restore_creates, 1);
    assert_eq!(state.restore_disposals, 0);
    assert!(state.restore_queries.is_empty());
    // The fake daemon has no real terminal reconciler. Only this exact test
    // staging directory is removed after all assertions; no production target
    // or provider effect is inferred from the refused call.
    remove_host_staging_path(physical.parent().unwrap()).unwrap();
}

#[tokio::test]
async fn retained_restore_recovery_rechecks_the_same_authority_after_its_pending_read() {
    // C1 an exact existing target survived the previous acquisition; C2 its
    // recovery read is actually Pending; C3 grant changes by revoke/replace/
    // expire/renew. R1 first three => refuse before handle/projection work;
    // R2 renewal => reuse the same target, without creating or deleting it.
    // These are provider response cuts; backend mutation cuts are separate.
    for change in ["revoke", "replace", "expire", "renew"] {
        let runtime = Arc::new(FakeRuntime::default());
        let provider = provider(runtime.clone());
        let spec = spec("retained-recovery-authority");
        let request = restore_request(&spec);
        let (fence, _) = pc::test_support::fixture_effect(&request.effect_id);
        let fence = fence.with_persisted(|w| w.expires_at_unix_ms -= 1).unwrap();
        let authority = pc::test_support::EffectAuthority::new(&fence);
        let effect = pc::SandboxEffect::Fenced {
            fence: &fence,
            authority: &authority,
        };
        let _original = provider
            .acquire_restore_environment(&spec, &request, effect)
            .await
            .unwrap();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let targets = {
            let mut state = runtime.st.lock().unwrap();
            state.restore_queries.clear();
            state.restore_query_gate = Some(("recover", gate.clone()));
            state.restore_targets.clone()
        };
        let mut pending = Box::pin(provider.acquire_restore_environment(&spec, &request, effect));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(pending.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        match change {
            "revoke" => authority.set(None),
            "replace" => {
                let mut successor = fence.clone();
                successor.reassign();
                authority.set(Some((successor, std::time::Duration::from_secs(1))));
            }
            "expire" => authority.set(Some((fence.clone(), std::time::Duration::ZERO))),
            "renew" => authority.set(Some((
                fence.with_persisted(|w| w.expires_at_unix_ms += 1).unwrap(),
                std::time::Duration::from_secs(1),
            ))),
            _ => unreachable!(),
        }
        gate.add_permits(1);
        assert_eq!(pending.await.is_ok(), change == "renew", "R1/R2 {change}");
        let state = runtime.st.lock().unwrap();
        assert_eq!(state.restore_targets, targets, "original targets retained");
        assert_eq!(state.restore_creates, 1);
        assert_eq!(state.restore_disposals, 0);
        if change != "renew" {
            assert_eq!(state.restore_queries.len(), 1, "R1 no next query");
        }
    }
}
