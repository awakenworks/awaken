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

use crate::RunError;

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tenure: Option<TaskTenureRead>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrivateTaskInputWire {
    version: u32,
    observations: Vec<Value>,
    #[serde(default)]
    tenure: Option<TaskTenureRead>,
}

/// Why a private Task input was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PrivateTaskInputError {
    #[error("unsupported private Task input version {0}")]
    Version(u32),
    #[error("private Task input carries no observation")]
    Empty,
    #[error("private Task input tenure is invalid: {0}")]
    Tenure(String),
}

impl TryFrom<PrivateTaskInputWire> for PrivateTaskInput {
    type Error = PrivateTaskInputError;

    fn try_from(wire: PrivateTaskInputWire) -> Result<Self, Self::Error> {
        if wire.version != PRIVATE_TASK_INPUT_VERSION {
            return Err(PrivateTaskInputError::Version(wire.version));
        }
        match wire.tenure {
            Some(tenure) => Self::with_tenure(tenure, wire.observations),
            None => Self::new(wire.observations),
        }
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
            tenure: None,
        })
    }

    /// Bind the complete authenticated Task subject into the ordinary command
    /// fingerprint. An explicitly addressed input-required attention may carry
    /// no observation; the ordinary unbound `new(empty)` remains forbidden.
    pub fn with_tenure(
        tenure: TaskTenureRead,
        observations: Vec<Value>,
    ) -> Result<Self, PrivateTaskInputError> {
        tenure.validate().map_err(PrivateTaskInputError::Tenure)?;
        Ok(Self {
            version: PRIVATE_TASK_INPUT_VERSION,
            observations,
            tenure: Some(tenure),
        })
    }

    #[must_use]
    pub fn tenure(&self) -> Option<&TaskTenureRead> {
        self.tenure.as_ref()
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
    /// The exact Task, logical Thread, Session partition, origin Run and
    /// attempt held by the authenticated sender. Required even without input.
    pub tenure: TaskTenureRead,
    /// The model-visible notice the background Run starts from.
    pub message: Message,
    /// The observation, carried privately to the Run that folds it. `None`
    /// when the attention only wakes the Thread (input required) and the slot
    /// holds no new candidate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<PrivateTaskInput>,
}

impl TaskAttention {
    /// One canonical lowering into immutable private command input. Authority is
    /// checked by the authenticated boundary; this method alone grants nothing.
    pub fn bind_input(mut self) -> Result<Self, PrivateTaskInputError> {
        if self
            .input
            .as_ref()
            .and_then(PrivateTaskInput::tenure)
            .is_some_and(|tenure| tenure != &self.tenure)
        {
            return Err(PrivateTaskInputError::Tenure(
                "attention and input name different Tasks".into(),
            ));
        }
        let observations = self
            .input
            .take()
            .map_or_else(Vec::new, |input| input.observations);
        self.input = Some(PrivateTaskInput::with_tenure(
            self.tenure.clone(),
            observations,
        )?);
        Ok(self)
    }
}

/// The application durably admitted the Run carrying the observation.
/// This receipt does not certify the complete Task aggregate or grant tool
/// execution; those remain the Task owner's StepStart/admission checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskAttentionAck {
    pub run_id: RunId,
}

/// The registered Worker route that reads the committed slot of a Task the
/// authenticated incarnation holds.
pub const WORKER_TASK_TENURE_PATH: &str = "/v1/worker/task/tenure";

/// The one admitted Task tenure record layout.
pub const TASK_TENURE_RECORD_VERSION: u32 = 1;

/// Which Task's committed slot a holder reads: the Task, the Thread whose
/// State holds it, the Session partition that Thread is committed in, a
/// committed Run of that Thread (the Task's origin Run) that addresses the
/// committed read, and the holder the reader claims. No claim exists when the
/// read happens and the holder names only the incarnation, so the partition
/// travels with the read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskTenureRead {
    pub thread: String,
    pub partition: TaskReadPartition,
    pub run: String,
    pub task: String,
    pub holder: String,
    pub epoch: awaken_lease_contract::fence::Epoch,
}

impl TaskTenureRead {
    pub fn validate(&self) -> Result<(), String> {
        if [&self.thread, &self.run, &self.task, &self.holder]
            .iter()
            .any(|value| value.trim().is_empty())
        {
            return Err("Task tenure coordinates must not be empty".into());
        }
        if let TaskReadPartition::Session { session_thread } = &self.partition
            && (session_thread.trim().is_empty() || session_thread == &self.thread)
        {
            return Err("a child Task must name a distinct nonempty Session partition".into());
        }
        Ok(())
    }

    #[must_use]
    pub fn session_thread(&self) -> &str {
        match &self.partition {
            TaskReadPartition::Thread {} => &self.thread,
            TaskReadPartition::Session { session_thread } => session_thread,
        }
    }
}

