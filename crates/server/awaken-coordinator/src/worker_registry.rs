//! Coordinator-owned WorkerDirectory adapter construction.
//!
//! Backend selection stays at the server root and returns an explicit handle to
//! the process composition. There is deliberately no process global and no
//! implicit volatile fallback.

use std::path::Path;
use std::sync::Arc;

use awaken_worker_registry::{PostgresWorkerDirectory, SqliteWorkerDirectory, WorkerDirectory};

pub type WorkerDirectoryHandle = Arc<dyn WorkerDirectory>;

pub fn open_sqlite(storage_dir: &Path) -> Result<WorkerDirectoryHandle, String> {
    std::fs::create_dir_all(storage_dir).map_err(|error| {
        format!(
            "create Worker registry directory {}: {error}",
            storage_dir.display()
        )
    })?;
    let path = storage_dir.join("worker-registry.db");
    SqliteWorkerDirectory::open(&path)
        .map(|directory| Arc::new(directory) as WorkerDirectoryHandle)
        .map_err(|error| format!("open Worker registry {}: {error}", path.display()))
}

pub async fn open_postgres(
    url: &str,
    max_connections: u32,
) -> Result<WorkerDirectoryHandle, String> {
    PostgresWorkerDirectory::connect(url, max_connections)
        .await
        .map(|directory| Arc::new(directory) as WorkerDirectoryHandle)
        .map_err(|error| error.to_string())
}

pub async fn open_existing_postgres(
    url: &str,
    max_connections: u32,
) -> Result<WorkerDirectoryHandle, String> {
    PostgresWorkerDirectory::connect_existing(url, max_connections)
        .await
        .map(|directory| Arc::new(directory) as WorkerDirectoryHandle)
        .map_err(|error| error.to_string())
}

pub async fn migrate_postgres(url: &str, max_connections: u32) -> Result<(), String> {
    PostgresWorkerDirectory::connect(url, max_connections)
        .await
        .map(drop)
        .map_err(|error| error.to_string())
}

#[cfg(any(test, feature = "test-support"))]
pub fn test_directory() -> WorkerDirectoryHandle {
    Arc::new(awaken_worker_registry::MemoryWorkerDirectory::new())
}
