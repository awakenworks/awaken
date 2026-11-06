// @formal surface obligation=acp.process_cleanup_requires_observed_exit_and_retained_owner
// @formal reason: Static owner: the one exec-attempt identity for the Container tier - the enumerable
// @formal reason: in-container pid-file path every backend wrapper writes, the shell that lists and
// @formal reason: signals those records, and the in-process set naming attempts whose spawn was dropped
// @formal reason: unsettled. Dynamic path: an attempt is counted before its ClaimedEffect can be polled,
// @formal reason: retention records it only on an unsettled drop, and disposal reaps by enumerating the
// @formal reason: container own PID/starttime records before Secret write-back or Memory harvest reads
// @formal reason: the workspace; an unproven reap skips in-container harvest and reports incomplete
// @formal reason: disposal while removal proceeds. Signal delivery and container-side
// @formal reason: shell behaviour are real process effects covered by the cancellation tests and the
// @formal reason: feature-gated backend suites, not by a direct proof.
//! Exec attempt identity: where a started process records itself, and who is
//! on record when a caller is cancelled before it ever holds the process.
//!
//! # Why this is one module
//!
//! Starting an agent is two separate obligations that used to live nowhere.
//! The *physical* one is the PID and process-start record the container writes: without an
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
const EXEC_PID_START_SUFFIX: &str = ".start";

/// The environment variable each exec wrapper exports so a started process
/// knows its own pid file. Only the orphan reaper reads it, and only so that it
/// does not kill and then delete itself.
pub(crate) const EXEC_PID_FILE_ENV: &str = "AWAKEN_EXEC_PID_FILE";

/// The pid file for one exec attempt, named by the attempt id its backend
/// minted. Attempt ids carry the starting process identity, so two Worker
/// incarnations exec'ing into the same adopted container cannot collide on a
/// name and cannot delete each other's record.
pub fn exec_pid_file(attempt_id: &str) -> String {
    format!("{EXEC_PID_DIR}/{attempt_id}.pid")
}

/// The shell prelude every backend's exec wrapper runs before writing its pid:
/// create the enumerable directory (idempotent under a second exec in the same
/// container), publish the pid file's own path, and record PID plus `/proc`
/// start time before executing the workload. Shared so the three wrappers
/// cannot drift from the identity the reaper checks.
pub fn exec_pid_file_prelude() -> String {
    format!(
        "mkdir -p {EXEC_PID_DIR}; export {EXEC_PID_FILE_ENV}=\"$pid_file\"; \
         record_exec_identity() {{ \
           stat=$(cat \"/proc/$$/stat\") || exit 125; \
           stat=${{stat##*) }}; set -- $stat; \
           [ \"$#\" -ge 20 ] || exit 125; shift 19; \
           printf '%s' \"$1\" > \"$pid_file{EXEC_PID_START_SUFFIX}\" || exit 125; \
           printf '%s' \"$$\" > \"$pid_file\" || exit 125; \
         }};"
    )
}

