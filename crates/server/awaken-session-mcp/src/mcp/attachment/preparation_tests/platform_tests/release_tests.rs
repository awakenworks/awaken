// @formal surface exclude=test_only
// @formal owner crates/server/awaken-session-mcp/src/mcp/attachment.rs
// @formal reason: Original Platform allocation cleanup through the public staging/drain entrances and canonical slot; mediator acknowledgments are an installed test adapter, not hosted enforcement evidence.
use super::*;
use crate::mcp_material::McpGenerationProjection;
use std::{future::Future, time::Duration};

struct Prepared {
    request: StageMcpAttachment,
    gate: Arc<Provisioner>,
    owner: McpGenerationProjection,
}

fn current(
    mcp: &McpSessions,
    slots: &Slots,
    request: &StageMcpAttachment,
) -> McpGenerationProjection {
    mcp.projection(slots, &request.generation).unwrap().unwrap()
}

fn observed(gate: &Provisioner) -> AllocationObservation {
    let entries = gate.observed.lock().unwrap();
    assert_eq!(entries.len(), 1, "one original physical allocation");
    entries[0].clone()
}

fn assert_retained(mcp: &McpSessions, slots: &Slots, prepared: &Prepared) {
    let owner = current(mcp, slots, &prepared.request);
    assert_eq!(owner.state, McpProjectionState::Draining);
    assert!(Arc::ptr_eq(&owner.drain, &prepared.owner.drain));
    assert!(Arc::ptr_eq(
        &owner.platform_egress.as_ref().unwrap().allocation,
        &prepared.owner.platform_egress.as_ref().unwrap().allocation,
    ));
    assert!(!slots.0.lock().unwrap().state.local_effects_cleared());
    let remote = observed(&prepared.gate);
    assert_eq!(remote.realization, prepared.request.realization_id);
    assert_eq!(remote.stage, prepared.request.stage_idempotency_key);
    assert!(remote.issued.load(Ordering::SeqCst));
}

fn assert_removed(mcp: &McpSessions, slots: &Slots, prepared: &Prepared) {
    let owner = current(mcp, slots, &prepared.request);
    assert!(owner.effects_cleared());
    assert!(owner.request == prepared.request);
    assert!(slots.0.lock().unwrap().state.local_effects_cleared());
    assert_eq!(observed(&prepared.gate).phase.load(Ordering::SeqCst), 2);
}

async fn refused_after_provision(
    mcp: &McpSessions,
    slots: &Arc<Slots>,
    lost_reply: bool,
) -> Prepared {
    let PlatformPreparation {
        request,
        gate,
        mut task,
    } = begin(mcp, slots);
    tokio::select! {
        entered = gate.entered.acquire() => entered.unwrap().forget(),
        result = &mut task => panic!("provision cut was not reached: {result:?}"),
        _ = tokio::time::sleep(Duration::from_secs(10)) => panic!("provision cut timed out"),
    }
    let reserved = current(mcp, slots, &request);
    assert_eq!(reserved.state, McpProjectionState::Staging);
    assert!(
        reserved.platform_egress.as_ref().unwrap().route().is_none(),
        "owner precedes route IO"
    );
    gate.lose_provision_reply
        .store(lost_reply, Ordering::SeqCst);
    slots.0.lock().unwrap().root = Some(false);
    gate.release.add_permits(1);
    let error = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(
        error.code,
        if lost_reply {
            "mcp_gateway_route_unavailable"
        } else {
            "mcp_stale_ownership"
        }
    );
    let owner = current(mcp, slots, &request);
    assert_eq!(
        owner.platform_egress.as_ref().unwrap().route().is_none(),
        lost_reply
    );
    let prepared = Prepared {
        request,
        gate,
        owner,
    };
    assert_retained(mcp, slots, &prepared);
    prepared
}

async fn reach_cut(
    gate: &tokio::sync::Semaphore,
    operation: impl Future<Output = Result<(), McpError>>,
) {
    tokio::pin!(operation);
    tokio::select! {
        entered = gate.acquire() => entered.unwrap().forget(),
        result = &mut operation => panic!("cleanup completed before its fault cut: {result:?}"),
        _ = tokio::time::sleep(Duration::from_secs(10)) => panic!("cleanup cut timed out"),
    }
}

