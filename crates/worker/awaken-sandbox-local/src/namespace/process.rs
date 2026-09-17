//! Bind bubblewrap's trusted child report to the existing local lifecycle port.
//! No PID discovery through task-visible files, argv, or host process scans.
use super::{LocalProcess, TokioCommand, pc};

pub(super) struct LaunchInfo {
    #[cfg(target_os = "linux")]
    report: tokio::net::UnixStream,
}

impl LaunchInfo {
    pub(super) fn attach(command: &mut TokioCommand) -> Result<Self, pc::SandboxError> {
        #[cfg(target_os = "linux")]
        {
            use command_fds::{CommandFdExt, FdMapping};
            // LocalProcess::spawn owns process-group and cancellation policy;
            // this adapter only supplies the trusted namespace identity report.
            let (read, write) = std::os::unix::net::UnixStream::pair()
                .map_err(|error| pc::SandboxError::backend("attach", error))?;
            read.set_nonblocking(true)
                .map_err(|error| pc::SandboxError::backend("attach", error))?;
            command
                .as_std_mut()
                .fd_mappings(vec![FdMapping {
                    parent_fd: write.into(),
                    child_fd: 3,
                }])
                .map_err(|error| pc::SandboxError::backend("attach", error))?;
            command.args(["--info-fd", "3"]);
            Ok(Self {
                report: tokio::net::UnixStream::from_std(read)
                    .map_err(|error| pc::SandboxError::backend("attach", error))?,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = command;
            Ok(Self {})
        }
    }

    pub(super) async fn bind(
        self,
        process: LocalProcess,
    ) -> Result<LocalProcess, pc::SandboxError> {
        #[cfg(not(target_os = "linux"))]
        {
            Ok(process)
        }
        #[cfg(target_os = "linux")]
        {
            use pc::ProcessHandle as _;
            use std::time::Duration;
            use tokio::io::AsyncReadExt as _;
            let mut process = process;
            let binding = async {
                // bubblewrap closes this fd in both monitor and namespace child
                // before exec. The application cannot forge the reported identity.
                let mut bytes = Vec::new();
                self.report
                    .take(4097)
                    .read_to_end(&mut bytes)
                    .await
                    .map_err(|error| pc::SandboxError::backend("bind", error))?;
                let group = reported_group(&bytes)?;
                process.bind_private_signal_group(group).await
            };
            let result = tokio::time::timeout(Duration::from_secs(6), binding)
                .await
                .map_err(|_| pc::SandboxError::timeout("bubblewrap process report timed out"))
                .and_then(|result| result);
            if let Err(cause) = result {
                // Startup failed before the handle could be published. Reap the
                // same owned launcher, preserving its original isolation boundary.
                process.signal(pc::Signal::Kill).await?;
                tokio::time::timeout(Duration::from_secs(5), process.wait())
                    .await
                    .map_err(|_| {
                        pc::SandboxError::timeout("bubblewrap setup cleanup timed out")
                    })??;
                return Err(cause);
            }
            Ok(process)
        }
    }
}

#[cfg(target_os = "linux")]
fn reported_group(bytes: &[u8]) -> Result<u32, pc::SandboxError> {
    #[derive(serde::Deserialize)]
    struct Report {
        #[serde(rename = "child-pid")]
        child_pid: u32,
    }
    if bytes.len() > 4096 {
        return Err(pc::SandboxError::invalid(
            "bubblewrap process report is oversized",
        ));
    }
    let report: Report = serde_json::from_slice(bytes)
        .map_err(|error| pc::SandboxError::backend("reported_group", error))?;
    Ok(report.child_pid)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn private_report_requires_one_bounded_unsigned_host_pid() {
        // R1 complete positive host PID + optional namespace fields -> bind.
        // R2 malformed/truncated/empty/missing/duplicate/negative/overflow or
        // oversized report -> reject. LocalProcess owns valid group ranges; the
        // next decision table covers those without a second numeric policy here.
        assert_eq!(
            reported_group(br#"{"child-pid":42,"pid-namespace":7}"#).unwrap(),
            42
        );
        for bytes in [
            b"".as_slice(),
            b"{",
            b"{}",
            br#"{"child-pid":-1}"#,
            br#"{"child-pid":4294967296}"#,
            br#"{"child-pid":2,"child-pid":3}"#,
            br#"{"child-pid":"42"}"#,
        ] {
            assert!(reported_group(bytes).is_err(), "{bytes:?}");
        }
        assert!(reported_group(&vec![b' '; 4097]).is_err());
    }

    #[tokio::test]
    async fn failed_or_cancelled_private_report_reaps_the_unpublished_launcher() {
        // R3 malformed + closed peer -> parse error and physical exit.
        // R3b zero/one/overflow -> canonical group validation error and exit.
        // R4 silent live peer -> bounded timeout and physical exit.
        // R5 caller drops binding while peer is live -> kill-on-drop owns cleanup.
        // No row publishes a handle or guesses a fallback PID. Only test-owned
        // children are observed; the silence row keeps its peer alive explicitly.
        use std::time::Duration;
        use tokio::io::AsyncWriteExt as _;
        for mode in [
            "malformed",
            "zero",
            "one",
            "overflow",
            "silent",
            "cancelled",
        ] {
            let (report, mut peer) = tokio::net::UnixStream::pair().unwrap();
            let payload = match mode {
                "malformed" => b"not a report".as_slice(),
                "zero" => br#"{"child-pid":0}"#,
                "one" => br#"{"child-pid":1}"#,
                "overflow" => br#"{"child-pid":2147483648}"#,
                _ => b"",
            };
            peer.write_all(payload).await.unwrap();
            let peer = matches!(mode, "silent" | "cancelled").then_some(peer);
            let mut command = TokioCommand::new("/bin/sh");
            command.args(["-c", "exec sleep 30"]);
            // Use the sole LocalProcess spawn boundary for R5: its drop policy
            // must retain cleanup when private-report binding is cancelled.
            let launch = if mode == "cancelled" {
                LaunchInfo::attach(&mut command).unwrap()
            } else {
                LaunchInfo { report }
            };
            let process = LocalProcess::spawn(&mut command).unwrap();
            let pid = pc::ProcessHandle::id(&process).to_owned();
            let deadline = if mode == "cancelled" {
                Duration::from_millis(30)
            } else {
                Duration::from_secs(8)
            };
            drop(command);
            let result = tokio::time::timeout(deadline, launch.bind(process)).await;
            if mode == "cancelled" {
                assert!(result.is_err());
            } else {
                assert!(result.unwrap().is_err());
            }
            tokio::time::timeout(Duration::from_secs(2), async {
                while std::path::Path::new(&format!("/proc/{pid}")).exists() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("owned failed launch is reaped");
            drop(peer);
        }
    }
}
