//! Composition owner for the process-wide worker directory. The host substrate
//! consumes only the injected port; backend selection stays at the server root so
//! `awaken-runtime-host` does not become a registry/store god hub.

use std::sync::{Arc, OnceLock};

use awaken_worker_registry::{
    MemoryWorkerDirectory, PostgresWorkerDirectory, SqliteWorkerDirectory, WorkerDirectory,
};

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

pub(crate) fn shared() -> Arc<dyn WorkerDirectory> {
    if let Some(directory) = DIRECTORY.get() {
        return directory.clone();
    }
    let directory: Arc<dyn WorkerDirectory> = match awaken_runtime_host::DeploymentConfig::from_env(
    )
    .dispatch_backend
    {
        awaken_runtime_host::DispatchBackend::Postgres => {
            panic!(
                "Postgres dispatch requires awaken_server::init_postgres_worker_registry before mount"
            )
        }
        awaken_runtime_host::DispatchBackend::Sqlite => {
            match std::env::var("AWAKEN_STORAGE_DIR")
                .ok()
                .filter(|value| !value.is_empty())
            {
                Some(directory) => {
                    std::fs::create_dir_all(&directory)
                        .expect("create worker registry storage directory");
                    Arc::new(
                        SqliteWorkerDirectory::open(&format!("{directory}/worker_registry.db"))
                            .expect("open worker registry database"),
                    )
                }
                None => Arc::new(MemoryWorkerDirectory::new()),
            }
        }
    };
    let _ = DIRECTORY.set(directory);
    DIRECTORY.get().cloned().expect("worker directory set")
}
