//! Exec attempt identity: where a started process records itself, and who is
//! on record when a caller is cancelled before it ever holds the process.
//!
//! # Why this is one module
//!
//! Starting an agent is two separate obligations that used to live nowhere.
//! The *physical* one is the pid file the container writes: without an
//! enumerable name, a process whose `ProcessHandle` never reached a caller can
//! never be signalled again, because the only reference to it was the handle.
//! The *in-process* one is the record saying a spawn was started and did not
//! come back, so the sandbox's own disposal knows it must reap rather than
//! assume every exec it started was also awaited.
//!
//! Both halves are here because they are the same fact seen from two sides,
//! and keeping them together is what stops a wrapper's path and the reaper's
//! glob from drifting apart.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_provisioning_contract as pc;

/// The one in-container directory every backend's exec wrapper writes its pid
/// file into.
///
/// A directory rather than the old `/tmp/awaken-exec-{n}.pid` name pattern
/// because enumeration is the whole point: an exec whose `ProcessHandle` never
/// reached a caller is findable only if something can *list* the attempts, and
/// the only thing that can read this path is the container's own shell. The
/// previous docker name was keyed by a process-local counter, so nothing — not
/// a later call, not a restarted Worker, not the container itself — could name
/// it.
pub(crate) const EXEC_PID_DIR: &str = "/tmp/awaken-exec";

/// The environment variable each exec wrapper exports so a started process
/// knows its own pid file. Only the orphan reaper reads it, and only so that it
/// does not kill and then delete itself.
pub(crate) const EXEC_PID_FILE_ENV: &str = "AWAKEN_EXEC_PID_FILE";

/// The pid file for one exec attempt, named by the attempt id its backend
/// minted. Attempt ids carry the starting process identity, so two Worker
/// incarnations exec'ing into the same adopted container cannot collide on a
/// name and cannot delete each other's record.
#[cfg(any(test, feature = "docker", feature = "podman", feature = "k8s"))]
pub(crate) fn exec_pid_file(attempt_id: &str) -> String {
    format!("{EXEC_PID_DIR}/{attempt_id}.pid")
}

/// The shell prelude every backend's exec wrapper runs before writing its pid:
/// create the enumerable directory (idempotent under a second exec in the same
/// container) and publish the pid file's own path. Shared so the three wrappers
/// cannot drift from the one path the reaper enumerates.
#[cfg(any(test, feature = "docker", feature = "podman", feature = "k8s"))]
pub(crate) fn exec_pid_file_prelude() -> String {
    format!("mkdir -p {EXEC_PID_DIR}; export {EXEC_PID_FILE_ENV}=\"$pid_file\";")
}

/// The command that makes the container reap its own exec orphans.
///
/// It runs *in* the container because the pid files are in the container's
/// filesystem: no Worker-side value survives the task that started the exec,
/// and no backend API lists "processes I exec'd earlier". Every pid file it
/// finds belongs to an attempt whose owner is gone or going, so it signals and
/// then forgets each one. Its own record is excluded by name and removed last,
/// so a later reap can never read a stale pid and signal whatever now holds it.
///
/// `kill -TERM` is a request. The backend's forced removal at disposal is what
/// makes stopping unconditional; reaping first is what makes it happen *before*
/// Secret write-back and Memory harvest read the workspace.
pub(crate) fn exec_orphan_reaper_command() -> pc::MaterializedCommand {
    let script = format!(
        "for pid_file in {EXEC_PID_DIR}/*.pid; do \
           [ -f \"$pid_file\" ] || continue; \
           if [ \"$pid_file\" = \"${{{EXEC_PID_FILE_ENV}:-}}\" ]; then continue; fi; \
           pid=$(cat -- \"$pid_file\" 2>/dev/null) || pid=''; \
           if [ -n \"$pid\" ]; then kill -TERM \"$pid\" 2>/dev/null; fi; \
           rm -f -- \"$pid_file\"; \
         done; \
         if [ -n \"${{{EXEC_PID_FILE_ENV}:-}}\" ]; then rm -f -- \"${EXEC_PID_FILE_ENV}\"; fi; \
         exit 0"
    );
    let mut command = pc::MaterializedCommand::new(["sh", "-c", script.as_str()]);
    // Nothing reads the reaper's output, and inheriting the Worker's streams
    // would push container noise into the Worker log at disposal time.
    command.stdio = pc::Stdio::Null;
    command
}

