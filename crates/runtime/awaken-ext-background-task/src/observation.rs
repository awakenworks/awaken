//! The durable form of one Task observation a process hands to the Session.
//!
//! A Task's tool runs in the process that holds its tenure. When the Run that
//! folds the result may execute elsewhere, the result crosses as this record,
//! carried privately on that Run's dispatch row. The record is data, not
//! authority: StepStart still folds it only when its tenure equals the
//! committed attempt.
//!
//! Decision table (causes → effect) for [`TaskObservationRecord::decode`]:
//! | Rule | cause | effect |
//! |---|---|---|
//! | C4 | `version` is 1 and every field is known and valid | the record |
//! | C5a | `version` missing or not an unsigned integer | `Decode` |
//! | C5b | `version` is any other number | `Version(n)` |
//! | C5c | an unknown field, here or inside the observation | `Decode` |
//! | C5d | empty Task id or holder, or a zero epoch | `Decode` / `EmptyHolder` |

use awaken_lease_contract::fence::{Epoch, Tenure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{
    BackgroundTaskCompletion, BackgroundTaskEnd, BackgroundTaskId, BackgroundTaskWaitCandidate,
    BackgroundWait, TaskTenure,
};

/// The one admitted record version. A record of any other version is refused,
/// never read as an older or newer shape.
pub const TASK_OBSERVATION_VERSION: u32 = 1;

/// What the holder observed about its attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskObservation {
    /// The attempt produced its terminal candidate.
    Ended { end: BackgroundTaskEnd },
    /// The attempt entered a durable wait; `renew_lease` marks a watchdog
    /// checkpoint that extends the same attempt's lease.
    Waiting {
        wait: BackgroundWait,
        renew_lease: bool,
    },
}

/// One versioned Task observation, bound to the exact attempt tenure.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TaskObservationRecord {
    version: u32,
    task_id: BackgroundTaskId,
    holder: String,
    epoch: Epoch,
    observation: TaskObservation,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskObservationWire {
    version: u32,
    task_id: BackgroundTaskId,
    holder: String,
    epoch: Epoch,
    observation: TaskObservation,
}

/// Why a Task observation record was refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TaskObservationCodecError {
    #[error("unsupported Task observation version {0}")]
    Version(u64),
    #[error("Task observation names no holder")]
    EmptyHolder,
    #[error("invalid Task observation: {0}")]
    Decode(String),
}

impl TaskObservationRecord {
    fn new(fence: &TaskTenure, observation: TaskObservation) -> Self {
        Self {
            version: TASK_OBSERVATION_VERSION,
            task_id: fence.subject().clone(),
            holder: fence.holder().clone(),
            epoch: fence.epoch(),
            observation,
        }
    }

    /// The record of a terminal completion candidate.
    #[must_use]
    pub fn ended(completion: &BackgroundTaskCompletion) -> Self {
        Self::new(
            &completion.fence,
            TaskObservation::Ended {
                end: completion.end.clone(),
            },
        )
    }

    /// The record of a durable wait candidate.
    #[must_use]
    pub fn waiting(candidate: &BackgroundTaskWaitCandidate) -> Self {
        Self::new(
            &candidate.fence,
            TaskObservation::Waiting {
                wait: candidate.wait.clone(),
                renew_lease: candidate.renew_lease,
            },
        )
    }

    #[must_use]
    pub fn task_id(&self) -> &BackgroundTaskId {
        &self.task_id
    }

    /// The exact attempt this observation belongs to.
    #[must_use]
    pub fn tenure(&self) -> TaskTenure {
        Tenure::granted(self.task_id.clone(), self.holder.clone(), self.epoch)
    }

    #[must_use]
    pub fn observation(&self) -> &TaskObservation {
        &self.observation
    }

    /// The record's one wire form.
    #[must_use]
    pub fn encode(&self) -> Value {
        serde_json::to_value(self).expect("a Task observation record serializes")
    }

