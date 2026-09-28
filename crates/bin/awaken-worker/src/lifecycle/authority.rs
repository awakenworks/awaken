// @formal entrypoint session_realization.concurrent_phase_drivers_are_serialized_per_session
//! One guarded projection of the current registry receipt per Worker instance.
//! The durable registry remains authoritative. Heartbeat scheduling and local
//! Session admission consult this same projection; neither keeps a second lease.

pub(crate) use awaken_authority_clock::authority_proof_started;
use awaken_provisioning_contract::CurrentExecutionAuthority;
use awaken_runtime_contract::authority_lease::{
    AuthorityLeaseTiming, AuthorityProofStarted as ProofStarted,
};
use std::{io, sync::Mutex, time::Duration};

#[derive(Clone, Copy)]
pub(super) struct Receipt {
    pub(super) started: ProofStarted,
    pub(super) timing: AuthorityLeaseTiming,
}

impl Receipt {
    fn remaining_at(self, now: ProofStarted) -> Option<Duration> {
        self.timing.remaining_proof_at(self.started, now)
    }
}

#[derive(Default)]
enum Proof {
    #[default]
    Starting,
    Live(Receipt),
    Closed,
}

fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "Worker execution authority is not current",
    )
}

impl Proof {
    fn current(&mut self, now: ProofStarted) -> io::Result<(Receipt, Duration)> {
        if let Self::Live(receipt) = self
            && let Some(remaining) = receipt.remaining_at(now)
        {
            return Ok((*receipt, remaining));
        }
        // Observed expiry is irreversible for this Worker incarnation.
        if matches!(self, Self::Live(_)) {
            *self = Self::Closed;
        }
        Err(denied())
    }

    fn apply(&mut self, receipt: Receipt, now: ProofStarted) -> io::Result<()> {
        let valid_previous = match self {
            Self::Starting => true,
            Self::Live(previous) => {
                previous.remaining_at(now).is_some()
                    && previous.started.elapsed_at(receipt.started).is_some()
            }
            Self::Closed => false,
        };
        if !valid_previous || receipt.remaining_at(now).is_none() {
            *self = Self::Closed;
            return Err(denied());
        }
        *self = Self::Live(receipt);
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct WorkerAuthorityProof(Mutex<Proof>);

impl WorkerAuthorityProof {
    pub(crate) fn apply(
        &self,
        started: ProofStarted,
        timing: AuthorityLeaseTiming,
    ) -> io::Result<()> {
        let mut proof = self.0.lock().map_err(|_| denied())?;
        proof.apply(Receipt { started, timing }, authority_proof_started()?)
    }

    /// Copies are for cadence/metrics only; effects must use `with_current`.
    pub(super) fn snapshot(&self) -> io::Result<(Receipt, Duration)> {
        self.0
            .lock()
            .map_err(|_| denied())?
            .current(authority_proof_started()?)
    }

    pub(super) fn remaining(&self) -> Duration {
        self.snapshot()
            .map(|(_, remaining)| remaining)
            .unwrap_or_default()
    }

    pub(crate) fn close(&self) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Proof::Closed;
    }
}

impl CurrentExecutionAuthority for WorkerAuthorityProof {
    fn with_current(&self, effect: &mut dyn FnMut(Duration) -> io::Result<()>) -> io::Result<()> {
        let mut proof = self.0.lock().map_err(|_| denied())?;
        let (_, remaining) = proof.current(authority_proof_started()?)?;
        effect(remaining)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, mpsc};

    fn at(base: ProofStarted, millis: u64) -> ProofStarted {
        let delta = Duration::from_millis(millis);
        ProofStarted::from_clock_sample(base.scheduled() + delta, base.elapsed_clock() + delta)
    }

