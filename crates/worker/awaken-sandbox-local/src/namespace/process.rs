//! Bind bubblewrap's trusted child report to the existing local lifecycle port.
//! No PID discovery through task-visible files, argv, or host process scans.
use super::{LocalProcess, TokioCommand, pc};

pub(super) struct LaunchInfo {
    #[cfg(target_os = "linux")]
    report: tokio::net::UnixStream,
}

impl LaunchInfo {
    /// Map bubblewrap's private report (fd 3) and the workload syscall profile
    /// (fds 4..) in one mapping, so no parent descriptor can be clobbered by an
    /// earlier child-side `dup2`.
    pub(super) fn attach(
        command: &mut TokioCommand,
        runtime_roots: Vec<awaken_fs_durability::ProjectionRoot>,
    ) -> Result<Self, pc::SandboxError> {
        #[cfg(target_os = "linux")]
        {
            // LocalProcess::spawn owns process-group and cancellation policy;
            // this adapter only supplies the trusted namespace identity report.
            let (read, write) = std::os::unix::net::UnixStream::pair()
                .map_err(|error| pc::SandboxError::backend("attach", error))?;
            read.set_nonblocking(true)
                .map_err(|error| pc::SandboxError::backend("attach", error))?;
            map_launch_fds(
                command,
                Some(write.into()),
                awaken_local_process::seccomp::SyscallProfile::Namespace,
                runtime_roots,
            )?;
            Ok(Self {
                report: tokio::net::UnixStream::from_std(read)
                    .map_err(|error| pc::SandboxError::backend("attach", error))?,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (command, runtime_roots);
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

/// The syscall profile alone, for a launch that needs no identity report (the
/// readiness probe, the Workdir networkless shell). Readiness must exercise the
/// same filter a Session gets.
#[cfg(target_os = "linux")]
pub(crate) fn attach_syscall_profile(
    command: &mut TokioCommand,
    profile: awaken_local_process::seccomp::SyscallProfile,
    runtime_roots: Vec<awaken_fs_durability::ProjectionRoot>,
) -> Result<(), pc::SandboxError> {
    map_launch_fds(command, None, profile, runtime_roots)
}

/// Map the report (fd 3) and the compiled programs onto the descriptors the
/// rendered `--add-seccomp-fd` options name, in one `command-fds` mapping.
#[cfg(target_os = "linux")]
fn map_launch_fds(
    command: &mut TokioCommand,
    report: Option<std::os::fd::OwnedFd>,
    profile: awaken_local_process::seccomp::SyscallProfile,
    runtime_roots: Vec<awaken_fs_durability::ProjectionRoot>,
) -> Result<(), pc::SandboxError> {
    use awaken_local_process::seccomp::BUBBLEWRAP_FILTER_FDS;
    use command_fds::{CommandFdExt, FdMapping};
    use std::os::fd::AsFd as _;
    let filters = profile
        .bubblewrap_filter_fds()
        .map_err(|error| pc::SandboxError::unsupported(error.to_string()))?;
    if filters.len() != BUBBLEWRAP_FILTER_FDS.len() {
        return Err(pc::SandboxError::backend(
            "attach",
            "syscall profile program count does not match the rendered launcher options",
        ));
    }
    let mut mappings = Vec::with_capacity(filters.len() + 1);
    let with_report = report.is_some();
    if let Some(report) = report {
        mappings.push(FdMapping {
            parent_fd: report,
            child_fd: 3,
        });
    }
    for (child_fd, filter) in BUBBLEWRAP_FILTER_FDS.into_iter().zip(filters) {
        mappings.push(FdMapping {
            parent_fd: filter,
            child_fd,
        });
    }
    for (index, root) in runtime_roots.into_iter().enumerate() {
        mappings.push(FdMapping {
            parent_fd: root
                .as_fd()
                .try_clone_to_owned()
                .map_err(|error| pc::SandboxError::backend("attach", error))?,
            child_fd: runtime_root_fd(index)?,
        });
    }
    command
        .as_std_mut()
        .fd_mappings(mappings)
        .map_err(|error| pc::SandboxError::backend("attach", error))?;
    if with_report {
        command.args(["--info-fd", "3"]);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn runtime_root_fd(index: usize) -> Result<i32, pc::SandboxError> {
    let first = awaken_local_process::BUBBLEWRAP_SECCOMP_FDS
        .into_iter()
        .max()
        .expect("a syscall profile has descriptors")
        + 1;
    i32::try_from(index)
        .ok()
        .and_then(|index| first.checked_add(index))
        .ok_or_else(|| pc::SandboxError::invalid("too many runtime projection descriptors"))
}

/// Replace path-based automatic runtime binds with the retained directory's
/// descriptor. Missing automatic roots are omitted, not left as a path that
/// could appear after admission. Both the resolved path and the actual opened
/// directory must satisfy the same root predicate before any launcher starts.
#[cfg(target_os = "linux")]
pub(super) fn pin_runtime_roots(
    argv: &mut Vec<String>,
    roots: &[String],
) -> Result<Vec<awaken_fs_durability::ProjectionRoot>, pc::SandboxError> {
    let mut pinned = Vec::new();
    for destination in roots {
        let position = argv
            .windows(3)
            .position(|args| args == ["--ro-bind-try", destination, destination])
            .ok_or_else(|| {
                pc::SandboxError::invalid("runtime projection is absent from launch plan")
            })?;
        let Some((_, root)) = super::seatbelt::prepare_runtime_root(destination)? else {
            argv.drain(position..position + 3);
            continue;
        };
        argv.splice(
            position..position + 3,
            [
                "--ro-bind-fd".to_owned(),
                runtime_root_fd(pinned.len())?.to_string(),
                destination.clone(),
            ],
        );
        pinned.push(root);
    }
    Ok(pinned)
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

    /// P1 a plain runtime directory or a benign alias -> one fd bind retaining
    /// the admitted inode. P2 substitute that alias after preparation -> the
    /// pinned view still reads P1. P3 an absent automatic root -> remove its
    /// path bind entirely, so later creation cannot add a new host view.
    #[test]
    fn runtime_projection_retains_identity_and_omits_missing_roots() {
        use std::os::fd::{AsFd as _, AsRawFd as _};
        let temp = tempfile::tempdir().unwrap();
        let owned = temp.path().join("owned");
        let foreign = temp.path().join("foreign");
        let alias = temp.path().join("runtime");
        std::fs::create_dir(&owned).unwrap();
        std::fs::create_dir(&foreign).unwrap();
        std::fs::write(owned.join("marker"), b"owned").unwrap();
        std::fs::write(foreign.join("marker"), b"foreign").unwrap();
        std::os::unix::fs::symlink(&owned, &alias).unwrap();
        let root = alias.to_string_lossy().into_owned();
        let missing = temp.path().join("missing").to_string_lossy().into_owned();
        let mut argv = vec![
            "bwrap".into(),
            "--ro-bind-try".into(),
            root.clone(),
            root.clone(),
            "--ro-bind-try".into(),
            missing.clone(),
            missing.clone(),
        ];
        let retained = pin_runtime_roots(&mut argv, &[root.clone(), missing.clone()]).unwrap();
        assert_eq!(retained.len(), 1, "P1");
        assert_eq!(argv, ["bwrap", "--ro-bind-fd", "6", &root], "P1/P3");
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&foreign, &alias).unwrap();
        std::fs::create_dir(&missing).unwrap();
        let pinned =
            std::path::PathBuf::from(format!("/proc/self/fd/{}", retained[0].as_fd().as_raw_fd()));
        assert_eq!(
            std::fs::read(pinned.join("marker")).unwrap(),
            b"owned",
            "P2"
        );
        assert_eq!(
            std::fs::read(alias.join("marker")).unwrap(),
            b"foreign",
            "P2 control"
        );
    }

    /// P4 lexical installation-looking alias -> physical operator HOME: refuse
    /// before adding inherited descriptors or starting a launcher. The test
    /// reads only directory metadata, never any HOME file or secret.
    #[test]
    fn runtime_alias_cannot_project_the_operator_home() {
        let temp = tempfile::tempdir().unwrap();
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
        let alias = temp.path().join("installation");
        std::os::unix::fs::symlink(&home, &alias).unwrap();
        let root = alias.to_string_lossy().into_owned();
        let mut argv = vec![
            "bwrap".into(),
            "--ro-bind-try".into(),
            root.clone(),
            root.clone(),
        ];
        assert!(pin_runtime_roots(&mut argv, &[root]).is_err(), "P4");
        assert!(
            !argv.iter().any(|arg| arg == "--ro-bind-fd"),
            "P4 no admitted bind"
        );
    }

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
                LaunchInfo::attach(&mut command, Vec::new()).unwrap()
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
