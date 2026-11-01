// @formal surface requirement=runtime.background_tool_execution.durable_session_scope
// @formal reason: This single wire/value contract strictly decodes versioned observations and exact
// @formal reason: tenures for HTTP and StepStart. Aggregate transitions, lease decisions and durable
// @formal reason: writes remain with their existing owners; decoding alone grants no execution.
//! The canonical wire vocabulary of a detached ordinary tool's Task observation.
//!
//! The BackgroundTask extension owns the aggregate and its transitions. Both
//! authenticated attention admission and the claiming Runtime decode this same
//! versioned record; neither transport defines a permissive parallel parser.

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

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_lease_contract::fence::{Epoch, Tenure, TenureScope};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use super::ToolTaskHandle;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BackgroundTaskId(String);

impl BackgroundTaskId {
    pub fn new(value: impl Into<String>) -> Result<Self, BackgroundTaskError> {
        let value = value.into();
        if value.trim().is_empty() {
            Err(BackgroundTaskError::InvalidIdentity)
        } else {
            Ok(Self(value))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for BackgroundTaskId {
    type Error = BackgroundTaskError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<BackgroundTaskId> for String {
    fn from(value: BackgroundTaskId) -> Self {
        value.0
    }
}

/// The lease family of a background task: the task (subject) held by one
/// worker. Consumer: [`TaskTenure`].
#[derive(Debug)]
pub struct BackgroundTaskScope;

impl TenureScope for BackgroundTaskScope {
    type Subject = BackgroundTaskId;
    type Holder = String;
}

/// The exact authority a worker presents for a fenced task mutation: the
/// task, the worker and the attempt epoch. It replaces a worker/epoch pair
/// that named no task, so a fence minted for one task could pass for another
/// task's attempt at the same epoch.
pub type TaskTenure = Tenure<BackgroundTaskScope>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum BackgroundWait {
    Remote(ToolTaskHandle),
    ExternalInput {},
    Resource {},
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum BackgroundTaskEnd {
    Completed {
        content: Vec<ContentBlock>,
        #[serde(default)]
        is_error: bool,
    },
    Failed {
        message: String,
    },
    Cancelled {},
    Indeterminate {
        message: String,
    },
}

#[derive(Debug, Clone)]
pub struct BackgroundTaskCompletion {
    pub fence: TaskTenure,
    pub end: BackgroundTaskEnd,
}

#[derive(Debug, Clone)]
pub struct BackgroundTaskWaitCandidate {
    pub fence: TaskTenure,
    pub wait: BackgroundWait,
    /// True only for a poll watchdog checkpoint that must extend the same
    /// durable attempt lease before resuming observation.
    pub renew_lease: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BackgroundTaskError {
    #[error("background task id must not be empty")]
    InvalidIdentity,
    #[error("background task claim requires a worker and a positive lease")]
    InvalidClaim,
    #[error("background task is already owned")]
    Busy,
    #[error("background task is terminal")]
    Terminal,
    #[error("background task transition is invalid")]
    InvalidTransition,
    #[error("background task fence is stale")]
    StaleFence,
    #[error("background task lease must advance monotonically")]
    NonMonotonicLease,
    #[error(
        "remote continuation requires an owner, binding, task id, and a positive poll interval when present"
    )]
    InvalidContinuation,
    #[error("background task clock overflow")]
    ClockOverflow,
    #[error("background task epoch overflow")]
    EpochOverflow,
    #[error("background task revision overflow")]
    RevisionOverflow,
    #[error("background task persisted state violates its aggregate invariants")]
    InvalidPersistedState,
}

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

    /// Decode and validate the complete delivered batch before a caller may
    /// admit or offer any member. The same codec law serves HTTP and StepStart.
    pub fn decode_bound_batch(
        values: &[Value],
        expected: &TaskTenure,
    ) -> Result<Vec<Self>, TaskObservationCodecError> {
        let records = values
            .iter()
            .map(Self::decode)
            .collect::<Result<Vec<_>, _>>()?;
        if records.iter().any(|record| record.tenure() != *expected) {
            return Err(TaskObservationCodecError::Decode(
                "observation batch names another Task tenure".into(),
            ));
        }
        Ok(records)
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
    use crate::tool::ToolTaskHandle;
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
        // U1/U2/U3: the same version1 codec admits each documented no-payload
        // leaf (external_input/resource/cancelled), encodes its exact legal JSON
        // bytes and tenure, but refuses an unknown sibling beside that leaf tag.
        // The outer record and observation stay valid: only the deepest sibling
        // changes. Input is JSON, so these SAME tests compile against the old
        // bare-unit model and the corrected empty-struct model; no constructor
        // syntax or alternate decoder can turn the intended Serde red into a
        // compiler/selection failure. Existing version/coordinate/Remote rules
        // above remain their own unchanged controls.
        for (rule, nested, observation) in [
            (
                "U1 external_input",
                "wait",
                json!({"kind":"waiting","wait":{"reason":"external_input"},"renew_lease":false}),
            ),
            (
                "U2 resource",
                "wait",
                json!({"kind":"waiting","wait":{"reason":"resource"},"renew_lease":false}),
            ),
            (
                "U3 cancelled",
                "end",
                json!({"kind":"ended","end":{"outcome":"cancelled"}}),
            ),
        ] {
            let wire = json!({
                "version":1, "task_id":"task", "holder":"holder", "epoch":2,
                "observation":observation,
            });
            let decoded = TaskObservationRecord::decode(&wire).expect(rule);
            assert_eq!(decoded.tenure(), fence(2), "{rule} exact tenure");
            assert_eq!(decoded.encode(), wire, "{rule} legal v1 shape");
            assert_eq!(
                serde_json::to_vec(&decoded.encode()).unwrap(),
                serde_json::to_vec(&wire).unwrap(),
                "{rule} legal v1 bytes"
            );
            let mut unknown = wire;
            unknown["observation"][nested]["future"] = json!(true);
            assert!(
                matches!(
                    TaskObservationRecord::decode(&unknown),
                    Err(TaskObservationCodecError::Decode(_))
                ),
                "{rule} unknown leaf sibling must not disappear"
            );
        }
    }

    #[test]
    fn a_bound_batch_refuses_every_foreign_tenure_and_nested_unknown_before_delivery() {
        // B1 exact terminal/waiting records, including duplicate delivery =>
        // complete decoded batch; B2 any changed task/holder/epoch or unknown
        // nested End/Wait field in any member => no returned partial batch.
        // B3 empty batch is structurally valid: the carrying PrivateTaskInput
        // separately requires a complete authenticated meta-only subject.
        for record in [ended(), waiting()] {
            let wire = record.encode();
            let expected = record.tenure();
            assert_eq!(
                TaskObservationRecord::decode_bound_batch(&[wire.clone(), wire.clone()], &expected)
                    .unwrap(),
                vec![record.clone(), record.clone()],
                "B1"
            );
            for axis in ["task_id", "holder", "epoch", "nested"] {
                let mut changed = wire.clone();
                match axis {
                    "task_id" => changed[axis] = json!("foreign-task"),
                    "holder" => changed[axis] = json!("foreign-holder"),
                    "epoch" => changed[axis] = json!(expected.epoch().get() + 1),
                    "nested" => {
                        let nested = if changed["observation"]["kind"] == "ended" {
                            "end"
                        } else {
                            "wait"
                        };
                        changed["observation"][nested]["future"] = json!(true);
                    }
                    _ => unreachable!(),
                }
                assert!(
                    TaskObservationRecord::decode_bound_batch(&[wire.clone(), changed], &expected)
                        .is_err(),
                    "B2 {axis}"
                );
            }
            assert!(
                TaskObservationRecord::decode_bound_batch(&[], &expected)
                    .unwrap()
                    .is_empty(),
                "B3 no result or grant is fabricated"
            );
        }
    }
}
