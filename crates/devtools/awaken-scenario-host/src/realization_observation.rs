//! File observation/fault adapter for the existing real HTTP scenario server.
//! The contract port emits observations; this adapter never selects transitions.
use awaken_session_realization_contract::realization_observation::{self, RealizationObserver};
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
    async fn observe(
        &self,
        point: &'static str,
        session_id: &awaken_session_types::SessionId,
        data: serde_json::Value,
    ) {
        let should_pause = {
            let _guard = self.serial.lock().unwrap();
            let bytes = match std::fs::read(self.directory.join("armed.json")) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
                Err(error) => panic!("read observation configuration: {error}"),
            };
            let config: serde_json::Value =
                serde_json::from_slice(&bytes).expect("observation config");
            if config["session_id"] != session_id.as_str() {
                return;
            }
            let path = self.directory.join("trace.ndjson");
            let sequence = match std::fs::read_to_string(&path) {
                Ok(trace) => trace.lines().count(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
                Err(error) => panic!("read observation sequence: {error}"),
            };
            if !starts_observation_window(sequence, point) {
                return;
            }
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

/// Arming selects a Session, not a pass. The canonical recovery supervisor may
/// already be inside an ordinary realization pass for that Session (its root
/// was read before arming), and that pass's remaining projection/effect
/// observations would otherwise open the trace without the observed root the
/// refinement relation starts from, or satisfy the fault boundary on behalf of
/// a pass the parent did not start. An empty trace therefore opens only at the
/// root read that begins a pass; once open, every later observation is kept.
fn starts_observation_window(recorded: usize, point: &str) -> bool {
    recorded > 0 || point == "begin-before"
}

#[cfg(test)]
mod tests {
    use super::{FileObserver, RealizationObserver, starts_observation_window};
    use serde_json::json;
    use std::sync::Mutex;

    #[test]
    fn an_armed_trace_opens_only_at_a_pass_root_read() {
        // Cause/effect decision table: C1 the armed trace is empty; C2 the
        // observation is the pass-opening root read. E1 record it; E2 drop an
        // in-flight pass's tail. Rules: R1=C1+C2=>E1; R2=C1+!C2=>E2;
        // R3=!C1=>E1 for every point, including roots of later phases.
        assert!(starts_observation_window(0, "begin-before"), "R1");
        for point in [
            "projection",
            "complete",
            "stage",
            "published",
            "ack-after",
            "begin-after",
        ] {
            assert!(!starts_observation_window(0, point), "R2 {point}");
        }
        for point in [
            "begin-before",
            "projection",
            "complete",
            "stage",
            "ack-after",
        ] {
            assert!(starts_observation_window(3, point), "R3 {point}");
        }
    }

    #[tokio::test]
    async fn a_pass_in_flight_at_arming_neither_opens_the_trace_nor_takes_the_boundary() {
        // Cause/effect graph: C1 the parent arms Session S with boundary
        // `stage` while a supervisor pass for S already read its root; C2 that
        // pass then emits projection, stage and complete; C3 the parent's own
        // pass begins with its root read and reaches stage. E1 no C2 point is
        // recorded and the C2 stage does not publish the pause marker; E2 the
        // trace opens at C3's begin-before with sequence 0; E3 a later
        // interleaved point from any pass is recorded once the window is open.
        // Rule: C1+C2+C3 => E1+E2+E3 (the C3 stage pause is not awaited here).
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("armed.json"),
            json!({"session_id": "S", "boundary": "stage"}).to_string(),
        )
        .unwrap();
        let observer = FileObserver {
            directory: directory.path().into(),
            serial: Mutex::new(()),
        };
        for point in ["projection", "stage", "complete"] {
            // A captured boundary suspends forever by design; bound it so a
            // regression fails here instead of hanging the test binary.
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                observer.observe(
                    point,
                    &awaken_session_types::SessionId::from("S"),
                    json!({}),
                ),
            )
            .await
            .unwrap_or_else(|_| panic!("E1 in-flight {point} took the boundary"));
        }
        assert!(!directory.path().join("trace.ndjson").exists(), "E1 trace");
        assert!(
            !directory.path().join("boundary.json").exists(),
            "E1 boundary"
        );
        observer
            .observe(
                "begin-before",
                &awaken_session_types::SessionId::from("S"),
                json!({"attachments": []}),
            )
            .await;
        observer
            .observe(
                "complete",
                &awaken_session_types::SessionId::from("S"),
                json!({}),
            )
            .await;
        let trace = std::fs::read_to_string(directory.path().join("trace.ndjson")).unwrap();
        let events = trace
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 2, "E2+E3");
        assert_eq!(
            (events[0]["sequence"].as_u64(), events[0]["point"].as_str()),
            (Some(0), Some("begin-before")),
            "E2"
        );
        assert_eq!(events[1]["point"], "complete", "E3");
    }
}