    #[test]
    fn receipt_decision_table_never_revives_expired_or_closed_worker() {
        // Causes: initial/renewal, old proof live, new request latency, explicit
        // close, request ordering. Effects: install exactly the current receipt
        // or irreversibly close. Constraint: TTL=30ms => proof window=20ms.
        // | rule | old | request start | response | effect |
        // | W1 | Starting | 0 | 19 | live, 1ms remaining |
        // | W2 | Starting | 0 | 20/21 | closed |
        // | W3 | Live@0 | 10 | 19 | live@10, 11ms remaining |
        // | W4 | Live@0 | 10 | 20/21 | closed, even though new proof is fresh |
        // | W5 | Closed | 10 | 11 | closed |
        // | W6 | Live@10 | 0 | 11 | closed, backward receipt |
        // | W7 | Live@0 | 10 | 30 | closed, aged new receipt |
        // | W10 | Starting | 10 | 9 | closed, regressed response clock |
        // | W11 | Live@0 | 10 | 9 | closed, regressed response clock |
        // W10/W11 correspond to ApplyHeartbeatAt(-1) in the existing
        // WorkerExecutionAuthority model. Both must remain closed after a
        // later well-formed receipt; this is not general rollback detection.
        let base = authority_proof_started().unwrap();
        let receipt = |start| Receipt {
            started: at(base, start),
            timing: AuthorityLeaseTiming::from_ttl_ms(30),
        };
        for (rule, previous, start, response, expected) in [
            ("W1", None, 0, 19, Some(1)),
            ("W2a", None, 0, 20, None),
            ("W2b", None, 0, 21, None),
            ("W3", Some(0), 10, 19, Some(11)),
            ("W4a", Some(0), 10, 20, None),
            ("W4b", Some(0), 10, 21, None),
            ("W5", Some(u64::MAX), 10, 11, None),
            ("W6", Some(10), 0, 11, None),
            ("W7", Some(0), 10, 30, None),
            ("W10", None, 10, 9, None),
            ("W11", Some(0), 10, 9, None),
        ] {
            let mut proof = match previous {
                None => Proof::Starting,
                Some(u64::MAX) => Proof::Closed,
                Some(start) => Proof::Live(receipt(start)),
            };
            assert_eq!(
                proof.apply(receipt(start), at(base, response)).is_ok(),
                expected.is_some(),
                "{rule}"
            );
            assert_eq!(
                proof
                    .current(at(base, response))
                    .ok()
                    .map(|(_, remaining)| remaining.as_millis()),
                expected,
                "{rule}"
            );
            if expected.is_none() {
                assert!(
                    proof.apply(receipt(100), at(base, 100)).is_err(),
                    "{rule}: cannot revive"
                );
            }
        }
        let mut expired = Proof::Live(receipt(0));
        assert!(expired.current(at(base, 20)).is_err());
        assert!(
            expired.current(at(base, 1)).is_err(),
            "observed expiry is not reversible by clock regression"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn suspend_consumes_proof_without_advancing_scheduling_clock() {
        // W8: scheduler Instant stalls during suspend; BOOTTIME advances to
        // expiry. Admission closes although cadence has not observed its tick.
        // W9: retain submillisecond precision; never round remaining upward.
        let base = authority_proof_started().unwrap();
        let mut proof = Proof::Live(Receipt {
            started: base,
            timing: AuthorityLeaseTiming::from_ttl_ms(30),
        });
        let now = ProofStarted::from_clock_sample(
            base.scheduled(),
            base.elapsed_clock() + Duration::from_micros(19_999),
        );
        assert_eq!(
            proof.current(now).unwrap().1,
            Duration::from_micros(1),
            "W9"
        );
        let now = ProofStarted::from_clock_sample(
            base.scheduled(),
            base.elapsed_clock() + Duration::from_millis(20),
        );
        assert!(proof.current(now).is_err(), "W8");
    }

    fn live() -> Arc<WorkerAuthorityProof> {
        let proof = Arc::new(WorkerAuthorityProof::default());
        proof
            .apply(
                authority_proof_started().unwrap(),
                AuthorityLeaseTiming::from_ttl_ms(60_000),
            )
            .unwrap();
        proof
    }

    #[test]
    fn effect_and_close_share_one_synchronous_guard() {
        // W10 effect wins the lock: close cannot pass the callback boundary;
        // W11 close wins: no callback or later Applied receipt reopens it.
        // Channels establish entry/attempt/release order, not timing sleeps.
        let authority = live();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let effect_authority = authority.clone();
        let effect = std::thread::spawn(move || {
            effect_authority.with_current(&mut |remaining| {
                assert!(!remaining.is_zero());
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                Ok(())
            })
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            matches!(
                authority.0.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            ),
            "W10"
        );
        let closer = authority.clone();
        let (attempt_tx, attempt_rx) = mpsc::channel();
        let close = std::thread::spawn(move || {
            attempt_tx.send(()).unwrap();
            closer.close();
        });
        attempt_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        release_tx.send(()).unwrap();
        effect.join().unwrap().unwrap();
        close.join().unwrap();
        authority
            .with_current(&mut |_| panic!("W11 callback after close"))
            .unwrap_err();
        assert!(
            authority
                .apply(
                    authority_proof_started().unwrap(),
                    AuthorityLeaseTiming::from_ttl_ms(60_000)
                )
                .is_err()
        );
    }

    #[test]
    fn missing_authority_and_poison_deny_but_effect_error_does_not_mint_or_revoke() {
        // W12 Starting denies without preventing a first valid receipt.
        // W13 callback error propagates once, retaining only the SAME receipt.
        // W14 callback unwind poisons guard: no later grant/renewal; close is
        // still possible. A replacement Worker requires a fresh allocation.
        let starting = WorkerAuthorityProof::default();
        starting.with_current(&mut |_| panic!("W12")).unwrap_err();
        starting
            .apply(
                authority_proof_started().unwrap(),
                AuthorityLeaseTiming::from_ttl_ms(60_000),
            )
            .unwrap();
        let before = starting.snapshot().unwrap().0.started.scheduled();
        let mut calls = 0;
        let error = starting
            .with_current(&mut |_| {
                calls += 1;
                Err(io::ErrorKind::BrokenPipe.into())
            })
            .unwrap_err();
        assert_eq!(calls, 1, "W13");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(starting.snapshot().unwrap().0.started.scheduled(), before);
        assert!(
            std::panic::catch_unwind(|| {
                let _ = starting.with_current(&mut |_| panic!("W14"));
            })
            .is_err()
        );
        starting
            .with_current(&mut |_| panic!("W14 denied"))
            .unwrap_err();
        assert!(
            starting
                .apply(
                    authority_proof_started().unwrap(),
                    AuthorityLeaseTiming::from_ttl_ms(60_000)
                )
                .is_err()
        );
        starting.close();
        assert!(starting.remaining().is_zero());
        assert!(live().remaining() > Duration::ZERO, "W14 new incarnation");
    }
}