#[tokio::test]
async fn platform_egress_cleanup_ack_is_required_before_generation_removal() {
    // C1 remote admission closes but an accepted stream remains; E1 no Removed
    // or local-effects-cleared proof. C2 quiescence acknowledged; E2 the same
    // original allocation alone becomes Removed. No elapsed-time oracle.
    let slots = Slots::live();
    let mcp = McpSessions::new(None, awaken_time::system_clock());
    let prepared = refused_after_provision(&mcp, &slots, false).await;
    prepared.gate.quiescence.try_acquire().unwrap().forget();
    let mut drain = Box::pin(mcp.drain_selected_projection(slots.as_ref(), &prepared.owner));
    reach_cut(&prepared.gate.release_entered, drain.as_mut()).await;
    assert_retained(&mcp, &slots, &prepared);
    let route = current(&mcp, &slots, &prepared.request)
        .platform_egress
        .unwrap();
    assert_eq!(
        route.route().unwrap().url,
        "https://mediator.test/opaque-route"
    );
    assert_eq!(route.route().unwrap().expires_at_unix_ms, u64::MAX);
    assert_eq!(observed(&prepared.gate).phase.load(Ordering::SeqCst), 1);
    prepared.gate.quiescence.add_permits(1);
    drain.await.unwrap();
    assert_removed(&mcp, &slots, &prepared);
}

#[tokio::test]
async fn platform_egress_lost_provision_response_retains_the_original_cleanup_owner() {
    // The mediator issued the route, but no URL returned. The pre-IO owner is
    // still discoverable and releases that exact allocation without provision.
    let slots = Slots::live();
    let mcp = McpSessions::new(None, awaken_time::system_clock());
    let prepared = refused_after_provision(&mcp, &slots, true).await;
    assert!(prepared.owner.server.is_none());
    mcp.drain_selected_projection(slots.as_ref(), &prepared.owner)
        .await
        .unwrap();
    assert_removed(&mcp, &slots, &prepared);
}

#[tokio::test(start_paused = true)]
async fn platform_egress_release_failure_and_timeout_retain_the_same_allocation() {
    // F1 refused release and F2 the production timeout on an owned paused Tokio
    // clock both retain the original request/capability. Retry needs its ack.
    for fault in ["unavailable", "timeout"] {
        let slots = Slots::live();
        let mcp = McpSessions::new(None, awaken_time::system_clock());
        let prepared = refused_after_provision(&mcp, &slots, false).await;
        if fault == "unavailable" {
            prepared.gate.release_failures.store(1, Ordering::SeqCst);
        } else {
            prepared.gate.quiescence.try_acquire().unwrap().forget();
        }
        let error = mcp
            .drain_selected_projection(slots.as_ref(), &prepared.owner)
            .await
            .unwrap_err();
        assert_eq!(
            error.code,
            if fault == "unavailable" {
                "mcp_gateway_route_release_failed"
            } else {
                "mcp_gateway_route_release_timeout"
            }
        );
        assert_retained(&mcp, &slots, &prepared);
        assert_ne!(observed(&prepared.gate).phase.load(Ordering::SeqCst), 2);
        if fault == "timeout" {
            prepared.gate.quiescence.add_permits(1);
        }
        mcp.drain_selected_projection(slots.as_ref(), &prepared.owner)
            .await
            .unwrap();
        assert_removed(&mcp, &slots, &prepared);
    }
}

#[tokio::test]
async fn platform_egress_cancelled_drain_preserves_seal_and_waits_original_quiescence() {
    let slots = Slots::live();
    let mcp = McpSessions::new(None, awaken_time::system_clock());
    let prepared = refused_after_provision(&mcp, &slots, false).await;
    prepared.gate.quiescence.try_acquire().unwrap().forget();
    let mut drain = Box::pin(mcp.drain_selected_projection(slots.as_ref(), &prepared.owner));
    reach_cut(&prepared.gate.release_entered, drain.as_mut()).await;
    drop(drain); // Drop the owned future, including its original drain mutex.
    assert_retained(&mcp, &slots, &prepared);
    assert_eq!(observed(&prepared.gate).phase.load(Ordering::SeqCst), 1);
    let mut retry = Box::pin(mcp.drain_selected_projection(slots.as_ref(), &prepared.owner));
    reach_cut(&prepared.gate.release_entered, retry.as_mut()).await;
    assert_retained(&mcp, &slots, &prepared);
    prepared.gate.quiescence.add_permits(1);
    retry.await.unwrap();
    assert_removed(&mcp, &slots, &prepared);
}