/// The Session partition a Task's Thread is committed in. It is required on
/// the wire: a read without it is refused, never taken as a root Thread.
///
/// Decision table (partition → committed read on the Coordinator):
/// | Rule | partition | read |
/// |---|---|---|
/// | P1 | `thread` | the Thread is its own partition (a root Thread or a Thread with no Session) |
/// | P2 | `session` naming another Thread | the child Thread inside that Session's partition |
/// | P3 | `session` naming the Thread itself | refused: a root Thread is `thread` |
/// | P4 | the origin Run is not committed in the addressed partition | refused, never read as an empty slot |
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskReadPartition {
    Thread {},
    Session { session_thread: String },
}

impl TaskReadPartition {
    /// The partition of `thread` when the reading Session is `session_thread`
    /// (P1 for the Session's own Thread, P2 for a child Thread).
    #[must_use]
    pub fn of(thread: &str, session_thread: &str) -> Self {
        if thread == session_thread {
            Self::Thread {}
        } else {
            Self::Session {
                session_thread: session_thread.to_owned(),
            }
        }
    }
}

/// The committed slot of a Task whose current attempt names the requesting
/// holder. The slot is the Task owner's committed value; its codec decodes it.
///
/// Decision table (causes → effect) for decoding a response record:
/// | Rule | cause | effect |
/// |---|---|---|
/// | V1 | `version` is 1, a `slot` and no other field | the record |
/// | V2 | `version` missing, not an integer, or another number | refused |
/// | V3 | an unknown field, or no `slot` | refused |
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskTenureRecord {
    version: TaskTenureRecordVersion,
    slot: Value,
}

/// The record layout version: only [`TASK_TENURE_RECORD_VERSION`] decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
struct TaskTenureRecordVersion;

/// Why a Task tenure record was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskTenureRecordError {
    #[error("unsupported Task tenure record version {0}")]
    Version(u32),
}

impl TryFrom<u32> for TaskTenureRecordVersion {
    type Error = TaskTenureRecordError;

    fn try_from(version: u32) -> Result<Self, Self::Error> {
        if version == TASK_TENURE_RECORD_VERSION {
            Ok(Self)
        } else {
            Err(TaskTenureRecordError::Version(version))
        }
    }
}

impl From<TaskTenureRecordVersion> for u32 {
    fn from(_: TaskTenureRecordVersion) -> Self {
        TASK_TENURE_RECORD_VERSION
    }
}

impl TaskTenureRecord {
    /// Carry one committed Task slot at the current record version.
    #[must_use]
    pub fn new(slot: Value) -> Self {
        Self {
            version: TaskTenureRecordVersion,
            slot,
        }
    }

    /// The committed slot, for the Task owner's codec.
    #[must_use]
    pub fn slot(&self) -> &Value {
        &self.slot
    }
}

/// A registered Worker incarnation's Task control over its authenticated
/// transport: the holder it tenures Tasks to, delivery of the observations it
/// holds, and the holder-bound read of a held Task's committed slot.
///
/// The port carries the holder it is authenticated as, so the holder that
/// tenures a Task, the identity that delivers its attention and the identity
/// that reads its tenure cannot belong to different incarnations.
///
/// Decision table (delivery result → sender):
/// | Rule | result | sender |
/// |---|---|---|
/// | K1 | `Ok(ack)` | the carrying Run is durably admitted; stop |
/// | K2 | any retry | resend the same attention: same operation id, same Run |
/// | K4 | `BadRequest` | definitive; stop; the Task lease law takes over |
/// | K5/K6 | any other error, including an undecodable acknowledgement | retry with the same attention |
///
/// Decision table (tenure read → ownership check):
/// | Rule | result | effect |
/// |---|---|---|
/// | H2 | `Ok(Some(record))` | the committed slot; the caller applies its admission |
/// | H3 | `Ok(None)` | no committed attempt names this holder: the attempt is lost |
/// | H4/H5 | `Err` | the attempt cannot prove it is current; the effect does not run |
#[async_trait::async_trait]
pub trait RegisteredTaskControl: Send + Sync {
    /// The [`registered_task_holder`] of the authenticated incarnation.
    fn holder(&self) -> &str;

    /// Deliver one observation this incarnation holds.
    async fn deliver(&self, attention: TaskAttention) -> Result<TaskAttentionAck, RunError>;

