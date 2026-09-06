//! Linux cgroup-v2 process ownership primitives, not a Session authority.
//!
//! The caller must supply an externally supervised, delegated non-root cgroup
//! and retain one owner per physical realization. This module neither creates
//! units nor discovers/claims arbitrary host processes. It does not yet make
//! ordinary `LocalProcess::spawn` scoped or advertise Worker crash containment.
//! A launcher holds `launch_permit` until its trusted child has joined and
//! acknowledged membership, BEFORE allowing workload code to execute.
//! Recovery must use the Session/realization fence; opening another handle does
//! not share this process-local admission lock or prove ownership.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rustix::fs::{Mode, OFlags, open, openat};
use tokio::sync::{Mutex, MutexGuard};

/// Process-local physical identity while the owner retains its descriptors,
/// never a PID. Durable recovery additionally needs the existing realization
/// fence and host-boot identity; these numbers alone are not recovery authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CgroupIdentity {
    pub device: u64,
    pub inode: u64,
}

/// Descriptor-pinned cgroup control. Renaming/replacing its path cannot redirect
/// a stop to a different generation. Drop has no destructive effects.
pub struct LinuxProcessScope {
    path: PathBuf,
    identity: CgroupIdentity,
    membership: File,
    kill: File,
    events: File,
    closed: Mutex<bool>,
}

/// The launch/close critical section. Dropping it cancels admission only; the
/// caller still owns any child it already created and must resolve that child.
pub struct ScopeLaunchPermit<'a> {
    _guard: MutexGuard<'a, bool>,
    identity: CgroupIdentity,
}

impl ScopeLaunchPermit<'_> {
    pub fn identity(&self) -> CgroupIdentity {
        self.identity
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

impl LinuxProcessScope {
    /// Open a caller-authorized cgroup. Reject ordinary filesystems, symlink
    /// leaves, root cgroups and unavailable control interfaces. This verifies
    /// the kernel substrate, NOT systemd liveness or the caller's Session lease.
    pub fn open(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(invalid("process scope requires an absolute path"));
        }
        let directory = open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        // Linux UAPI linux/magic.h; rustix does not export this filesystem tag.
        if rustix::fs::fstatfs(&directory)?.f_type != 0x6367_7270 {
            return Err(invalid("process scope requires cgroup v2"));
        }
        let stat = rustix::fs::fstat(&directory)?;
        let file = |name: &str, mode| -> io::Result<File> {
            Ok(openat(
                &directory,
                name,
                mode | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?
            .into())
        };
        let scope = Self {
            path: path.to_owned(),
            identity: CgroupIdentity {
                device: stat.st_dev,
                inode: stat.st_ino,
            },
            membership: file("cgroup.procs", OFlags::WRONLY)?,
            kill: file("cgroup.kill", OFlags::WRONLY)?,
            events: file("cgroup.events", OFlags::RDONLY)?,
            closed: Mutex::new(false),
        };
        scope.populated()?;
        Ok(scope)
    }

    pub fn identity(&self) -> CgroupIdentity {
        self.identity
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub async fn launch_permit(&self) -> io::Result<ScopeLaunchPermit<'_>> {
        let guard = self.closed.lock().await;
        if *guard {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "process scope is closed",
            ));
        }
        Ok(ScopeLaunchPermit {
            _guard: guard,
            identity: self.identity,
        })
    }

    /// Trusted child bootstrap only: move THIS process before workload exec.
    /// Never call this from the multi-Session Worker. Membership acknowledgement
    /// belongs to the launcher; successful parent spawn is not acknowledgement.
    /// `expected` must be supplied by the exact owner's launch permit.
    pub fn enter_current_process(&self, expected: CgroupIdentity) -> io::Result<()> {
        if self.identity != expected {
            return Err(invalid("process scope physical identity changed"));
        }
        write_control(&self.membership, b"0")
    }

    /// Close admission, request whole-subtree KILL, then observe no live members.
    /// A failed request, missing event, timeout or cancelled future is NOT a
    /// quiescence receipt. After admission closes, all retries remain closed.
    /// This says nothing about already-issued remote effects or actors with
    /// authority to migrate processes into/out of the delegated hierarchy.
    pub async fn stop(&self, timeout: Duration) -> io::Result<CgroupIdentity> {
        let deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "process scope timeout exceeds the clock range",
                )
            })?;
        let mut closed = tokio::time::timeout_at(deadline, self.closed.lock())
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "process scope launch did not settle",
                )
            })?;
        *closed = true;
        write_control(&self.kill, b"1")?;
        drop(closed);
        loop {
            if !self.populated()? {
                return Ok(self.identity);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "process scope remains populated",
                ));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn populated(&self) -> io::Result<bool> {
        let mut bytes = [0_u8; 4096];
        let count = self.events.read_at(&mut bytes, 0)?;
        if count == bytes.len() {
            return Err(invalid(
                "process scope events exceeded the observation bound",
            ));
        }
        parse_populated(
            std::str::from_utf8(&bytes[..count])
                .map_err(|_| invalid("invalid process scope events"))?,
        )
    }
}

