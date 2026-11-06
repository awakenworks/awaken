// @formal surface requirement=runtime.cancellation.terminal_cancellation
// @formal reason: The Session's foreground Run identity and its interrupt signal share one lock and
// @formal reason: every write replaces both; poison is a typed refusal. Decision tables in this file
// @formal reason: cover each write and each poisoned operation; this is not a formal-proof claim.
//! The one record of the foreground Run a Session drives and its interrupt
//! signal ([`ForegroundRunRecord`]).
//!
//! Identity and signal live under one lock and every write replaces them
//! together. A reader therefore observes a [`ForegroundRun`] whose signal
//! belongs to the attempt that installed it, never one Run's identity beside
//! another attempt's signal. Interruption, quiescence and ownership checks
//! consume the observed value; none of them keeps the lock across an await.

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_runtime_contract::CancellationToken;

/// One observation of the foreground Run a Session drives and the signal that
/// interrupts it. Both fields come from the same write of the observer's
/// record, so the signal always belongs to the attempt that installed it: to
/// `run` when present, or to an auxiliary attempt that has no Run of this
/// Session. Holding the value grants no lock and is never re-read.
#[derive(Clone, Debug, Default)]
pub(crate) struct ForegroundRun {
    pub(crate) run: Option<RunId>,
    pub(crate) cancellation: Option<CancellationToken>,
}

/// A writer panicked while holding the foreground record. The record is then
/// unreadable: every operation that needs it is refused, none guesses idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the Session's foreground Run record is unreadable after a panic")]
pub(crate) struct ForegroundRunPoisoned;

impl From<ForegroundRunPoisoned> for crate::SessionExecutionError {
    fn from(error: ForegroundRunPoisoned) -> Self {
        crate::SessionExecutionError::classified("foreground_run_poisoned", error.to_string())
    }
}

/// The foreground Run of one Session and the signal that interrupts it.
///
/// Shared by `Arc` only where an auxiliary attempt (an Outcome grader) lends
/// its signal to the Session it serves; every other holder is the Session's
/// own context.
#[derive(Debug, Default)]
pub(crate) struct ForegroundRunRecord(awaken_sync::InvariantLock<ForegroundRun>);

impl ForegroundRunRecord {
    fn with<R>(&self, f: impl FnOnce(&mut ForegroundRun) -> R) -> Result<R, ForegroundRunPoisoned> {
        let mut record = self.0.lock().map_err(|_| ForegroundRunPoisoned)?;
        Ok(f(&mut record))
    }

    /// The identity and signal installed by one write.
    pub(crate) fn observe(&self) -> Result<ForegroundRun, ForegroundRunPoisoned> {
        self.with(|record| record.clone())
    }

    pub(crate) fn has_run(&self) -> Result<bool, ForegroundRunPoisoned> {
        self.with(|record| record.run.is_some())
    }

    pub(crate) fn is_current(&self, run: &RunId) -> Result<bool, ForegroundRunPoisoned> {
        self.with(|record| record.run.as_ref() == Some(run))
    }

    /// Admit `run` as the foreground. Its attempt has no signal yet, so any
    /// signal an earlier attempt left behind is dropped with its identity.
    pub(crate) fn begin(&self, run: RunId) -> Result<(), ForegroundRunPoisoned> {
        self.with(|record| {
            *record = ForegroundRun {
                run: Some(run),
                cancellation: None,
            };
        })
    }

    /// Install a fresh signal for an attempt of `run`, together with `run`.
    pub(crate) fn arm(&self, run: &RunId) -> Result<CancellationToken, ForegroundRunPoisoned> {
        let token = CancellationToken::new();
        self.with(|record| {
            *record = ForegroundRun {
                run: Some(run.clone()),
                cancellation: Some(token.clone()),
            };
        })?;
        Ok(token)
    }

    /// Lend an auxiliary attempt's signal (an Outcome grader) to this Session,
    /// so interrupting the Session reaches it. The attempt has no Run of this
    /// Session, so the signal is installed only while no foreground Run is:
    /// `false` leaves that Run's identity and signal untouched.
    pub(crate) fn lend_auxiliary(
        &self,
        token: CancellationToken,
    ) -> Result<bool, ForegroundRunPoisoned> {
        self.with(|record| {
            if record.run.is_some() {
                return false;
            }
            record.cancellation = Some(token);
            true
        })
    }

    /// Retire `run` with its signal when it is still the foreground; a later
    /// Run that replaced it keeps its own identity and signal.
    pub(crate) fn release(&self, run: &RunId) -> Result<bool, ForegroundRunPoisoned> {
        self.with(|record| {
            if record.run.as_ref() != Some(run) {
                return false;
            }
            *record = ForegroundRun::default();
            true
        })
    }

