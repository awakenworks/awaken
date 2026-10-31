//! Test-only execution and process-crash evidence primitives.
//!
//! This crate owns no product state and implements no domain transition. It
//! gives existing integration suites one shared parent-side process-kill
//! harness and fixture admission; an admitted PostgreSQL fixture is a fresh
//! namespace per admission. Each domain test still owns its durable marker,
//! tables and behavioral assertions.

#[cfg(feature = "postgres")]
mod fixture;
mod nats_fixture;
#[cfg(feature = "postgres")]
pub use fixture::{
    assert_postgres_fixture_child_contract, postgres_fixture_pool, postgres_fixture_url,
};
pub use nats_fixture::{
    assert_nats_fixture_child_contract, nats_fixture_url, report_nats_fixture_exercised,
};

#[cfg(feature = "postgres")]
mod postgres_crash;
#[cfg(feature = "postgres")]
pub use postgres_crash::postgres_crash_pool;

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Parent-side runner for a real child-process crash boundary.
pub struct CrashProcess {
    test_name: String,
    marker: PathBuf,
    environment: Vec<(OsString, OsString)>,
    timeout: Duration,
}

impl CrashProcess {
    pub fn new(test_name: impl Into<String>, marker: impl Into<PathBuf>) -> Self {
        Self {
            test_name: test_name.into(),
            marker: marker.into(),
            environment: Vec::new(),
            timeout: Duration::from_secs(10),
        }
    }

    pub fn env(mut self, name: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.environment
            .push((name.as_ref().to_os_string(), value.as_ref().to_os_string()));
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Spawn this integration-test binary, wait until the child publishes its
    /// durable-boundary marker, then terminate it without graceful cleanup.
    #[expect(
        clippy::disallowed_methods,
        reason = "the crash fixture bounds its child-process wait on the host monotonic clock"
    )]
    pub fn run(self) -> Result<ExitStatus, String> {
        let mut command = Command::new(std::env::current_exe().map_err(display)?);
        command
            .arg("--exact")
            .arg(&self.test_name)
            .arg("--nocapture")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (name, value) in &self.environment {
            command.env(name, value);
        }
        let mut child = command.spawn().map_err(display)?;
        let deadline = Instant::now() + self.timeout;
        while !self.marker.exists() && Instant::now() < deadline {
            if let Some(status) = child.try_wait().map_err(display)? {
                return Err(format!(
                    "crash child {} exited before marker {} with {status}",
                    self.test_name,
                    self.marker.display()
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if !self.marker.exists() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "crash child {} did not reach marker {} within {:?}",
                self.test_name,
                self.marker.display(),
                self.timeout
            ));
        }
        child.kill().map_err(display)?;
        child.wait().map_err(display)
    }
}

fn display(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// Emit raw observations only when the existing formal runner requests them.
/// Models and projection contracts remain owned by formal/, never by this I/O
/// helper. Distinct scenario names avoid parallel-test artifact replacement.
#[expect(
    clippy::disallowed_methods,
    reason = "the reliability fixture reads which live service the test run provides from the environment"
)]
pub fn write_observation_trace(name: &str, family: &str, steps: serde_json::Value) {
    let Ok(directory) = std::env::var("AWAKEN_OBSERVATION_TRACE_DIR") else {
        return;
    };
    assert!(name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'));
    let directory = PathBuf::from(directory);
    std::fs::create_dir_all(&directory).expect("observation trace directory");
    let value = serde_json::json!({"schema_version": 4, "projection_id": "session-observations-v4", "name": name, "family": family, "steps": steps});
    std::fs::write(
        directory.join(format!("{name}.json")),
        serde_json::to_vec_pretty(&value).unwrap(),
    )
    .expect("observation trace");
}

#[cfg(feature = "tracing")]
pub mod events;