#[tokio::test]
async fn platform_egress_lost_cleanup_ack_replays_the_same_completed_allocation() {
    let slots = Slots::live();
    let mcp = McpSessions::new(None, awaken_time::system_clock());
    let prepared = refused_after_provision(&mcp, &slots, false).await;
    prepared.gate.acknowledgment.try_acquire().unwrap().forget();
    let mut drain = Box::pin(mcp.drain_selected_projection(slots.as_ref(), &prepared.owner));
    reach_cut(&prepared.gate.quiesced, drain.as_mut()).await;
    assert_eq!(observed(&prepared.gate).phase.load(Ordering::SeqCst), 2);
    assert_retained(&mcp, &slots, &prepared);
    drop(drain);
    // The original allocation can observe its already completed physical
    // release; it needs neither another provision nor the lost first reply.
    mcp.drain_selected_projection(slots.as_ref(), &prepared.owner)
        .await
        .unwrap();
    assert_removed(&mcp, &slots, &prepared);
}

#[tokio::test]
async fn platform_egress_late_cleanup_never_releases_replacement_allocation() {
    // Same logical generation/request/URL, two different physical allocations.
    // Replace the slot while the captured original cleanup is waiting. Its
    // physical release may finish, but it cannot erase or release the successor.
    let slots = Slots::live();
    let mcp = McpSessions::new(None, awaken_time::system_clock());
    let original = refused_after_provision(&mcp, &slots, false).await;
    original.gate.quiescence.try_acquire().unwrap().forget();
    let mut drain = Box::pin(mcp.drain_selected_projection(slots.as_ref(), &original.owner));
    reach_cut(&original.gate.release_entered, drain.as_mut()).await;
    let other_slots = Slots::live();
    let successor = refused_after_provision(&mcp, &other_slots, false).await;
    assert!(original.owner.request == successor.owner.request);
    assert!(!Arc::ptr_eq(&original.owner.drain, &successor.owner.drain));
    slots.0.lock().unwrap().state.projections = vec![successor.owner.clone()];
    original.gate.quiescence.add_permits(1);
    assert_eq!(
        drain.await.unwrap_err().code,
        "mcp_generation_owner_changed"
    );
    assert_eq!(observed(&original.gate).phase.load(Ordering::SeqCst), 2);
    assert_eq!(observed(&successor.gate).phase.load(Ordering::SeqCst), 0);
    assert_retained(&mcp, &slots, &successor);
}

#[tokio::test]
async fn platform_egress_removed_label_cannot_erase_a_retained_allocation() {
    // Negative control for selectors that trust only a state label. An owned
    // physical allocation prevents tombstone replay and requires real drain.
    let slots = Slots::live();
    let mcp = McpSessions::new(None, awaken_time::system_clock());
    let prepared = refused_after_provision(&mcp, &slots, false).await;
    {
        let mut slot = slots.0.lock().unwrap();
        slot.root = Some(true);
        slot.state.projections[0].state = McpProjectionState::Removed;
        assert!(!slot.state.local_effects_cleared());
    }
    assert!(
        !mcp.forget_replayable_removed_projection(slots.as_ref(), &prepared.request)
            .unwrap()
    );
    mcp.drain_projections(
        slots.as_ref(),
        prepared.request.generation.session_id().as_str(),
        &[],
    )
    .await
    .unwrap();
    assert_removed(&mcp, &slots, &prepared);
}