    /// Hold the record's lock, for fixtures that poison it by panicking.
    #[cfg(test)]
    pub(crate) fn hold_for_test(&self) -> std::sync::MutexGuard<'_, ForegroundRun> {
        self.0.lock().expect("test foreground record")
    }

    /// Install a foreground identity without an attempt signal, replacing the
    /// whole record, for fixtures that model an admitted Run directly.
    #[cfg(test)]
    pub(crate) fn install_for_test(&self, run: Option<RunId>) {
        *self.0.lock().expect("test foreground record") = ForegroundRun {
            run,
            cancellation: None,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(id: &str) -> RunId {
        RunId(id.into())
    }

    #[test]
    fn every_write_installs_identity_and_signal_together() {
        // Causes: C1 operation begin/arm/lend/release; C2 the record holds no
        // Run, the same Run, or another Run; C3 an earlier attempt left a
        // signal. Effects: E1 the observed signal belongs to the observed Run
        // (or to a lent auxiliary attempt while no Run is installed); E2 the
        // record is unchanged; E3 a stale signal never survives a new identity.
        // R1 begin(B) after A armed => {B, none} (E1+E3).
        // R2 arm(B) => {B, B's fresh signal} (E1).
        // R3 release(A) while B is current => unchanged, false (E2).
        // R4 release(B) while B is current => {none, none}, true (E1+E3).
        // R5 lend while no Run => {none, lent}, true (E1).
        // R6 lend while B is current => unchanged, false (E2).
        // R7 begin after a lent signal => {B, none} (E3).
        let record = ForegroundRunRecord::default();
        let first = record.arm(&run("a")).unwrap();
        record.begin(run("b")).unwrap();
        let observed = record.observe().unwrap();
        assert_eq!(observed.run, Some(run("b")), "R1");
        assert!(observed.cancellation.is_none(), "R1 A's signal left with A");

        let armed = record.arm(&run("b")).unwrap();
        let observed = record.observe().unwrap();
        assert_eq!(observed.run, Some(run("b")), "R2");
        observed.cancellation.expect("R2 signal").cancel();
        assert!(
            armed.is_cancelled() && !first.is_cancelled(),
            "R2 B's own signal"
        );

        assert!(!record.release(&run("a")).unwrap(), "R3");
        assert_eq!(record.observe().unwrap().run, Some(run("b")), "R3");
        assert!(record.release(&run("b")).unwrap(), "R4");
        let observed = record.observe().unwrap();
        assert!(
            observed.run.is_none() && observed.cancellation.is_none(),
            "R4"
        );

        let lent = CancellationToken::new();
        assert!(record.lend_auxiliary(lent.clone()).unwrap(), "R5");
        let observed = record.observe().unwrap();
        assert!(observed.run.is_none(), "R5 no Run identity");
        observed.cancellation.expect("R5 lent signal").cancel();
        assert!(lent.is_cancelled(), "R5 the lent attempt's own signal");

        record.arm(&run("c")).unwrap();
        assert!(
            !record.lend_auxiliary(CancellationToken::new()).unwrap(),
            "R6"
        );
        let observed = record.observe().unwrap();
        assert_eq!(observed.run, Some(run("c")), "R6");
        assert!(
            !observed.cancellation.unwrap().is_cancelled(),
            "R6 C's signal"
        );

        assert!(record.release(&run("c")).unwrap());
        assert!(record.lend_auxiliary(CancellationToken::new()).unwrap());
        record.begin(run("d")).unwrap();
        assert!(record.observe().unwrap().cancellation.is_none(), "R7");
    }

    #[test]
    fn a_poisoned_record_refuses_every_operation() {
        // Causes: C1 a writer panicked while holding the record. Effects: E1
        // each operation returns the typed refusal; E2 no operation panics or
        // reports an idle Session. One row per operation.
        let record = std::sync::Arc::new(ForegroundRunRecord::default());
        record.begin(run("held")).unwrap();
        let writer = std::thread::spawn({
            let record = record.clone();
            move || {
                let _held = record.hold_for_test();
                panic!("poison the foreground record");
            }
        });
        assert!(writer.join().is_err());
        assert_eq!(record.observe().unwrap_err(), ForegroundRunPoisoned);
        assert_eq!(record.has_run(), Err(ForegroundRunPoisoned));
        assert_eq!(record.is_current(&run("held")), Err(ForegroundRunPoisoned));
        assert_eq!(record.begin(run("next")), Err(ForegroundRunPoisoned));
        assert_eq!(record.arm(&run("next")).unwrap_err(), ForegroundRunPoisoned);
        assert_eq!(
            record.lend_auxiliary(CancellationToken::new()),
            Err(ForegroundRunPoisoned)
        );
        assert_eq!(record.release(&run("held")), Err(ForegroundRunPoisoned));
    }
}
