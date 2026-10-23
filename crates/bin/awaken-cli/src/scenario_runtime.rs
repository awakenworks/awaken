//! The runtime authority a scenario or test process selects.
//!
//! A selection with no durable coordinate (no storage dir, no database, the
//! SQLite defaults) gets the executor's in-memory reference authority over
//! an in-memory dispatch queue; any durable coordinate opens the
//! Coordinator's durable authority over the same selection. Product startup
//! never reaches this module: it opens the Coordinator persistence.

use std::sync::Arc;

use awaken_dispatch_erasure::RuntimeAuthority;
use awaken_dispatch_execution::{CoordinatorStorage, DispatchBackend, StoreKind};

/// Select the one runtime authority a scenario process shares between its
/// executor and its Session application.
pub async fn scenario_runtime_authority(
    storage: &CoordinatorStorage,
    clock: Arc<dyn awaken_time::WallClock>,
) -> Result<Arc<dyn RuntimeAuthority>, String> {
    if storage.storage_dir.is_none()
        && storage.database_url.is_none()
        && storage.store == StoreKind::Sqlite
        && storage.dispatch_backend == DispatchBackend::Sqlite
    {
        let dispatch = awaken_dispatch::AnyCoordinatorDispatch::from_durable_store(
            awaken_dispatch::MemoryDispatchStore::new(clock.clone()),
            None,
        );
        return Ok(Arc::new(
            awaken_session_executor::EphemeralRuntimeAuthority::with_dispatch(
                clock,
                Arc::new(dispatch),
            ),
        ));
    }
    awaken_coordinator::open_durable_runtime_authority(storage, clock).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn volatile_selection_is_the_reference_authority_with_its_dispatch_roles() {
        // Decision table: R0 SQLite commit + SQLite dispatch + no storage dir
        // + no database -> one explicitly injected reference authority with
        // its original atomic Session-continuation and operational-feed
        // roles. Durable selections open the Coordinator's authority; its
        // fail-closed rules are owned by the Coordinator persistence tests.
        let missing = awaken_session_executor::DeploymentConfig::ephemeral().coordinator_storage();
        let reference = scenario_runtime_authority(&missing, awaken_time::system_clock())
            .await
            .expect("R0 scenario SQLite runtime needs no shared backend");
        fn continuation(_: &dyn awaken_dispatch::SessionContinuationOutbox) {}
        continuation(reference.dispatch_store().as_ref());
        use awaken_dispatch::DispatchOperationalFeed as _;
        assert!(
            reference
                .dispatch_store()
                .events_after(awaken_dispatch::DispatchCursor(0), 1, None)
                .await
                .is_ok(),
            "R0 exact operational feed survives erasure"
        );
    }
}
