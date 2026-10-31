//! Coordinator-owned WorkerDirectory adapter construction.
//!
//! Backend selection stays at the server root and returns an explicit handle to
//! process startup. There is deliberately no process global and no
//! implicit volatile fallback.

use std::path::Path;
use std::sync::Arc;

use awaken_worker_registry::{PostgresWorkerDirectory, SqliteWorkerDirectory, WorkerDirectory};

pub type WorkerDirectoryHandle = Arc<dyn WorkerDirectory>;

pub(crate) async fn open_sqlite(storage_dir: &Path) -> Result<WorkerDirectoryHandle, String> {
    std::fs::create_dir_all(storage_dir).map_err(|error| {
        format!(
            "create Worker registry directory {}: {error}",
            storage_dir.display()
        )
    })?;
    let path = storage_dir.join("worker-registry.db");
    SqliteWorkerDirectory::open(&path)
        .await
        .map(|directory| Arc::new(directory) as WorkerDirectoryHandle)
        .map_err(|error| format!("open Worker registry {}: {error}", path.display()))
}

pub(crate) async fn open_postgres_pool(
    pool: sqlx::PgPool,
    verify_only: bool,
) -> Result<WorkerDirectoryHandle, String> {
    let directory = if verify_only {
        PostgresWorkerDirectory::with_existing_pool(pool).await
    } else {
        PostgresWorkerDirectory::with_pool(pool).await
    };
    directory
        .map(|directory| Arc::new(directory) as WorkerDirectoryHandle)
        .map_err(|error| error.to_string())
}

#[cfg(any(test, feature = "test-support"))]
pub fn test_directory() -> WorkerDirectoryHandle {
    Arc::new(awaken_worker_registry::MemoryWorkerDirectory::new())
}