fn write_control(file: &File, value: &[u8]) -> io::Result<()> {
    // A control request is a single write, never a partial-write retry that
    // could be interpreted as another kernel operation.
    if file.write_at(value, 0)? != value.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "incomplete cgroup control request",
        ));
    }
    Ok(())
}

fn parse_populated(text: &str) -> io::Result<bool> {
    let mut value = None;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() == Some("populated") {
            if value.is_some() {
                return Err(invalid("duplicate cgroup populated field"));
            }
            value = Some(match (fields.next(), fields.next()) {
                (Some("0"), None) => false,
                (Some("1"), None) => true,
                _ => return Err(invalid("invalid cgroup populated field")),
            });
        }
    }
    value.ok_or_else(|| invalid("missing cgroup populated field"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failed_or_cancelled_stop_never_reopens_admission_or_invents_empty() {
        // CG13 closed + failed kill => error/still closed; CG14 accepted kill
        // + populated => timeout/still closed; CG15 cancelled after close =>
        // retry may observe empty, but launch stays denied. These private I/O
        // doubles inject failures; only the separate systemd test qualifies
        // the real kernel. Public open rejects these ordinary directories.
        let root = tempfile::tempdir().unwrap();
        let events_path = root.path().join("events");
        let kill_path = root.path().join("kill");
        std::fs::write(&events_path, "populated 1\n").unwrap();
        std::fs::write(&kill_path, "").unwrap();
        let mut scope = LinuxProcessScope {
            path: root.path().to_owned(),
            identity: CgroupIdentity {
                device: 1,
                inode: 2,
            },
            membership: File::open(&kill_path).unwrap(),
            kill: File::open(&kill_path).unwrap(),
            events: File::open(&events_path).unwrap(),
            closed: Mutex::new(false),
        };
        // CG17 unrepresentable deadline => reject before changing admission.
        assert_eq!(
            scope.stop(Duration::MAX).await.unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        drop(scope.launch_permit().await.unwrap());
        assert!(scope.stop(Duration::from_secs(1)).await.is_err(), "CG13");
        assert!(scope.launch_permit().await.is_err());
        scope.kill = std::fs::OpenOptions::new()
            .write(true)
            .open(&kill_path)
            .unwrap();
        assert_eq!(
            scope.stop(Duration::ZERO).await.unwrap_err().kind(),
            io::ErrorKind::TimedOut,
            "CG14"
        );
        {
            let stop = scope.stop(Duration::from_secs(1));
            tokio::pin!(stop);
            tokio::select! {
                biased;
                result = &mut stop => panic!("CG15 cannot observe empty: {result:?}"),
                () = tokio::task::yield_now() => {}
            }
        }
        assert!(scope.launch_permit().await.is_err());
        std::fs::write(&events_path, "populated 0\n").unwrap();
        assert_eq!(
            scope.stop(Duration::from_secs(1)).await.unwrap(),
            scope.identity()
        );
        assert!(scope.launch_permit().await.is_err());
        std::fs::write(&events_path, "frozen 0\n").unwrap();
        assert_eq!(
            scope.stop(Duration::from_secs(1)).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData,
            "CG16 missing observation is not empty"
        );
        // CG19 malformed bytes or an observation reaching the read bound =>
        // reject, even when its prefix appears to report an empty subtree.
        for bytes in [b"populated 0\n\xff".to_vec(), {
            let mut bytes = b"populated 0\n".to_vec();
            bytes.resize(4096, b' ');
            bytes
        }] {
            std::fs::write(&events_path, bytes).unwrap();
            assert_eq!(
                scope.stop(Duration::from_secs(1)).await.unwrap_err().kind(),
                io::ErrorKind::InvalidData,
                "CG19"
            );
            assert!(scope.launch_permit().await.is_err());
        }
    }

    #[test]
    fn substrate_and_observation_admission_fail_closed() {
        // CG1 real kernel 0/1 => empty/live; CG2 missing, duplicate, malformed
        // or truncated value => error, never empty. Unknown kernel keys are
        // forward-compatible but cannot substitute for the required field.
        for (value, expected) in [
            ("populated 0\nfrozen 0\n", false),
            ("future 7\npopulated 1\n", true),
        ] {
            assert_eq!(parse_populated(value).unwrap(), expected, "CG1");
        }
        for value in [
            "",
            "frozen 0",
            "populated",
            "populated 2",
            "populated 0 extra",
            "populated 0\npopulated 1",
        ] {
            assert!(parse_populated(value).is_err(), "CG2: {value}");
        }
        // CG3 a lookalike directory, its symlink, or a missing path cannot
        // become a process scope. No arbitrary filesystem control-file writes.
        let root = tempfile::tempdir().unwrap();
        assert!(LinuxProcessScope::open(root.path()).is_err());
        std::os::unix::fs::symlink(root.path(), root.path().join("alias")).unwrap();
        assert!(LinuxProcessScope::open(&root.path().join("alias")).is_err());
        assert!(LinuxProcessScope::open(&root.path().join("absent")).is_err());
        // CG20 relative paths are not stable handoff addresses; reject before
        // inspecting a filesystem, regardless of the current working directory.
        assert!(
            LinuxProcessScope::open(Path::new("."))
                .err()
                .unwrap()
                .to_string()
                .contains("absolute path")
        );
    }
}
