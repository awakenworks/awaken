//! Task observations delivered to the Session application.
//!
//! A BackgroundTask's holder hands each observation (terminal candidate or
//! durable wait) to the Session application as a [`TaskAttention`]. The
//! application admits one ordinary background Run whose dispatch row carries
//! the observation privately ([`PrivateTaskInput`]); the claiming process folds
//! it at StepStart against committed truth. The application interprets no
//! observation: the Task owner decodes the records.

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::Id as RunId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The registered Worker route that delivers a [`TaskAttention`].
pub const WORKER_TASK_ATTENTION_PATH: &str = "/v1/worker/task/attention";

/// The one admitted private Task input layout.
pub const PRIVATE_TASK_INPUT_VERSION: u32 = 1;

/// Task observations one Run carries privately on its dispatch row.
///
/// Decision table (causes → effect) for decoding a persisted row:
/// | Rule | cause | effect |
/// |---|---|---|
/// | C2 | `version` is 1 and at least one observation | the input |
/// | C3a | `version` missing, not an integer, or another number | refused |
/// | C3b | an unknown field | refused |
/// | C3c | no observation | refused |
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "PrivateTaskInputWire")]
pub struct PrivateTaskInput {
    version: u32,
    observations: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrivateTaskInputWire {
    version: u32,
    observations: Vec<Value>,
}

/// Why a private Task input was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PrivateTaskInputError {
    #[error("unsupported private Task input version {0}")]
    Version(u32),
    #[error("private Task input carries no observation")]
    Empty,
}

impl TryFrom<PrivateTaskInputWire> for PrivateTaskInput {
    type Error = PrivateTaskInputError;

    fn try_from(wire: PrivateTaskInputWire) -> Result<Self, Self::Error> {
        if wire.version != PRIVATE_TASK_INPUT_VERSION {
            return Err(PrivateTaskInputError::Version(wire.version));
        }
        Self::new(wire.observations)
    }
}

impl PrivateTaskInput {
    /// Carry the Task owner's encoded observation records.
    pub fn new(observations: Vec<Value>) -> Result<Self, PrivateTaskInputError> {
        if observations.is_empty() {
            return Err(PrivateTaskInputError::Empty);
        }
        Ok(Self {
            version: PRIVATE_TASK_INPUT_VERSION,
            observations,
        })
    }

    /// The encoded records, for the Task owner's codec.
    #[must_use]
    pub fn observations(&self) -> &[Value] {
        &self.observations
    }
}

/// One Task observation its holder hands to the Session application.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskAttention {
    /// Deterministic over Thread, Task, tenure and observation kind: a retry
    /// after a lost acknowledgement reaches the same Run.
    pub operation_id: String,
    /// The Session's primary Thread.
    pub thread: String,
    /// The holder in the observation's Task tenure.
    pub holder: String,
    /// The model-visible notice the background Run starts from.
    pub message: Message,
    /// The observation, carried privately to the Run that folds it. `None`
    /// when the attention only wakes the Thread (input required) and the slot
    /// holds no new candidate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<PrivateTaskInput>,
}

/// The application durably admitted the Run carrying the observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskAttentionAck {
    pub run_id: RunId,
}

const REGISTERED_HOLDER_TAG: &str = "registered-task-holder-v1";

/// The Task holder of one registered Worker incarnation. The Coordinator
/// recomputes it from the authenticated Worker identity and refuses an
/// attention whose Task another Worker or incarnation holds. The encoding is
/// unambiguous for any owner and incarnation text.
#[must_use]
pub fn registered_task_holder(worker_id: &str, incarnation_id: &str) -> String {
    serde_json::to_string(&(REGISTERED_HOLDER_TAG, worker_id, incarnation_id))
        .expect("a registered Task holder serializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn private_task_input_admits_only_its_current_version() {
        // C2 round trip; C3a every other version and historical shape; C3b an
        // unknown field; C3c no observation.
        let input = PrivateTaskInput::new(vec![json!({"version": 1})]).unwrap();
        let wire = serde_json::to_value(&input).unwrap();
        assert_eq!(
            wire,
            json!({"version": 1, "observations": [{"version": 1}]})
        );
        assert_eq!(
            serde_json::from_value::<PrivateTaskInput>(wire.clone()).unwrap(),
            input
        );
        for version in [
            json!(0),
            json!(2),
            json!(u32::MAX),
            json!("1"),
            json!(1.5),
            Value::Null,
        ] {
            let mut changed = wire.clone();
            changed["version"] = version;
            assert!(serde_json::from_value::<PrivateTaskInput>(changed).is_err());
        }
        let mut missing = wire.clone();
        missing.as_object_mut().unwrap().remove("version");
        assert!(serde_json::from_value::<PrivateTaskInput>(missing).is_err());
        let mut unknown = wire.clone();
        unknown["legacy"] = json!(true);
        assert!(serde_json::from_value::<PrivateTaskInput>(unknown).is_err());
        // A pre-codec row that carried a bare record list is not an input.
        assert!(serde_json::from_value::<PrivateTaskInput>(json!([{"version": 1}])).is_err());
        assert!(
            serde_json::from_value::<PrivateTaskInput>(json!({"version": 1, "observations": []}))
                .is_err()
        );
        assert_eq!(
            PrivateTaskInput::new(Vec::new()),
            Err(PrivateTaskInputError::Empty)
        );
    }

    #[test]
    fn registered_holders_differ_per_worker_and_incarnation() {
        // A2/A3: the holder is a function of exactly (Worker, incarnation);
        // separator characters in either part cannot make two pairs collide.
        let holder = registered_task_holder("worker/a:1", "incarnation");
        assert_eq!(holder, registered_task_holder("worker/a:1", "incarnation"));
        for (worker, incarnation) in [
            ("worker/a:1", "other"),
            ("other", "incarnation"),
            ("worker/a", ":1\",incarnation"),
            ("", "incarnation"),
        ] {
            assert_ne!(holder, registered_task_holder(worker, incarnation));
        }
    }
}
