// @formal surface requirement=runtime.background_tool_execution.durable_session_scope
// @formal reason: The committed BackgroundTask slot's state key and the holder a platform crate
// @formal reason: reads from it. The extension keeps the aggregate, its decisions and codec.
//! The committed slot of one BackgroundTask, as platform crates read it.
//!
//! A BackgroundTask is Thread state owned by `awaken-ext-background-task`: the
//! extension writes one slot per Task under [`background_task_state_key`] and
//! owns its aggregate, transitions and codec. A Coordinator that answers a
//! registered Worker's Task tenure read needs only two facts from committed
//! State: where the slot lives, and which holder its current attempt names.
//! This module carries exactly those, so the Coordinator never depends on the
//! extension.
//! This is an ownership projection, not the complete aggregate decoder or an
//! execution permit. The Task owner validates the complete namespace at
//! StepStart; a decoding failure stops that attempt before model/tool work.

use serde::Deserialize;

use crate::agent::state::{Command, Key, Scope, Store};

/// The Thread-scope key prefix of every BackgroundTask slot.
pub const BACKGROUND_TASK_STATE_PREFIX: &str = "background_task/";

/// The Thread-scope state key of the Task `task_id`.
#[must_use]
pub fn background_task_state_key(task_id: &str) -> String {
    format!("{BACKGROUND_TASK_STATE_PREFIX}{task_id}")
}

/// The committed value of the Task `task_id`'s slot, or `None` when the
/// committed State holds no slot for it (never written, or removed).
#[must_use]
pub fn committed_task_slot(commands: &[Command], task_id: &str) -> Option<serde_json::Value> {
    Store::rebuild(commands)
        .get(Scope::Thread, &Key(background_task_state_key(task_id)))
        .cloned()
}

#[derive(Deserialize)]
struct SlotView {
    id: String,
    origin: OriginView,
    lifecycle: LifecycleView,
}

#[derive(Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum LifecycleView {
    Requested {},
    Running { attempt: AttemptView },
    Waiting { attempt: AttemptView },
    Cancelling { attempt: AttemptView },
    Ended {},
}

#[derive(Deserialize)]
struct OriginView {
    thread_id: String,
    run_id: String,
}

#[derive(Deserialize)]
struct AttemptView {
    worker_id: String,
    epoch: std::num::NonZeroU64,
}

/// The holder named by the current attempt of the committed slot of the Task
/// `task_id`.
///
/// Decision table (slot → result):
/// | Rule | slot | result |
/// |---|---|---|
/// | T1 | Running, Waiting or Cancelling | `Some(holder)` of its attempt |
/// | T2 | Requested or Ended | `None`: no attempt holds the Task |
/// | T3 | names another Task, an unknown state, or a phase without its attempt | refused |
pub fn committed_task_attempt(
    task_id: &str,
    thread_id: &str,
    origin_run_id: &str,
    slot: &serde_json::Value,
) -> Result<Option<(String, std::num::NonZeroU64)>, String> {
    let view = SlotView::deserialize(slot)
        .map_err(|error| format!("committed Task slot does not decode: {error}"))?;
    if view.id != task_id {
        return Err(format!(
            "committed Task slot for {task_id} names Task {}",
            view.id
        ));
    }
    if view.origin.thread_id != thread_id || view.origin.run_id != origin_run_id {
        return Err("committed Task origin does not match the addressed Thread and Run".into());
    }
    Ok(match view.lifecycle {
        LifecycleView::Requested {} | LifecycleView::Ended {} => None,
        LifecycleView::Running { attempt }
        | LifecycleView::Waiting { attempt }
        | LifecycleView::Cancelling { attempt } => {
            if attempt.worker_id.trim().is_empty() {
                return Err("committed Task holder is empty".into());
            }
            Some((attempt.worker_id, attempt.epoch))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::state::{Action, MergePolicy};
    use serde_json::json;

    fn set(task: &str, value: serde_json::Value) -> Command {
        Command {
            key: Key(background_task_state_key(task)),
            scope: Scope::Thread,
            merge: MergePolicy::Exclusive,
            run_id: None,
            action: Action::Set(value),
        }
    }

    fn slot(task: &str, lifecycle: serde_json::Value) -> serde_json::Value {
        json!({
            "id": task,
            "origin": {"thread_id": "thread", "run_id": "run", "operation_id": "op"},
            "invocation": {"call": {}},
            "lifecycle": lifecycle,
            "revision": 3,
        })
    }

    fn attempt() -> serde_json::Value {
        json!({"worker_id": "holder-a", "epoch": 4, "lease_expires_at_ms": 900, "policy": {}})
    }

    #[test]
    fn the_committed_slot_is_the_last_thread_value_under_the_task_key() {
        let first = slot("task-1", json!({"state": "requested"}));
        let second = slot("task-1", json!({"state": "running", "attempt": attempt()}));
        let state = vec![
            set("task-1", first),
            set("task-2", slot("task-2", json!({"state": "requested"}))),
            set("task-1", second.clone()),
        ];
        assert_eq!(committed_task_slot(&state, "task-1"), Some(second));
        assert_eq!(committed_task_slot(&state, "task-3"), None, "never written");
        let mut removed = state.clone();
        removed.push(Command {
            action: Action::Remove,
            ..set("task-1", json!(null))
        });
        assert_eq!(committed_task_slot(&removed, "task-1"), None, "removed");
        assert_eq!(
            background_task_state_key("task-1"),
            "background_task/task-1"
        );
    }

    #[test]
    fn the_holder_is_read_from_the_current_attempt_only() {
        // T1: every phase that holds an attempt; extension fields are ignored.
        for state in ["running", "waiting", "cancelling"] {
            let value = slot(
                "task-1",
                json!({"state": state, "attempt": attempt(), "wait": {"reason": "resource"}}),
            );
            assert_eq!(
                committed_task_attempt("task-1", "thread", "run", &value),
                Ok(Some((
                    "holder-a".to_owned(),
                    std::num::NonZeroU64::new(4).unwrap()
                ))),
                "T1 {state}"
            );
        }
        // T2: no attempt holds the Task.
        for lifecycle in [
            json!({"state": "requested"}),
            json!({"state": "ended", "end": {"outcome": "cancelled"}}),
        ] {
            assert_eq!(
                committed_task_attempt("task-1", "thread", "run", &slot("task-1", lifecycle)),
                Ok(None),
                "T2"
            );
        }
        // T3: another Task, an unknown state, a phase without its attempt.
        for value in [
            slot("task-2", json!({"state": "running", "attempt": attempt()})),
            slot("task-1", json!({"state": "paused", "attempt": attempt()})),
            slot("task-1", json!({"state": "running"})),
            json!({"lifecycle": {"state": "running", "attempt": attempt()}}),
            json!("task-1"),
        ] {
            assert!(
                committed_task_attempt("task-1", "thread", "run", &value).is_err(),
                "T3 {value}"
            );
        }
    }
}