    /// Decode one record. The version is read first, so a record of another
    /// version is refused as such rather than as a shape mismatch.
    pub fn decode(value: &Value) -> Result<Self, TaskObservationCodecError> {
        let version = value
            .get("version")
            .ok_or_else(|| TaskObservationCodecError::Decode("missing version".into()))?
            .as_u64()
            .ok_or_else(|| {
                TaskObservationCodecError::Decode("version is not an unsigned integer".into())
            })?;
        if version != u64::from(TASK_OBSERVATION_VERSION) {
            return Err(TaskObservationCodecError::Version(version));
        }
        let wire: TaskObservationWire = serde_json::from_value(value.clone())
            .map_err(|error| TaskObservationCodecError::Decode(error.to_string()))?;
        if wire.holder.trim().is_empty() {
            return Err(TaskObservationCodecError::EmptyHolder);
        }
        Ok(Self {
            version: wire.version,
            task_id: wire.task_id,
            holder: wire.holder,
            epoch: wire.epoch,
            observation: wire.observation,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_tool_contract::tool::ToolTaskHandle;
    use serde_json::json;

    fn fence(generation: u64) -> TaskTenure {
        Tenure::granted(
            BackgroundTaskId::new("task").unwrap(),
            "holder".into(),
            Epoch::from_persisted(generation).unwrap(),
        )
    }

    fn ended() -> TaskObservationRecord {
        TaskObservationRecord::ended(&BackgroundTaskCompletion {
            fence: fence(3),
            end: BackgroundTaskEnd::Failed {
                message: "boom".into(),
            },
        })
    }

    fn waiting() -> TaskObservationRecord {
        TaskObservationRecord::waiting(&BackgroundTaskWaitCandidate {
            fence: fence(2),
            wait: BackgroundWait::Remote(ToolTaskHandle {
                owner: "mcp".into(),
                binding: "generation".into(),
                task_id: "remote".into(),
                poll_interval_ms: Some(50),
            }),
            renew_lease: true,
        })
    }

    #[test]
    fn current_records_round_trip_with_their_exact_tenure() {
        // C4: both observation kinds survive encode/decode unchanged and keep
        // the exact attempt tenure (task, holder, epoch).
        for (record, generation) in [(ended(), 3), (waiting(), 2)] {
            let wire = record.encode();
            assert_eq!(wire["version"], json!(1));
            let decoded = TaskObservationRecord::decode(&wire).unwrap();
            assert_eq!(decoded, record);
            assert_eq!(decoded.tenure(), fence(generation));
        }
        assert_eq!(
            ended().encode(),
            json!({
                "version": 1,
                "task_id": "task",
                "holder": "holder",
                "epoch": 3,
                "observation": {"kind": "ended", "end": {"outcome": "failed", "message": "boom"}},
            })
        );
    }

    #[test]
    fn records_of_other_versions_and_historical_shapes_are_refused() {
        // C5a: missing or non-integer version; C5b: any other number,
        // including the zero an unset field decodes to.
        let wire = ended().encode();
        let mut missing = wire.clone();
        missing.as_object_mut().unwrap().remove("version");
        assert!(matches!(
            TaskObservationRecord::decode(&missing),
            Err(TaskObservationCodecError::Decode(_))
        ));
        for version in [json!("1"), json!(1.5), json!(-1), Value::Null] {
            let mut changed = wire.clone();
            changed["version"] = version;
            assert!(matches!(
                TaskObservationRecord::decode(&changed),
                Err(TaskObservationCodecError::Decode(_))
            ));
        }
        for version in [0_u64, 2, u64::from(u32::MAX) + 1] {
            let mut changed = wire.clone();
            changed["version"] = json!(version);
            assert_eq!(
                TaskObservationRecord::decode(&changed),
                Err(TaskObservationCodecError::Version(version))
            );
        }
        // A pre-codec process projection ({fence, end}) is not a record.
        let historical = json!({
            "fence": {"task": "task", "holder": "holder", "epoch": 3},
            "end": {"outcome": "cancelled"},
        });
        assert!(TaskObservationRecord::decode(&historical).is_err());
    }

    #[test]
    fn unknown_fields_and_invalid_coordinates_are_refused() {
        // C5c: an unknown field at either level; C5d: empty ids, zero epoch.
        let wire = waiting().encode();
        let mut unknown = wire.clone();
        unknown["legacy"] = json!(true);
        let mut inner = wire.clone();
        inner["observation"]["legacy"] = json!(true);
        let mut zero = wire.clone();
        zero["epoch"] = json!(0);
        let mut empty_task = wire.clone();
        empty_task["task_id"] = json!(" ");
        for refused in [unknown, inner, zero, empty_task] {
            assert!(matches!(
                TaskObservationRecord::decode(&refused),
                Err(TaskObservationCodecError::Decode(_))
            ));
        }
        let mut empty_holder = wire;
        empty_holder["holder"] = json!("");
        assert_eq!(
            TaskObservationRecord::decode(&empty_holder),
            Err(TaskObservationCodecError::EmptyHolder)
        );
    }
}
