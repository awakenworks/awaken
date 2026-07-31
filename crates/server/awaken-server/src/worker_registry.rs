//! Composition owner for the process-wide worker directory. The host substrate
//! consumes only the injected port; backend selection stays at the server root so
//! `awaken-runtime-host` does not become a registry/store god hub.

use std::sync::{Arc, OnceLock};

use awaken_worker_registry::{MemoryWorkerDirectory, PostgresWorkerDirectory, WorkerDirectory};

static DIRECTORY: OnceLock<Arc<dyn WorkerDirectory>> = OnceLock::new();

pub type WorkerDirectoryHandle = Arc<dyn WorkerDirectory>;

#[derive(Clone, Copy)]
enum SchemaAccess {
    Migrate,
    Verify,
}

pub async fn init_postgres(url: &str) -> Result<(), String> {
    init_postgres_with(url, SchemaAccess::Migrate).await
}

pub async fn init_existing_postgres(url: &str) -> Result<(), String> {
    init_postgres_with(url, SchemaAccess::Verify).await
}

async fn init_postgres_with(url: &str, schema: SchemaAccess) -> Result<(), String> {
    if DIRECTORY.get().is_some() {
        return Ok(());
    }
    let directory = match schema {
        SchemaAccess::Migrate => PostgresWorkerDirectory::connect(url).await,
        SchemaAccess::Verify => PostgresWorkerDirectory::connect_existing(url).await,
    }
    .map_err(|error| error.to_string())?;
    let _ = DIRECTORY.set(Arc::new(directory));
    Ok(())
}

pub async fn migrate_postgres(url: &str) -> Result<(), String> {
    PostgresWorkerDirectory::connect(url)
        .await
        .map(drop)
        .map_err(|error| error.to_string())
}

pub fn inject(directory: Arc<dyn WorkerDirectory>) {
    let _ = DIRECTORY.set(directory);
}

/// Shared Worker observation authority for outer composition adapters such as
/// publication readiness. Consumers receive only the neutral directory port.
pub fn shared() -> WorkerDirectoryHandle {
    if let Some(directory) = DIRECTORY.get() {
        return directory.clone();
    }
    let directory: Arc<dyn WorkerDirectory> = Arc::new(MemoryWorkerDirectory::new());
    let _ = DIRECTORY.set(directory);
    DIRECTORY.get().cloned().expect("worker directory set")
}
