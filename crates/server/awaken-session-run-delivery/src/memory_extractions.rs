// @formal surface obligation=memory_extraction.unknown_execution_never_reenters_retry
// @formal reason: The original process-owned extraction repository, claim owner and background
// @formal reason: registration move without the model/Sandbox executor. The existing Memory controller
// @formal reason: still owns every durable claim, lease, retry, mutation and receipt transition.
//! Recover Memory intents through their existing controller and process task registry.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_ext_memory::{
    MemoryExtractionController, MemoryExtractionDriver, MemoryExtractionError,
    MemoryExtractionRepository,
};

use crate::{BackgroundRuns, BackgroundWorkClass};

static EXTRACTION_OWNER_SEQ: AtomicU64 = AtomicU64::new(1);

/// Process-owned collaborators for Memory recovery, shared by the embedded
/// extractor and the Coordinator's Worker-dispatched extractor. This is not an
/// additional outbox or scheduler; the controller drives the injected repository.
pub struct MemoryExtractions {
    repository: Arc<dyn MemoryExtractionRepository>,
    background: Arc<BackgroundRuns>,
    clock: Arc<dyn awaken_time::WallClock>,
    owner: String,
}

impl MemoryExtractions {
    pub fn new(
        repository: Arc<dyn MemoryExtractionRepository>,
        background: Arc<BackgroundRuns>,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self {
            repository,
            background,
            clock,
            owner: format!(
                "memory-extractor:{}:{}",
                std::process::id(),
                EXTRACTION_OWNER_SEQ.fetch_add(1, Ordering::Relaxed)
            ),
        }
    }

    /// The originally selected durable authority, never a fallback store.
    #[must_use]
    pub fn repository(&self) -> Arc<dyn MemoryExtractionRepository> {
        self.repository.clone()
    }

    #[must_use]
    pub fn background(&self) -> Arc<BackgroundRuns> {
        self.background.clone()
    }

    /// Commands and recovery use the same controller identity and clock.
    #[must_use]
    pub fn controller(&self) -> MemoryExtractionController {
        MemoryExtractionController::new(
            self.repository.clone(),
            self.owner.clone(),
            self.clock.clone(),
        )
    }

    /// Recover one already-ready binding. The binding's driver is the sole
    /// selector; a failed preflight remains visible and an empty scan spawns nothing.
    pub async fn recover(
        &self,
        session: &str,
        driver: Arc<dyn MemoryExtractionDriver>,
    ) -> Result<bool, MemoryExtractionError> {
        let controller = self.controller();
        if !controller.has_recoverable(driver.as_ref()).await? {
            return Ok(false);
        }
        self.background
            .spawn(
                BackgroundWorkClass::ExternalDurable {
                    durable_intent_id: format!("memory-extraction:{session}"),
                },
                async move {
                    controller.drive_recoverable(driver.as_ref()).await;
                },
            )
            .await;
        Ok(true)
    }
}
