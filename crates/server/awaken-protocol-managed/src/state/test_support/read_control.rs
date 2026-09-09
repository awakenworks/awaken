//! One test-only failure/pause observer for existing Runtime and repository reads.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

enum ReadTarget {
    Ordinal(usize),
    Port(&'static str),
}

struct Pause {
    target: ReadTarget,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[derive(Default)]
struct ReadState {
    trace: Vec<&'static str>,
    failed_ports: HashSet<&'static str>,
    fail_at: Option<usize>,
    pause: Option<Pause>,
}

#[derive(Clone, Default)]
pub(crate) struct ReadControl(Arc<Mutex<ReadState>>);

impl ReadControl {
    pub(crate) fn trace(&self) -> Vec<&'static str> {
        self.0.lock().unwrap().trace.clone()
    }

    pub(crate) fn count(&self, port: &str) -> usize {
        self.0
            .lock()
            .unwrap()
            .trace
            .iter()
            .filter(|value| **value == port)
            .count()
    }

    /// Arm a new observation only when no previously paused read is in flight.
    pub(crate) fn reset(&self) {
        *self.0.lock().unwrap() = ReadState::default();
    }

    pub(crate) fn fail(&self, port: &'static str, enabled: bool) {
        let mut state = self.0.lock().unwrap();
        if enabled {
            state.failed_ports.insert(port);
        } else {
            state.failed_ports.remove(port);
        }
    }

    pub(crate) fn fail_at(&self, ordinal: usize) {
        self.0.lock().unwrap().fail_at = Some(ordinal);
    }

    pub(crate) fn pause_at(&self, ordinal: usize) -> (Arc<Notify>, Arc<Notify>) {
        self.arm_pause(ReadTarget::Ordinal(ordinal))
    }

    pub(crate) fn pause_next(&self, port: &'static str) -> (Arc<Notify>, Arc<Notify>) {
        self.arm_pause(ReadTarget::Port(port))
    }

    fn arm_pause(&self, target: ReadTarget) -> (Arc<Notify>, Arc<Notify>) {
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let mut state = self.0.lock().unwrap();
        assert!(
            state.pause.is_none(),
            "one controlled read may be paused at a time"
        );
        state.pause = Some(Pause {
            target,
            entered: entered.clone(),
            release: release.clone(),
        });
        (entered, release)
    }

    /// Return the zero-based index for this port, captured before any await.
    /// Global ordinals continue to own cross-port failure/pause selection.
    pub(crate) async fn on_read(&self, port: &'static str) -> Result<usize, &'static str> {
        let (ordinal, port_index, pause) = {
            let mut state = self.0.lock().unwrap();
            let ordinal = state.trace.len();
            let port_index = state.trace.iter().filter(|value| **value == port).count();
            state.trace.push(port);
            let matches = state
                .pause
                .as_ref()
                .is_some_and(|pause| match pause.target {
                    ReadTarget::Ordinal(target) => ordinal == target,
                    ReadTarget::Port(target) => port == target,
                });
            (
                ordinal,
                port_index,
                if matches { state.pause.take() } else { None },
            )
        };
        if let Some(pause) = pause {
            pause.entered.notify_one();
            pause.release.notified().await;
        }
        // Re-read the failure plan after releasing the pause: the existing
        // event-replay race intentionally makes recovery unavailable while its
        // read is pending. No Mutex guard crosses an await.
        let mut state = self.0.lock().unwrap();
        let selected = state.fail_at == Some(ordinal);
        if selected {
            state.fail_at = None;
        }
        if selected || state.failed_ports.contains(port) {
            Err(port)
        } else {
            Ok(port_index)
        }
    }
}

#[tokio::test]
async fn read_indices_are_port_scoped_and_captured_before_pause() {
    // C1 a different port is read first; C2 recovery pauses; C3 another
    // recovery completes while the first is pending. R1 C1 => recovery starts
    // at zero, not global ordinal one. R2 C2+C3 => the paused receipt remains
    // zero, while the competing read receives one; completion order cannot
    // renumber either receipt. The global trace still records invocation order.
    let reads = ReadControl::default();
    assert_eq!(reads.on_read("session").await.unwrap(), 0);
    let (entered, release) = reads.pause_next("recovery");
    let pending = {
        let reads = reads.clone();
        tokio::spawn(async move { reads.on_read("recovery").await })
    };
    entered.notified().await;
    assert_eq!(reads.on_read("recovery").await.unwrap(), 1);
    release.notify_one();
    assert_eq!(pending.await.unwrap().unwrap(), 0);
    assert_eq!(reads.trace(), ["session", "recovery", "recovery"]);
}