/// The command that makes the container reap its own exec orphans.
///
/// It runs *in* the container because the pid files are in the container's
/// filesystem: no Worker-side value survives the task that started the exec,
/// and no backend API lists "processes I exec'd earlier". A PID can be reused,
/// so a stale record is removed only after the recorded `/proc` start time
/// differs; an unreadable identity refuses quiescence. Its own record is
/// excluded by name and removed last.
///
/// `kill -TERM` is a request, not completion. The reaper waits for the exact
/// identity to disappear or become a zombie, escalates to KILL within a bounded
/// interval, and reports failure otherwise. Disposal then removes the container
/// without reading mutable Secret or Memory bytes from an unquiet workspace.
pub(crate) fn exec_orphan_reaper_command() -> pc::MaterializedCommand {
    let script = format!(
        r#"read_identity() {{
  [ -e "/proc/$pid/stat" ] || return 1
  stat=$(cat "/proc/$pid/stat" 2>/dev/null) || {{
    [ -e "/proc/$pid/stat" ] && return 2
    return 1
  }}
  stat=${{stat##*) }}
  set -- $stat
  [ "$#" -ge 20 ] || {{
    [ -e "/proc/$pid/stat" ] && return 2
    return 1
  }}
  state=$1
  shift 19
  current_start=$1
}}
quiet_or_stale() {{
  read_identity
  result=$?
  case "$result" in
    0) ;;
    1) return 0 ;;
    *) return 2 ;;
  esac
  [ "$current_start" != "$started" ] || [ "$state" = Z ]
}}
wait_for_quiet() {{
  attempts=0
  while [ "$attempts" -lt 50 ]; do
    if quiet_or_stale; then
      return 0
    else
      result=$?
      [ "$result" -eq 1 ] || return 2
    fi
    sleep 0.1 || return 2
    attempts=$((attempts + 1))
  done
  return 1
}}
for pid_file in {EXEC_PID_DIR}/*.pid; do
  [ -f "$pid_file" ] || continue
  if [ "$pid_file" = "${{{EXEC_PID_FILE_ENV}:-}}" ]; then continue; fi
  pid=$(cat -- "$pid_file" 2>/dev/null) || pid=''
  started=$(cat -- "$pid_file{EXEC_PID_START_SUFFIX}" 2>/dev/null) || started=''
  case "$pid:$started" in *[!0-9:]*|:*|*:) exit 125;; esac
  if quiet_or_stale; then
    rm -f -- "$pid_file" "$pid_file{EXEC_PID_START_SUFFIX}"
    continue
  else
    result=$?
    [ "$result" -eq 1 ] || exit 125
  fi
  [ "$pid" != "$$" ] || exit 125
  kill -TERM "$pid" 2>/dev/null || :
  if wait_for_quiet; then
    :
  else
    result=$?
    [ "$result" -eq 1 ] || exit 125
    kill -KILL "$pid" 2>/dev/null || :
    wait_for_quiet || exit 125
  fi
  rm -f -- "$pid_file" "$pid_file{EXEC_PID_START_SUFFIX}"
done
if [ -n "${{{EXEC_PID_FILE_ENV}:-}}" ]; then
  rm -f -- "${EXEC_PID_FILE_ENV}" "${EXEC_PID_FILE_ENV}{EXEC_PID_START_SUFFIX}"
fi
exit 0"#
    );
    let mut command = pc::MaterializedCommand::new(["sh", "-c", script.as_str()]);
    // Nothing reads the reaper's output, and inheriting the Worker's streams
    // would push container noise into the Worker log at disposal time.
    command.stdio = pc::Stdio::Null;
    command
}

/// The bit that marks an environment closed to new exec attempts. It shares a
/// word with the attempt count so that sealing and counting cannot interleave:
/// a spawn that reads "not sealed" and then increments would otherwise be able
/// to slip a process past a sweep that had already decided what to stop.
const SEALED: u64 = 1 << 63;

/// The admission one exec needs before any runtime will start it.
///
/// A container process is a host resource, and the record that it exists is
/// what disposal reads. Making the runtime port take this instead of a bare
/// container id is what stops an edge from starting a process without leaving
/// that record: the value cannot be built except by taking an attempt from an
/// environment's own ledger, and the ledger refuses once disposal has sealed
/// it. That is the same obligation a registry of method names used to state,
/// except a type cannot forget a name it never heard of.
pub struct AdmittedExec {
    container_id: String,
    attempt: String,
}

impl AdmittedExec {
    /// The container this exec was admitted into. Backends address it the way
    /// they always did; what changed is that they cannot be reached without
    /// one of these.
    #[must_use]
    pub fn container_id(&self) -> &str {
        &self.container_id
    }

    /// The ledger name this attempt was taken under.
    pub(crate) fn attempt(&self) -> &str {
        &self.attempt
    }

    /// Admit an exec that owes no ledger, because no environment owns it.
    ///
    /// Two callers: a test driving the runtime port directly, which manages
    /// its own containers and has no disposal to read a record; and the
    /// reaper, whose own exec is the sweep rather than something the sweep
    /// must find. Gated so that production code outside this crate cannot
    /// reach it, which is the whole point of the type.
    #[cfg(any(test, feature = "unrecorded-exec"))]
    #[must_use]
    pub fn unrecorded(container_id: impl Into<String>) -> Self {
        Self {
            container_id: container_id.into(),
            attempt: String::new(),
        }
    }

    #[cfg(not(any(test, feature = "unrecorded-exec")))]
    pub(crate) fn unrecorded(container_id: impl Into<String>) -> Self {
        Self {
            container_id: container_id.into(),
            attempt: String::new(),
        }
    }
}

/// Evidence that one environment is closed to new Session work.
///
/// Cleanup reads the workspace — Secret write-back, the Memory harvest — and
/// those reads are only sound once nothing new can start writing to it.
/// Sealing is what makes that true, so the reads take this value rather than
/// trusting the order of the lines above them: a caller that has not sealed
/// has nothing to pass, and one cannot be built from anywhere else.
pub(crate) struct DisposalSeal {
    started: bool,
    /// Why the container could not be made quiet, when it could not. Sealing
    /// stops new work; only a completed reap stops what was already running,
    /// and a container that cannot exec cannot be asked.
    unquiet: Option<String>,
}

impl DisposalSeal {
    /// Whether any exec attempt was ever started, and so whether the container
    /// must still be asked to forget them.
    pub(crate) const fn any_started(&self) -> bool {
        self.started
    }

    /// Why reads that run inside the container are not sound, when they are
    /// not. `None` means the container was made quiet and they are.
    pub(crate) fn unquiet(&self) -> Option<&str> {
        self.unquiet.as_deref()
    }

    /// Record that the container could not be quiesced.
    pub(crate) fn with_unquiet_container(self, why: impl Into<String>) -> Self {
        Self {
            unquiet: Some(why.into()),
            ..self
        }
    }
}

/// Every exec attempt one Session environment started, and the subset whose
/// caller went away before it held the process.
///
/// `state` counts attempts, not survivors: it is incremented before the spawn
/// can be polled, so a sandbox that was cancelled mid-spawn still knows an exec
/// may exist. That is the flag disposal reads, because "reap only what we can
/// see" is exactly the assumption the leak violated.
///
/// It also carries whether disposal has closed this environment. Both facts
/// live in one word because they are read together and must not disagree: the
/// reaper's precondition is exactly "something was started, and nothing new
/// can be".
#[derive(Default)]
pub(crate) struct ExecAttempts {
    state: AtomicU64,
    orphaned: awaken_sync::LeafLock<BTreeSet<String>>,
}

impl ExecAttempts {
    /// Mint the next sandbox-local attempt name and count the attempt, or
    /// refuse because disposal has already sealed this environment.
    ///
    /// Sandbox-local, not backend-local: the backend mints its own id inside
    /// `spawn_agent` and never returns it, so this name is what the in-process
    /// record can speak about. The physical orphan is named by the backend's
    /// pid file, which the reaper enumerates rather than looks up — the two
    /// names describe the same attempt from the two sides that can see it.
    ///
    /// `None` is the disposal gate. The compare-exchange is what makes it a
    /// gate rather than a hint: a caller that observes an open environment has
    /// already taken its number, so no attempt can be admitted after the sweep
    /// that was supposed to stop it decided what it was stopping.
    /// Admit one Session exec into `container_id`, or refuse because disposal
    /// has sealed this environment.
    pub(crate) fn admit(&self, container_id: &str) -> Option<AdmittedExec> {
        self.begin().map(|attempt| AdmittedExec {
            container_id: container_id.to_string(),
            attempt,
        })
    }

    /// Admit one exec belonging to the environment's own machinery. See
    /// [`ExecAttempts::begin_for_machinery`] for why this one bypasses the
    /// gate but still takes a number.
    pub(crate) fn admit_machinery(&self, container_id: &str) -> AdmittedExec {
        AdmittedExec {
            container_id: container_id.to_string(),
            attempt: self.begin_for_machinery(),
        }
    }

    fn begin(&self) -> Option<String> {
        let mut observed = self.state.load(Ordering::SeqCst);
        loop {
            if observed & SEALED != 0 {
                return None;
            }
            match self.state.compare_exchange_weak(
                observed,
                observed + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Some(format!("exec-{observed}")),
                Err(current) => observed = current,
            }
        }
    }

    /// Mint an attempt name without consulting the disposal gate.
    ///
    /// Only the environment's own machinery uses this. Disposal seals the
    /// Session edges and *then* reads the workspace — harvest, file reads —
    /// through real execs of its own; those run inside the sweep by
    /// construction and exit within it, so the gate, which exists to stop
    /// Session work from outliving the sweep, must not close cleanup's own
    /// readers out of the container they are cleaning. The attempt is still
    /// counted, because it is still a process the container started.
    pub(crate) fn begin_for_machinery(&self) -> String {
        format!(
            "exec-{}",
            self.state.fetch_add(1, Ordering::SeqCst) & !SEALED
        )
    }

    /// Close this environment to new attempts, and report what that
    /// established.
    ///
    /// One call because disposal needs both answers and must not act on them
    /// separately: closing first is what keeps a concurrent spawn from
    /// starting a process the reaper has already finished enumerating, and the
    /// count is what keeps reaping from being demanded of a runtime that never
    /// exec'd, whose inherited failure would otherwise be reachable. Idempotent,
    /// so a retried disposal seals once and still reports the same count.
    pub(crate) fn seal(&self) -> DisposalSeal {
        DisposalSeal {
            started: self.state.fetch_or(SEALED, Ordering::SeqCst) & !SEALED > 0,
            unquiet: None,
        }
    }

    /// Whether this environment is closed to new attempts.
    #[cfg(test)]
    pub(crate) fn sealed(&self) -> bool {
        self.state.load(Ordering::SeqCst) & SEALED != 0
    }

    /// Put `attempt` on record as started-but-never-returned. Called only from
    /// an [`pc::EffectClaim::in_process`] retention, so it runs exactly when a
    /// spawn was dropped unsettled.
    pub(crate) fn retain_orphan(&self, attempt: String) {
        self.orphaned.lock().insert(attempt);
    }

    /// The attempts currently on record as orphaned.
    #[cfg(test)]
    pub(crate) fn orphaned(&self) -> Vec<String> {
        self.orphaned.lock().iter().cloned().collect()
    }

    /// Forget the record after the container reaped. Reaping is by enumeration
    /// of the container's own pid files, so it covers every orphan at once;
    /// clearing the whole set is the honest projection of that.
    pub(crate) fn clear_orphans(&self) {
        self.orphaned.lock().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cause/effect design:
    /// C1=an attempt is begun; C2=the spawn is dropped unsettled; C3=the
    /// container reaped. E1=sealing reports an exec may exist; E2=the attempt
    /// is on record; E3=the record is empty again.
    /// | Rule | C1 | C2 | C3 | Effect |
    /// | A1   | 0  | -  | -  | !E1    |
    /// | A2   | 1  | 0  | 0  | E1     |
    /// | A3   | 1  | 1  | 0  | E1,E2  |
    /// | A4   | 1  | 1  | 1  | E1,E3  |
    #[test]
    fn a_started_attempt_is_visible_to_disposal_even_when_it_never_returned() {
        assert!(!ExecAttempts::default().seal().any_started(), "A1");

        let attempts = ExecAttempts::default();
        let first = attempts.begin().expect("A2 an open environment admits");
        assert!(attempts.orphaned().is_empty(), "A2");
        attempts.retain_orphan(first.clone());
        assert_eq!(attempts.orphaned(), vec![first], "A3");
        attempts.clear_orphans();
        assert!(attempts.orphaned().is_empty(), "A4");
        assert!(
            attempts.seal().any_started(),
            "A4 started is not undone by reaping"
        );
    }

    /// The disposal gate. C1=the environment was sealed; C2=an attempt was
    /// begun before sealing; C3=sealing is repeated. E1=`begin` refuses;
    /// E2=`sealed` reports closed; E3=the reported count is unchanged.
    /// | Rule | C1 | C2 | C3 | Effect    |
    /// | B1   | 0  | -  | -  | !E1,!E2   |
    /// | B2   | 1  | 0  | 0  | E1,E2     |
    /// | B3   | 1  | 1  | 0  | E1,E2     |
    /// | B4   | 1  | 1  | 1  | E1,E2,E3  |
    #[test]
    fn a_sealed_environment_admits_no_further_attempt_and_still_reports_its_count() {
        let open = ExecAttempts::default();
        assert!(!open.sealed(), "B1");
        assert!(open.begin().is_some(), "B1");

        let empty = ExecAttempts::default();
        assert!(!empty.seal().any_started(), "B2 nothing was started");
        assert!(empty.sealed(), "B2/E2");
        assert!(empty.begin().is_none(), "B2/E1");

        let used = ExecAttempts::default();
        let attempt = used.begin().expect("B3 admitted before sealing");
        assert!(used.seal().any_started(), "B3 one attempt was started");
        assert!(used.sealed(), "B3/E2");
        assert!(used.begin().is_none(), "B3/E1");
        // The gate cannot resurrect a name either: the refused caller has no
        // attempt to retain, so the record still holds only what was admitted.
        used.retain_orphan(attempt.clone());
        assert_eq!(used.orphaned(), vec![attempt], "B3/E2");

        assert!(
            used.seal().any_started(),
            "B4/E3 sealing again reports the same count"
        );
        assert!(used.begin().is_none(), "B4/E1");
    }

    /// The reaper must not be able to delete the record of the process it is
    /// itself running as, and must enumerate the same directory the wrappers
    /// write into. Both are string-level invariants, so they are checked where
    /// the strings are built rather than only against a live daemon.
    #[test]
    fn the_reaper_enumerates_the_directory_the_wrappers_write_and_excludes_itself() {
        // C1 each wrapper records PID and process starttime in one directory;
        // C2 reaper checks that identity before signalling and excludes its
        // own record. E1 a stale PID is skipped; E2 the reaper cannot signal
        // itself; E3 signal alone cannot complete quiescence. The real Docker
        // case supplies the effectful R1/E1-E3 witness.
        let command = exec_orphan_reaper_command();
        let script = command.argv.last().expect("reaper carries a script");
        assert!(script.contains(&format!("{EXEC_PID_DIR}/*.pid")));
        assert!(script.contains(&format!("${{{EXEC_PID_FILE_ENV}:-}}")));
        assert!(script.contains("/proc/$pid/stat"));
        assert!(script.contains(EXEC_PID_START_SUFFIX));
        assert!(script.contains("wait_for_quiet"));
        assert_eq!(command.stdio, pc::Stdio::Null);
        assert!(exec_pid_file("k8s-exec-7-1").starts_with(EXEC_PID_DIR));
        let prelude = exec_pid_file_prelude();
        assert!(prelude.contains(&format!("mkdir -p {EXEC_PID_DIR}")));
        assert!(prelude.contains(EXEC_PID_FILE_ENV));
        assert!(prelude.contains("record_exec_identity"));
        assert!(prelude.contains(EXEC_PID_START_SUFFIX));
    }
}
