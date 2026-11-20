//! Crash-window proof: creation has succeeded, private Job assignment has not.
use process_wrap::tokio::{ChildWrapper, CommandWrap, CommandWrapper, JobObject};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::{
    io,
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
    },
};

const HELPER: &str = "sys::lifetime_tests::child";
#[derive(Debug)]
struct BeforeAssignment(PathBuf);
impl CommandWrapper for BeforeAssignment {
    fn wrap_child(
        &mut self,
        child: Box<dyn ChildWrapper>,
        _: &CommandWrap,
    ) -> io::Result<Box<dyn ChildWrapper>> {
        // All pre_spawn hooks have run, so the leader is suspended. This is
        // precisely before AssignOriginalJob/JobObject's wrap_child hooks.
        std::fs::write(&self.0, child.id().unwrap().to_string())?;
        std::thread::sleep(Duration::from_secs(60));
        Ok(child)
    }
}

// The parent retains the exact kernel incarnation before killing its owner.
// Drop cleans the negative control too, including when an assertion panics.
struct RetainedChild(OwnedHandle);
impl RetainedChild {
    fn open(pid: u32) -> Self {
        // SAFETY: PID came from this test's private owner-created marker; the
        // suspended process cannot exit before the retained handle is opened.
        let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, 0, pid) };
        assert!(!raw.is_null(), "{}", io::Error::last_os_error());
        Self(unsafe { OwnedHandle::from_raw_handle(raw) })
    }
    fn wait(&self, milliseconds: u32) -> u32 {
        // SAFETY: the owned process handle is retained throughout the wait.
        unsafe { WaitForSingleObject(self.0.as_raw_handle(), milliseconds) }
    }
}
impl Drop for RetainedChild {
    fn drop(&mut self) {
        // SAFETY: this handle was retained from our own exact child; never a
        // later PID lookup. Exit is joined before releasing the last handle.
        unsafe {
            TerminateProcess(self.0.as_raw_handle(), 1);
            WaitForSingleObject(self.0.as_raw_handle(), 10_000);
        }
    }
}

#[test]
#[ignore = "private subprocess participant"]
fn child() {
    let mode = std::env::var("AWAKEN_LIFETIME_MODE").unwrap();
    if mode == "guarded" || mode == "normal-exit" {
        crate::supervise_current_process_children().unwrap();
        // Idempotent installation cannot create another lifetime owner.
        crate::supervise_current_process_children().unwrap();
    }
    if mode == "normal-exit" {
        std::process::exit(23);
    }
    let marker = PathBuf::from(std::env::var_os("AWAKEN_LIFETIME_MARKER").unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--ignored", "--exact", HELPER])
        .env("AWAKEN_LIFETIME_MODE", "leaf")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut wrapped = CommandWrap::from(command);
    wrapped.wrap(BeforeAssignment(marker)).wrap(JobObject);
    let _child = wrapped.spawn().unwrap();
}

fn owner(mode: &str, marker: &std::path::Path) -> std::process::Child {
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", HELPER])
        .env("AWAKEN_LIFETIME_MODE", mode)
        .env("AWAKEN_LIFETIME_MARKER", marker)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

#[test]
fn owner_death_cannot_orphan_a_child_before_private_job_assignment() {
    // Positive and negative controls hit the SAME paused window. Existing
    // post-assignment Job tests cannot expose this earlier native race.
    for guarded in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("child.pid");
        let mut process = owner(if guarded { "guarded" } else { "unguarded" }, &marker);
        let deadline = Instant::now() + Duration::from_secs(15);
        let pid = loop {
            if let Ok(text) = std::fs::read_to_string(&marker) {
                if let Ok(pid) = text.parse() {
                    break pid;
                }
            }
            if Instant::now() > deadline {
                let _ = process.kill();
                let _ = process.wait();
                panic!("owner never reached the pre-assignment window");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let child = RetainedChild::open(pid);
        assert_eq!(child.wait(0), WAIT_TIMEOUT);
        process.kill().unwrap();
        process.wait().unwrap();
        assert_eq!(
            child.wait(if guarded { 10_000 } else { 100 }),
            if guarded { WAIT_OBJECT_0 } else { WAIT_TIMEOUT }
        );
    }
}

#[test]
fn owner_lifetime_preserves_the_requested_exit_code() {
    let directory = tempfile::tempdir().unwrap();
    assert_eq!(
        owner("normal-exit", &directory.path().join("unused"))
            .wait()
            .unwrap()
            .code(),
        Some(23)
    );
}
