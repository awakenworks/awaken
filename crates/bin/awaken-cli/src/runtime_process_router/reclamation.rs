// @formal evidence resource_reclamation.physical_purge_requires_zero_references_leases_and_fence
//! Start reclamation after the complete protected process surfaces are assembled.

use awaken_time::WallClockExt as _;
use std::sync::Arc;

pub(super) async fn install(
    service_lifecycle: &awaken_service_lifecycle::ServiceLifecycle,
    resource_reclaimer: Arc<awaken_resource_reclaimer::ResourceReclaimer>,
    clock: Arc<dyn awaken_time::WallClock>,
) {
    if let Ok(summary) = resource_reclaimer.reconcile(clock.now_ms(), 256).await
        && summary.completed > 0
    {
        eprintln!("reclaimed {} durable resource(s)", summary.completed);
    }
    let recurring_resource_reclaimer = resource_reclaimer;
    let reclamation_clock = clock;
    service_lifecycle.spawn("resources-reclamation", move |cancel| async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = interval.tick() => {}
            }
            if let Err(error) = recurring_resource_reclaimer
                .reconcile(reclamation_clock.now_ms(), 256)
                .await
            {
                eprintln!("resource reclamation retry remains pending: {error}");
            }
        }
        Ok(())
    });
}