#[tokio::test(start_paused = true)]
async fn platform_egress_replaced_preparation_owns_compensation_until_ack_even_without_its_waiter()
{
    // P1 the original slot is replaced during provision, with or without the
    // response; P2 release fails once, then seals but waits for quiescence and
    // its ack; P3 the caller is cancelled. The original owned task must retain
    // its exact allocation/activity through P1-P3, release unrelated Session
    // owners, and leave the same-named successor untouched.
    for lost_reply in [false, true] {
        let slots = Slots::live();
        let mcp = McpSessions::new(None, awaken_time::system_clock());
        let PlatformPreparation {
            request,
            gate,
            mut task,
        } = begin(&mcp, &slots);
        tokio::select! {
            entered = gate.entered.acquire() => entered.unwrap().forget(),
            result = &mut task => panic!("provision cut was not reached: {result:?}"),
            _ = tokio::time::sleep(Duration::from_secs(10)) => panic!("provision cut timed out"),
        }
        let original = current(&mcp, &slots, &request);
        let activity = original.staging.clone().unwrap();
        let mut completion = Box::pin(activity.wait());
        let other_slots = Slots::live();
        let successor = refused_after_provision(&mcp, &other_slots, false).await;
        assert!(original.request == successor.owner.request);
        slots.0.lock().unwrap().state.projections = vec![successor.owner.clone()];
        gate.lose_provision_reply
            .store(lost_reply, Ordering::SeqCst);
        gate.release_failures.store(1, Ordering::SeqCst);
        gate.quiescence.try_acquire().unwrap().forget();
        gate.acknowledgment.try_acquire().unwrap().forget();
        gate.release.add_permits(1);
        for _ in 0..2 {
            tokio::select! {
                entered = gate.release_entered.acquire() => entered.unwrap().forget(),
                result = &mut task => panic!("preparation lost its compensation: {result:?}"),
                _ = tokio::time::sleep(Duration::from_secs(10)) => panic!("release cut timed out"),
            }
            assert!(futures_util::poll!(completion.as_mut()).is_pending());
        }
        assert_eq!(observed(&gate).phase.load(Ordering::SeqCst), 1);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let registry = Arc::downgrade(&slots);
        drop(slots);
        assert!(
            registry.upgrade().is_none(),
            "compensation retains no Session registry"
        );
        gate.quiescence.add_permits(1);
        tokio::time::timeout(Duration::from_secs(10), gate.quiesced.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        assert!(
            futures_util::poll!(completion.as_mut()).is_pending(),
            "quiescence alone is not an ack"
        );
        gate.acknowledgment.add_permits(1);
        tokio::time::timeout(Duration::from_secs(10), completion.as_mut())
            .await
            .unwrap();
        assert_eq!(observed(&gate).phase.load(Ordering::SeqCst), 2);
        assert_eq!(observed(&successor.gate).phase.load(Ordering::SeqCst), 0);
        assert_retained(&mcp, &other_slots, &successor);
        mcp.drain_selected_projection(other_slots.as_ref(), &successor.owner)
            .await
            .unwrap();
        assert_removed(&mcp, &other_slots, &successor);
    }
}

#[tokio::test]
async fn platform_egress_unreadable_handoff_compensates_and_preserves_the_first_failure_class() {
    // H1 the original slot cannot be observed after provision; H2 provision
    // itself lost its response. H1 refuses a receipt with the original read
    // class; H1+H2 retains H2 as first cause. Both compensate the exact issued
    // allocation, retain Draining in the canonical slot, and can later drain.
    for lost_reply in [false, true] {
        let slots = Slots::live();
        let mcp = McpSessions::new(None, awaken_time::system_clock());
        let PlatformPreparation {
            request,
            gate,
            mut task,
        } = begin(&mcp, &slots);
        tokio::select! {
            entered = gate.entered.acquire() => entered.unwrap().forget(),
            result = &mut task => panic!("provision cut was not reached: {result:?}"),
            _ = tokio::time::sleep(Duration::from_secs(10)) => panic!("provision cut timed out"),
        }
        let original = current(&mcp, &slots, &request);
        slots.0.lock().unwrap().read_unavailable = true;
        gate.lose_provision_reply
            .store(lost_reply, Ordering::SeqCst);
        gate.release.add_permits(1);
        let error = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(
            error.code,
            if lost_reply {
                "mcp_gateway_route_unavailable"
            } else {
                "test_mcp_read_unavailable"
            }
        );
        assert_eq!(observed(&gate).phase.load(Ordering::SeqCst), 2);
        let retained = slots.0.lock().unwrap().state.projections[0].clone();
        assert_eq!(retained.state, McpProjectionState::Draining);
        assert!(Arc::ptr_eq(&retained.drain, &original.drain));
        assert!(Arc::ptr_eq(
            &retained.platform_egress.as_ref().unwrap().allocation,
            &original.platform_egress.as_ref().unwrap().allocation,
        ));
        assert!(!slots.0.lock().unwrap().state.local_effects_cleared());
        slots.0.lock().unwrap().read_unavailable = false;
        mcp.drain_selected_projection(slots.as_ref(), &retained)
            .await
            .unwrap();
        assert!(current(&mcp, &slots, &request).effects_cleared());
    }
}
