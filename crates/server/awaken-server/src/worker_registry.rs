//! Composition owner for the process-wide worker directory. The host substrate
//! consumes only the injected port; backend selection stays at the server root so
//! `awaken-runtime-host` does not become a registry/store god hub.

use std::sync::{Arc, OnceLock};

use awaken_worker_registry::{MemoryWorkerDirectory, PostgresWorkerDirectory, WorkerDirectory};

static DIRECTORY: OnceLock<Arc<dyn WorkerDirectory>> = OnceLock::new();

pub async fn init_postgres(url: &str) -> Result<(), String> {
    if DIRECTORY.get().is_some() {
        return Ok(());
    }
    let directory = PostgresWorkerDirectory::connect(url)
        .await
        .map_err(|error| error.to_string())?;
    let _ = DIRECTORY.set(Arc::new(directory));
    Ok(())
}

pub fn inject(directory: Arc<dyn WorkerDirectory>) {
    let _ = DIRECTORY.set(directory);
}

/// Shared Worker observation authority for outer composition adapters such as
/// publication readiness. Consumers receive only the neutral directory port.
pub fn shared() -> Arc<dyn WorkerDirectory> {
    if let Some(directory) = DIRECTORY.get() {
        return directory.clone();
    }
    let directory: Arc<dyn WorkerDirectory> = Arc::new(MemoryWorkerDirectory::new());
    let _ = DIRECTORY.set(directory);
    DIRECTORY.get().cloned().expect("worker directory set")
}
