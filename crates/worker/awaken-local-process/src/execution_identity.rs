// @formal evidence sandbox.scoped_launch_requires_current_grant_and_init_exit_for_quiescence
//! Physical execution evidence for the provider's EXISTING realization marker.
//! No process registry and no authority to signal a saved numeric PID. A cold
//! observer can prove absence or retain uncertainty, never invent termination.

use serde::{Deserialize, Serialize};
use std::io;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessNamespaceIdentity {
    pub device: u64,
    pub inode: u64,
}

/// Boot and observer scope qualify the PID namespace identity. The init PID
/// is only a read-only lookup hint: its absence/exit is useful, but sending a
/// signal through a reconstructed numeric PID is expressly not supported.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessExecutionIdentity {
    boot_id: [u8; 16],
    observer: ProcessNamespaceIdentity,
    init_pid: u32,
    namespace: ProcessNamespaceIdentity,
}

impl ProcessExecutionIdentity {
    pub fn validate(&self) -> io::Result<()> {
        if self.boot_id == [0; 16]
            || self.init_pid <= 1
            || self.init_pid > i32::MAX as u32
            || self.observer.inode == 0
            || self.namespace.inode == 0
            || self.observer == self.namespace
        {
            return Err(invalid());
        }
        Ok(())
    }

    pub fn namespace(&self) -> ProcessNamespaceIdentity {
        self.namespace
    }

