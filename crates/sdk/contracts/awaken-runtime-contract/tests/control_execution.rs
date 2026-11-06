//! Contract pins for the in-process control/error boundary types.
//!
//! NOTE ON "serde round-trip": `control::LiveCommand`, `control::Error`, and
//! `execution::Error` are deliberately NOT `Serialize`/`Deserialize` — they are the
//! *in-process* live-control + fault vocabulary (a `LiveCommand` is handed to a
//! `LiveRunControl` inside the runtime, never persisted or sent over a wire). So a
//! serde round-trip is not applicable to them. Instead we pin the contract a consumer
//! actually depends on: exact variant DISCRIMINATION (tag), a value round-trip through
//! the port (`LiveRunControl`), and the stable `Display` text of each error.

use std::sync::Mutex;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_runtime_contract::control::{Error as ControlError, LiveCommand, LiveRunControl};
use awaken_runtime_contract::execution::Error as ExecError;

// --- Item 2: control::LiveCommand ------------------------------------------

/// A recording `LiveRunControl` double: proves a `LiveCommand` crosses the port by
/// value and arrives byte-for-byte equal (Clone + PartialEq are the round-trip).
#[derive(Default)]
struct RecordingControl {
    delivered: Mutex<Vec<LiveCommand>>,
    reject: bool,
}

impl LiveRunControl for RecordingControl {
    fn deliver(&self, command: LiveCommand) -> Result<(), ControlError> {
        if self.reject {
            return Err(ControlError::Rejected("closed".into()));
        }
        self.delivered.lock().unwrap().push(command);
        Ok(())
    }
}

#[test]
fn live_command_round_trips_through_the_control_port_by_value() {
    let control = RecordingControl::default();
    let commands = vec![
        LiveCommand::Cancel {
            run_id: RunId("r1".into()),
        },
        LiveCommand::Pause {
            run_id: RunId("r2".into()),
        },
        LiveCommand::Wake {
            run_id: RunId("r3".into()),
            reason: "operator".into(),
        },
    ];
    for c in &commands {
        control.deliver(c.clone()).expect("delivered");
    }
    assert_eq!(
        *control.delivered.lock().unwrap(),
        commands,
        "arrive equal, in order"
    );
}

#[test]
fn live_command_variants_are_distinct() {
    let cancel = LiveCommand::Cancel {
        run_id: RunId("r".into()),
    };
    let pause = LiveCommand::Pause {
        run_id: RunId("r".into()),
    };
    let wake = LiveCommand::Wake {
        run_id: RunId("r".into()),
        reason: "x".into(),
    };
    // Same inner id, different variant tag ⇒ not equal (the discriminant matters).
    assert_ne!(cancel, pause);
    assert_ne!(pause, wake);
    // Wake carries a reason that participates in equality.
    assert_ne!(
        wake,
        LiveCommand::Wake {
            run_id: RunId("r".into()),
            reason: "y".into(),
        }
    );
}

#[test]
fn a_rejecting_control_surfaces_the_reason() {
    let control = RecordingControl {
        reject: true,
        ..Default::default()
    };
    assert_eq!(
        control.deliver(LiveCommand::Cancel {
            run_id: RunId("r".into()),
        }),
        Err(ControlError::Rejected("closed".into())),
    );
}

// --- Item 2: control::Error + execution::Error Display ---------------------

#[test]
fn control_error_display_is_pinned() {
    assert_eq!(ControlError::NotActive.to_string(), "run is not active");
    assert_eq!(
        ControlError::Rejected("bad target".into()).to_string(),
        "live command rejected: bad target",
    );
    // PartialEq discriminates the two kinds.
    assert_ne!(ControlError::NotActive, ControlError::Rejected("x".into()));
}

#[test]
fn execution_error_display_is_pinned_per_variant() {
    assert_eq!(
        ExecError::Resolution("no such agent".into()).to_string(),
        "runtime resolution failed: no such agent",
    );
    assert_eq!(
        ExecError::Execution("provider timeout".into()).to_string(),
        "runtime execution failed: provider timeout",
    );
    assert_eq!(
        ExecError::Commit("state conflict".into()).to_string(),
        "runtime commit failed: state conflict",
    );
}