/// Every exec attempt one Session environment started, and the subset whose
/// caller went away before it held the process.
///
/// `started` counts attempts, not survivors: it is incremented before the spawn
/// can be polled, so a sandbox that was cancelled mid-spawn still knows an exec
/// may exist. That is the flag disposal reads, because "reap only what we can
/// see" is exactly the assumption the leak violated.
#[derive(Default)]
pub(crate) struct ExecAttempts {
    started: AtomicU64,
    orphaned: Mutex<BTreeSet<String>>,
}

impl ExecAttempts {
    /// Mint the next sandbox-local attempt name and count the attempt.
    ///
    /// Sandbox-local, not backend-local: the backend mints its own id inside
    /// `spawn_agent` and never returns it, so this name is what the in-process
    /// record can speak about. The physical orphan is named by the backend's
    /// pid file, which the reaper enumerates rather than looks up — the two
    /// names describe the same attempt from the two sides that can see it.
    pub(crate) fn begin(&self) -> String {
        format!("exec-{}", self.started.fetch_add(1, Ordering::SeqCst))
    }

    /// Whether any exec attempt was ever started here, settled or not. False
    /// means no process can exist, so reaping has nothing to find and must not
    /// be demanded of a runtime that cannot exec.
    pub(crate) fn any_started(&self) -> bool {
        self.started.load(Ordering::SeqCst) > 0
    }

    /// Put `attempt` on record as started-but-never-returned. Called only from
    /// an [`pc::EffectClaim::in_process`] retention, so it runs exactly when a
    /// spawn was dropped unsettled.
    pub(crate) fn retain_orphan(&self, attempt: String) {
        self.orphaned
            .lock()
            .expect("exec attempt record mutex")
            .insert(attempt);
    }

    /// The attempts currently on record as orphaned.
    #[cfg(test)]
    pub(crate) fn orphaned(&self) -> Vec<String> {
        self.orphaned
            .lock()
            .expect("exec attempt record mutex")
            .iter()
            .cloned()
            .collect()
    }

    /// Forget the record after the container reaped. Reaping is by enumeration
    /// of the container's own pid files, so it covers every orphan at once;
    /// clearing the whole set is the honest projection of that.
    pub(crate) fn clear_orphans(&self) {
        self.orphaned
            .lock()
            .expect("exec attempt record mutex")
            .clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cause/effect design:
    /// C1=an attempt is begun; C2=the spawn is dropped unsettled; C3=the
    /// container reaped. E1=`any_started` reports an exec may exist; E2=the
    /// attempt is on record; E3=the record is empty again.
    /// | Rule | C1 | C2 | C3 | Effect |
    /// | A1   | 0  | -  | -  | !E1    |
    /// | A2   | 1  | 0  | 0  | E1     |
    /// | A3   | 1  | 1  | 0  | E1,E2  |
    /// | A4   | 1  | 1  | 1  | E1,E3  |
    #[test]
    fn a_started_attempt_is_visible_to_disposal_even_when_it_never_returned() {
        let attempts = ExecAttempts::default();
        assert!(!attempts.any_started(), "A1");
        let first = attempts.begin();
        assert!(attempts.any_started(), "A2");
        assert!(attempts.orphaned().is_empty(), "A2");
        attempts.retain_orphan(first.clone());
        assert_eq!(attempts.orphaned(), vec![first], "A3");
        attempts.clear_orphans();
        assert!(
            attempts.any_started(),
            "A4 started is not undone by reaping"
        );
        assert!(attempts.orphaned().is_empty(), "A4");
    }

    /// The reaper must not be able to delete the record of the process it is
    /// itself running as, and must enumerate the same directory the wrappers
    /// write into. Both are string-level invariants, so they are checked where
    /// the strings are built rather than only against a live daemon.
    #[test]
    fn the_reaper_enumerates_the_directory_the_wrappers_write_and_excludes_itself() {
        let command = exec_orphan_reaper_command();
        let script = command.argv.last().expect("reaper carries a script");
        assert!(script.contains(&format!("{EXEC_PID_DIR}/*.pid")));
        assert!(script.contains(&format!("${{{EXEC_PID_FILE_ENV}:-}}")));
        assert_eq!(command.stdio, pc::Stdio::Null);
        assert!(exec_pid_file("k8s-exec-7-1").starts_with(EXEC_PID_DIR));
        let prelude = exec_pid_file_prelude();
        assert!(prelude.contains(&format!("mkdir -p {EXEC_PID_DIR}")));
        assert!(prelude.contains(EXEC_PID_FILE_ENV));
    }
}