    /// `Ok(true)` means exact scope absence. A different boot or observer
    /// namespace is unknown, NOT proof that a shared/foreign root is safe.
    /// Errors, permissions and incomplete /proc evidence remain retryable by
    /// the existing realization owner. There is no timeout-to-absence edge.
    pub fn is_quiescent(&self) -> io::Result<bool> {
        self.validate()?;
        #[cfg(target_os = "linux")]
        {
            linux::observe(self)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Linux execution evidence requires its original host observer",
            ))
        }
    }
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid or foreign execution identity",
    )
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use rustix::process::{Pid, PidfdFlags, pidfd_open};
    use std::{
        fs::File,
        io::Read,
        os::fd::{AsRawFd, OwnedFd},
        os::unix::fs::MetadataExt,
    };

    fn read_bounded(path: impl AsRef<std::path::Path>) -> io::Result<String> {
        let mut text = String::new();
        File::open(path)?.take(16_385).read_to_string(&mut text)?;
        if text.len() > 16_384 {
            return Err(invalid());
        }
        Ok(text)
    }

    fn namespace(path: impl AsRef<std::path::Path>) -> io::Result<ProcessNamespaceIdentity> {
        let file = File::open(path)?;
        // A masked /proc entry can be a readable directory or ordinary file.
        // Such an inode is NOT evidence that the saved namespace disappeared.
        if rustix::fs::fstatfs(&file)?.f_type != 0x6e736673 {
            return Err(invalid());
        }
        let stat = file.metadata()?;
        Ok(ProcessNamespaceIdentity {
            device: stat.dev(),
            inode: stat.ino(),
        })
    }

    fn boot_id(text: &str) -> io::Result<[u8; 16]> {
        let text = text.trim_end_matches('\n');
        if text.len() != 36 {
            return Err(invalid());
        }
        let mut result = [0; 16];
        let mut digits = Vec::with_capacity(32);
        for (i, byte) in text.bytes().enumerate() {
            if [8, 13, 18, 23].contains(&i) {
                if byte != b'-' {
                    return Err(invalid());
                }
            } else {
                digits.push(match byte {
                    b'0'..=b'9' => byte - b'0',
                    b'a'..=b'f' => byte - b'a' + 10,
                    _ => return Err(invalid()),
                });
            }
        }
        for (byte, pair) in result.iter_mut().zip(digits.chunks_exact(2)) {
            *byte = pair[0] * 16 + pair[1];
        }
        if result == [0; 16] {
            return Err(invalid());
        }
        Ok(result)
    }

    fn context() -> io::Result<([u8; 16], ProcessNamespaceIdentity)> {
        // /proc is a PID-namespace view, not a globally meaningful PID map.
        // Require the same observer used by pidfd_open, including after restart.
        if std::fs::read_link("/proc/self")?
            != std::path::Path::new(&std::process::id().to_string())
        {
            return Err(invalid());
        }
        let observer = namespace("/proc/self/ns/pid")?;
        validate_observer_hierarchy(&read_bounded("/proc/self/status")?, std::process::id())?;
        Ok((
            boot_id(&read_bounded("/proc/sys/kernel/random/boot_id")?)?,
            observer,
        ))
    }

    fn validate_observer_hierarchy(status: &str, pid: u32) -> io::Result<()> {
        // proc_pid_status(5): NStgid starts at the procfs mounting namespace
        // and includes every nested namespace. Exactly one entry equal to
        // getpid proves that /proc and pidfd_open use the same PID numbering.
        // Unlike /proc/1/ns/pid, this does not require access to host init.
        let mut fields = status
            .lines()
            .filter_map(|line| line.strip_prefix("NStgid:"));
        let values = fields
            .next()
            .ok_or_else(invalid)?
            .split_whitespace()
            .collect::<Vec<_>>();
        if fields.next().is_some()
            || values.len() != 1
            || values[0].parse::<u32>().map_err(|_| invalid())? != pid
        {
            return Err(invalid());
        }
        Ok(())
    }

    fn init_pid(text: &str) -> io::Result<u32> {
        let mut pid = None;
        let mut nested = None;
        for line in text.lines() {
            if let Some(value) = line.strip_prefix("Pid:") {
                if pid.is_some() {
                    return Err(invalid());
                }
                pid = Some(value.trim().parse::<u32>().map_err(|_| invalid())?);
            }
            if let Some(value) = line.strip_prefix("NSpid:") {
                if nested.is_some() {
                    return Err(invalid());
                }
                nested = Some(
                    value
                        .split_whitespace()
                        .map(str::parse::<u32>)
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|_| invalid())?,
                );
            }
        }
        let pid = pid
            .filter(|pid| *pid > 1 && *pid <= i32::MAX as u32)
            .ok_or_else(invalid)?;
        let nested = nested.ok_or_else(invalid)?;
        if nested.len() < 2
            || nested.len() > 33
            || nested.first() != Some(&pid)
            || nested.last() != Some(&1)
            || nested.iter().any(|pid| *pid == 0 || *pid > i32::MAX as u32)
        {
            return Err(invalid());
        }
        Ok(pid)
    }

    impl ProcessExecutionIdentity {
        pub(crate) fn capture(scope: ProcessNamespaceIdentity, init: &OwnedFd) -> io::Result<Self> {
            let (boot_id, observer) = context().map_err(|error| {
                io::Error::new(error.kind(), "execution observer context unavailable")
            })?;
            let init_pid = init_pid(&read_bounded(format!(
                "/proc/self/fdinfo/{}",
                init.as_raw_fd()
            ))?)
            .map_err(|error| {
                io::Error::new(error.kind(), "execution init descriptor evidence invalid")
            })?;
            if namespace(format!("/proc/{init_pid}/ns/pid"))? != scope {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "execution init and namespace descriptor mismatch",
                ));
            }
            let identity = Self {
                boot_id,
                observer,
                init_pid,
                namespace: scope,
            };
            identity.validate()?;
            if pidfd_exited(init)? || context()? != (boot_id, observer) {
                return Err(invalid());
            }
            Ok(identity)
        }
    }

    pub(super) fn observe(identity: &ProcessExecutionIdentity) -> io::Result<bool> {
        let expected = (identity.boot_id, identity.observer);
        if context()? != expected {
            return Err(invalid());
        }
        let absent = match pidfd_open(
            Pid::from_raw(identity.init_pid as i32).ok_or_else(invalid)?,
            PidfdFlags::empty(),
        ) {
            Err(rustix::io::Errno::SRCH) => true,
            Err(error) => return Err(error.into()),
            Ok(init) => {
                if pidfd_exited(&init)? {
                    true
                } else {
                    // A live process cannot change its PID namespace. If this
                    // numeric PID names another namespace, the saved init was
                    // already released. Reuse can cause conservative waiting,
                    // never a signal to the new occupant. Missing ns links can
                    // occur during exit BEFORE reaping: retain unknown then.
                    match namespace(format!("/proc/{}/ns/pid", identity.init_pid)) {
                        Ok(actual) => actual != identity.namespace,
                        Err(_) if pidfd_exited(&init)? => true,
                        Err(error) => return Err(error),
                    }
                }
            }
        };
        if context()? != expected {
            return Err(invalid());
        }
        Ok(absent)
    }

    pub(crate) fn pidfd_exited(init: &OwnedFd) -> io::Result<bool> {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        let mut fds = [PollFd::new(init, PollFlags::IN)];
        let observation = poll(
            &mut fds,
            Some(&Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            }),
        )
        .map(|_| fds[0].revents());
        classify_pidfd_observation(observation)
    }

    fn classify_pidfd_observation(
        observation: rustix::io::Result<rustix::event::PollFlags>,
    ) -> io::Result<bool> {
        use rustix::event::PollFlags;
        let observed = match observation {
            // A zero-timeout poll may still be interrupted by SIGCHLD.
            // No observation is no receipt: retain pending and let the
            // existing async waiter retry, without spinning under signals.
            Err(rustix::io::Errno::INTR) => return Ok(false),
            Err(error) => return Err(error.into()),
            Ok(observed) => observed,
        };
        if observed.intersects(PollFlags::NVAL | PollFlags::ERR) {
            return Err(invalid());
        }
        Ok(observed.intersects(PollFlags::IN | PollFlags::HUP))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn interrupted_pidfd_poll_is_pending_not_exit_or_permanent_failure() {
            // P1 EINTR/empty -> no quiescence receipt, existing waiter retries;
            // P2 IN/HUP -> exact exit; P3 invalid/error flags dominate even IN;
            // P4 other syscall failure remains unknown/error. Pure seam keeps
            // signals deterministic and does not install process-global handlers.
            use rustix::{event::PollFlags as F, io::Errno};
            for observation in [Err(Errno::INTR), Ok(F::empty())] {
                assert!(!classify_pidfd_observation(observation).unwrap());
            }
            for flags in [F::IN, F::HUP, F::IN | F::HUP] {
                assert!(classify_pidfd_observation(Ok(flags)).unwrap());
            }
            for flags in [F::NVAL, F::ERR, F::NVAL | F::IN, F::ERR | F::HUP] {
                assert!(classify_pidfd_observation(Ok(flags)).is_err());
            }
            assert!(classify_pidfd_observation(Err(Errno::BADF)).is_err());
        }

        #[test]
        fn recovery_identity_decoding_requires_exact_boot_and_descendant_init() {
            // I1 exact boot UUID + matching Pid/NSpid ending in 1 => identity;
            // I2 malformed/zero boot, missing/duplicate fields, invisible/dead
            // PID, non-init or mismatched namespace hierarchy => reject before
            // any /proc lookup or signal. Extra kernel fdinfo fields are allowed.
            assert!(boot_id("12345678-1234-1234-1234-123456789abc\n").is_ok());
            for invalid in [
                "",
                "00000000-0000-0000-0000-000000000000",
                "12345678_1234-1234-1234-123456789abc",
                "12345678-1234-1234-1234-123456789abG",
            ] {
                assert!(boot_id(invalid).is_err());
            }
            assert_eq!(init_pid("pos: 0\nPid:\t42\nNSpid:\t42\t1\n").unwrap(), 42);
            for bad in [
                "",
                "Pid: -1\nNSpid: -1",
                "Pid: 0\nNSpid: 0",
                "Pid: 42\nNSpid: 42",
                "Pid: 42\nNSpid: 42 2",
                "Pid: 42\nNSpid: 43 1",
                "Pid: 42\nPid: 42\nNSpid: 42 1",
                "Pid: 42\nNSpid: 42 1\nNSpid: 42 1",
            ] {
                assert!(init_pid(bad).is_err(), "{bad}");
            }
        }

        #[test]
        fn observer_evidence_rejects_masked_proc_and_foreign_pid_numbering() {
            // I3 one exact NStgid => same observer; multiple entries (even an
            // accidental equal numeric PID), foreign, missing, duplicate or
            // malformed field => unknown. I4 ordinary file/directory masks,
            // missing paths, oversized/invalid UTF-8 proc input => unknown,
            // never a different namespace/absence receipt. I5 actual self
            // nsfs descriptor and bounded context must be independently usable.
            assert!(validate_observer_hierarchy("NStgid:\t42\n", 42).is_ok());
            for bad in [
                "",
                "NStgid: 43",
                "NStgid: 42 42",
                "NStgid: 42 1",
                "NStgid: 42\nNStgid: 42",
                "NStgid: 42x",
            ] {
                assert!(validate_observer_hierarchy(bad, 42).is_err(), "{bad}");
            }
            let temp = tempfile::tempdir().unwrap();
            let file = temp.path().join("mask");
            std::fs::write(&file, b"fake namespace").unwrap();
            for path in [
                temp.path().to_path_buf(),
                file.clone(),
                temp.path().join("absent"),
            ] {
                assert!(namespace(path).is_err());
            }
            std::fs::write(&file, vec![b'a'; 16_385]).unwrap();
            assert!(read_bounded(&file).is_err());
            std::fs::write(&file, [0xff]).unwrap();
            assert!(read_bounded(&file).is_err());
            assert_ne!(namespace("/proc/self/ns/pid").unwrap().inode, 0);
            assert!(context().is_ok());
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) use linux::pidfd_exited;