    /// Read the committed slot of a Task this incarnation holds.
    async fn read_tenure(&self, read: TaskTenureRead)
    -> Result<Option<TaskTenureRecord>, RunError>;
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
    fn meta_only_attention_requires_exact_nonzero_task_coordinates() {
        // I1 original new(empty) remains forbidden; I2 explicit complete tenure
        // permits an input-required attention and round-trips v1; I3 every
        // subject axis is serialized and a conflicting prebound input refuses.
        let tenure = TaskTenureRead {
            thread: "root".into(),
            partition: TaskReadPartition::Thread {},
            run: "origin".into(),
            task: "task".into(),
            holder: "worker".into(),
            epoch: awaken_lease_contract::fence::Epoch::from_persisted(2).unwrap(),
        };
        assert_eq!(
            PrivateTaskInput::new(Vec::new()),
            Err(PrivateTaskInputError::Empty),
            "I1"
        );
        let input = PrivateTaskInput::with_tenure(tenure.clone(), Vec::new()).unwrap();
        let wire = serde_json::to_value(&input).unwrap();
        assert_eq!(wire["tenure"]["epoch"], json!(2));
        assert_eq!(wire["tenure"]["task"], json!("task"));
        assert_eq!(
            serde_json::from_value::<PrivateTaskInput>(wire.clone()).unwrap(),
            input,
            "I2"
        );
        for field in ["thread", "run", "task", "holder"] {
            let mut invalid = wire.clone();
            invalid["tenure"][field] = json!("");
            assert!(
                serde_json::from_value::<PrivateTaskInput>(invalid).is_err(),
                "I3 {field}"
            );
        }
        let mut zero = wire;
        zero["tenure"]["epoch"] = json!(0);
        assert!(
            serde_json::from_value::<PrivateTaskInput>(zero).is_err(),
            "I3 zero epoch"
        );
    }

    #[test]
    fn task_tenure_records_admit_only_their_current_version() {
        // V1 round trip; V2 every other version; V3 unknown field or no slot.
        let record = TaskTenureRecord::new(json!({"id": "task-1"}));
        let wire = serde_json::to_value(&record).unwrap();
        assert_eq!(wire, json!({"version": 1, "slot": {"id": "task-1"}}));
        assert_eq!(
            serde_json::from_value::<TaskTenureRecord>(wire.clone()).unwrap(),
            record
        );
        for version in [json!(0), json!(2), json!("1"), json!(1.5), Value::Null] {
            let mut changed = wire.clone();
            changed["version"] = version;
            assert!(
                serde_json::from_value::<TaskTenureRecord>(changed).is_err(),
                "V2"
            );
        }
        let mut missing = wire.clone();
        missing.as_object_mut().unwrap().remove("version");
        assert!(
            serde_json::from_value::<TaskTenureRecord>(missing).is_err(),
            "V2"
        );
        let mut unknown = wire.clone();
        unknown["holder"] = json!("holder");
        assert!(
            serde_json::from_value::<TaskTenureRecord>(unknown).is_err(),
            "V3"
        );
        assert!(
            serde_json::from_value::<TaskTenureRecord>(json!({"version": 1})).is_err(),
            "V3"
        );
        // A bare slot (no record envelope) is not a record.
        assert!(serde_json::from_value::<TaskTenureRecord>(json!({"id": "task-1"})).is_err());
        // The read admits only its current fields.
        let read = json!({
            "thread": "t", "partition": {"kind": "thread"}, "run": "r", "task": "k", "holder": "h", "epoch":3
        });
        assert!(serde_json::from_value::<TaskTenureRead>(read.clone()).is_ok());
        let mut extra = read.clone();
        extra["legacy_epoch"] = json!(1);
        assert!(serde_json::from_value::<TaskTenureRead>(extra).is_err());
        // A read from before the partition existed is refused, not read as a
        // root Thread; so is an unknown partition or a partition field.
        let mut no_epoch = read.clone();
        no_epoch.as_object_mut().unwrap().remove("epoch");
        assert!(
            serde_json::from_value::<TaskTenureRead>(no_epoch).is_err(),
            "no default Task epoch"
        );
        let mut historical = read.clone();
        historical.as_object_mut().unwrap().remove("partition");
        assert!(serde_json::from_value::<TaskTenureRead>(historical).is_err());
        for partition in [
            json!({"kind": "root"}),
            json!({"kind": "thread", "session_thread": "s"}),
            json!({"kind": "session"}),
            json!({"kind": "session", "session_thread": "s", "depth": 1}),
            json!("thread"),
            serde_json::Value::Null,
        ] {
            let mut changed = read.clone();
            changed["partition"] = partition.clone();
            assert!(
                serde_json::from_value::<TaskTenureRead>(changed).is_err(),
                "{partition}"
            );
        }
        let mut child = read;
        child["partition"] = json!({"kind": "session", "session_thread": "s"});
        assert_eq!(
            serde_json::from_value::<TaskTenureRead>(child)
                .unwrap()
                .partition,
            TaskReadPartition::Session {
                session_thread: "s".into()
            }
        );
    }

    #[test]
    fn a_task_partition_is_its_own_thread_or_the_session_it_is_a_child_of() {
        assert_eq!(
            TaskReadPartition::of("t", "t"),
            TaskReadPartition::Thread {},
            "P1"
        );
        assert_eq!(
            TaskReadPartition::of("child", "session"),
            TaskReadPartition::Session {
                session_thread: "session".into()
            },
            "P2"
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
