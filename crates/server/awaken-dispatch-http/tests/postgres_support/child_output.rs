//! Drain both fixture pipes while the child runs, keeping its exit deadline.
use std::io::{self, Read};
use std::process::{Child, Output};
use std::thread;
use std::time::{Duration, Instant};

pub(super) struct CapturedChild {
    pub output: Output,
    pub timed_out: bool,
}

struct ReapChild(Child);

impl Drop for ReapChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn drain(mut stream: impl Read) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    stream.read_to_end(&mut output)?;
    Ok(output)
}

pub(super) fn capture(child: Child, budget: Duration) -> io::Result<CapturedChild> {
    let mut child = ReapChild(child);
    // Waiting for exit before reading can deadlock on a panic/backtrace larger
    // than the OS pipe buffer. Both pipes need independent readers; reading
    // either one to EOF first can block the other in exactly the same way.
    let stdout = thread::spawn({
        let stream = child.0.stdout.take().expect("fixture stdout is piped");
        move || drain(stream)
    });
    let stderr = thread::spawn({
        let stream = child.0.stderr.take().expect("fixture stderr is piped");
        move || drain(stream)
    });
    let deadline = Instant::now() + budget;
    let outcome = (|| {
        // awaken-allow(wait): enforce the actual child process exit deadline; this polls no Session projection.
        loop {
            if let Some(status) = child.0.try_wait()? {
                return Ok((status, false));
            }
            if Instant::now() >= deadline {
                if let Err(error) = child.0.kill() {
                    // A natural exit may race the deadline's kill request.
                    if let Some(status) = child.0.try_wait()? {
                        return Ok((status, false));
                    }
                    return Err(error);
                }
                return child.0.wait().map(|status| (status, true));
            }
            thread::sleep(Duration::from_millis(10));
        }
    })();
    if outcome.is_err() {
        let _ = child.0.kill();
        let _ = child.0.wait();
    }
    let stdout = stdout
        .join()
        .map_err(|_| io::Error::other("fixture stdout reader panicked"))??;
    let stderr = stderr
        .join()
        .map_err(|_| io::Error::other("fixture stderr reader panicked"))??;
    let (status, timed_out) = outcome?;
    Ok(CapturedChild {
        output: Output {
            status,
            stdout,
            stderr,
        },
        timed_out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    const CHILD_MODE: &str = "AWAKEN_HTTP_POSTGRES_CAPTURE_CHILD";
    const BYTES: usize = 128 * 1024;

    fn write_fixture_output() {
        io::stdout().write_all(&vec![b'O'; BYTES]).unwrap();
        io::stdout().flush().unwrap();
        io::stderr().write_all(&vec![b'E'; BYTES]).unwrap();
        io::stderr().flush().unwrap();
    }

    fn fixture_child(mode: &str) -> Child {
        let current = thread::current();
        let name = current.name().expect("libtest names the current test");
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CHILD_MODE, mode)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    #[test]
    fn configuration_child_capture_drains_both_streams_past_pipe_capacity() {
        // Decision rows: large stdout THEN large stderr => normal exit, exact
        // bytes retained. Each stream exceeds Windows and Linux pipe capacity.
        // Re-execute this real test body instead of adding a no-op fixture test.
        if std::env::var(CHILD_MODE).as_deref() == Ok("large") {
            write_fixture_output();
            return;
        }
        let result = capture(fixture_child("large"), Duration::from_secs(5)).unwrap();
        assert!(
            !result.timed_out,
            "large output must not consume the exit deadline"
        );
        assert!(result.output.status.success());
        assert_eq!(
            result
                .output
                .stdout
                .iter()
                .filter(|byte| **byte == b'O')
                .count(),
            BYTES
        );
        assert_eq!(
            result
                .output
                .stderr
                .iter()
                .filter(|byte| **byte == b'E')
                .count(),
            BYTES
        );
    }

    #[test]
    fn configuration_child_capture_keeps_its_deadline_and_partial_output() {
        // Large output is not permission to wait forever: drain it, then kill
        // and reap a child that stays alive, preserving both diagnostic streams.
        if std::env::var(CHILD_MODE).as_deref() == Ok("stall") {
            write_fixture_output();
            // awaken-allow(wait): deliberately outlive the capture deadline so the parent must kill and reap this child.
            thread::sleep(Duration::from_secs(60));
            return;
        }
        let result = capture(fixture_child("stall"), Duration::from_secs(3)).unwrap();
        assert!(result.timed_out);
        assert!(!result.output.status.success());
        assert_eq!(
            result
                .output
                .stdout
                .iter()
                .filter(|byte| **byte == b'O')
                .count(),
            BYTES
        );
        assert_eq!(
            result
                .output
                .stderr
                .iter()
                .filter(|byte| **byte == b'E')
                .count(),
            BYTES
        );
    }
}
