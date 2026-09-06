//! File observation/fault adapter for the existing real HTTP scenario server.
//! The contract port emits observations; this adapter never selects transitions.
use awaken_session_contract::realization_observation::{self, RealizationObserver};
use std::{
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub fn install_from_environment() {
    if let Some(directory) = std::env::var_os("AWAKEN_REALIZATION_OBSERVATION_DIR") {
        realization_observation::install(Arc::new(FileObserver {
            directory: directory.into(),
            serial: Mutex::new(()),
        }));
    }
}
struct FileObserver {
    directory: PathBuf,
    serial: Mutex<()>,
}
#[async_trait::async_trait]
impl RealizationObserver for FileObserver {
    async fn observe(&self, point: &'static str, session_id: &str, data: serde_json::Value) {
        let should_pause = {
            let _guard = self.serial.lock().unwrap();
            let bytes = match std::fs::read(self.directory.join("armed.json")) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
                Err(error) => panic!("read observation configuration: {error}"),
            };
            let config: serde_json::Value =
                serde_json::from_slice(&bytes).expect("observation config");
            if config["session_id"] != session_id {
                return;
            }
            let path = self.directory.join("trace.ndjson");
            let sequence = match std::fs::read_to_string(&path) {
                Ok(trace) => trace.lines().count(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
                Err(error) => panic!("read observation sequence: {error}"),
            };
            let event = serde_json::json!({"sequence": sequence, "session": session_id, "point": point, "data": data});
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(file, "{}", event).unwrap();
            file.sync_all().unwrap();
            let pause = config["boundary"] == point;
            if pause {
                std::fs::write(
                    self.directory.join("boundary.tmp"),
                    serde_json::to_vec(&event).unwrap(),
                )
                .unwrap();
                std::fs::rename(
                    self.directory.join("boundary.tmp"),
                    self.directory.join("boundary.json"),
                )
                .unwrap();
            }
            pause
        };
        // The parent waits for this exact persisted observation, then SIGKILLs
        // the real server. Drop the I/O lock before suspending the phase owner.
        if should_pause {
            std::future::pending::<()>().await;
        }
    }
}
