//! Test-only process-crash evidence primitive.
//!
//! This crate owns no product state and implements no domain transition. It
//! gives existing integration suites one shared parent-side process-kill
//! harness. Each domain test still owns its durable marker and assertions.

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
